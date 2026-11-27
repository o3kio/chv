use axum::{extract::State, response::Json};
use serde_json::{json, Value};

use crate::auth::BearerToken;
use crate::router::AppState;
use crate::BffError;

#[derive(sqlx::FromRow)]
struct SnapshotRow {
    snapshot_id: String,
    vm_id: String,
    name: String,
    description: String,
    size_bytes: i64,
    includes_memory: i64,
    snapshot_path: String,
    status: String,
    created_at: String,
}

/// How a failed snapshot dispatch leaves the pre-inserted `creating` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotDispatchFailure {
    /// The control plane definitively refused the request (not found, invalid
    /// request, state conflict, auth). It never accepted the mutation, so no
    /// artifact can be produced at `snapshot_path`; the row can be deleted.
    DefiniteRejection,
    /// Transport-level ambiguity (`BffError::Internal` and any unexpected
    /// error): the control plane may have accepted the dispatch before the
    /// failure surfaced (for example a timeout after accept). The agent may
    /// still materialize the artifact, so the row is kept — marked `failed` —
    /// as the only discoverable pointer to that path for cleanup.
    Ambiguous,
}

fn classify_snapshot_dispatch_failure(error: &BffError) -> SnapshotDispatchFailure {
    match error {
        BffError::NotFound(_)
        | BffError::BadRequest(_)
        | BffError::Conflict(_)
        | BffError::Unauthorized(_)
        | BffError::Forbidden(_) => SnapshotDispatchFailure::DefiniteRejection,
        _ => SnapshotDispatchFailure::Ambiguous,
    }
}

/// Apply the rollback policy to the `creating` row after dispatch failed.
///
/// `vm_snapshots` has no `error_message` column, so the failure detail is only
/// logged server-side; `status = 'failed'` is the durable marker that the
/// attempt did not complete while the row (and its `snapshot_path`) remains
/// discoverable for cleanup.
async fn reconcile_failed_snapshot_dispatch(
    pool: &sqlx::SqlitePool,
    snapshot_id: &str,
    failure: SnapshotDispatchFailure,
) -> Result<(), sqlx::Error> {
    match failure {
        SnapshotDispatchFailure::DefiniteRejection => {
            sqlx::query("DELETE FROM vm_snapshots WHERE snapshot_id = ?")
                .bind(snapshot_id)
                .execute(pool)
                .await?;
        }
        SnapshotDispatchFailure::Ambiguous => {
            sqlx::query("UPDATE vm_snapshots SET status = 'failed' WHERE snapshot_id = ?")
                .bind(snapshot_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

pub async fn list_vm_snapshots(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing vm_id".into()))?;

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    super::vms::require_vm_owner(&mut conn, vm_id, &claims.sub, claims.role == "admin").await?;

    let rows = sqlx::query_as::<_, SnapshotRow>(
        r#"
        SELECT snapshot_id, vm_id, name, description, size_bytes,
               includes_memory, snapshot_path, status, created_at
        FROM vm_snapshots
        WHERE vm_id = ?
        ORDER BY created_at DESC
        "#,
    )
    .bind(vm_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to list snapshots: {}", e)))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.snapshot_id,
                "snapshot_id": r.snapshot_id,
                "vm_id": r.vm_id,
                "name": r.name,
                "description": r.description,
                "size_bytes": r.size_bytes,
                "includes_memory": r.includes_memory != 0,
                "snapshot_path": r.snapshot_path,
                "status": r.status,
                "created_at": r.created_at,
            })
        })
        .collect();

    Ok(Json(json!({ "items": items })))
}

pub async fn create_snapshot(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let vm_id = payload
        .get("vm_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing vm_id".into()))?
        .to_string();

    if !chv_common::validate_id(&vm_id) {
        return Err(BffError::BadRequest("invalid vm_id format".into()));
    }

    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing name".into()))?
        .to_string();

    let description = payload
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let includes_memory = payload
        .get("includes_memory")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Verify the VM exists and the caller owns it
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    super::vms::require_vm_owner(&mut conn, &vm_id, &claims.sub, claims.role == "admin").await?;

    // Check VM power state for quiescence advisory
    let (runtime_status, health_status): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT runtime_status, health_status FROM vm_observed_state WHERE vm_id = ?",
    )
    .bind(&vm_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to get vm power state: {}", e)))?
    .unwrap_or((None, None));

    // Core-managed agents report desired state as runtime_status with
    // health "Unknown" (observed power state not reported yet): treat an
    // unknown state as conservatively-running for the advisory.
    let vm_is_running = health_status.as_deref() == Some("Unknown")
        || runtime_status
            .as_deref()
            .is_some_and(|s| matches!(s, "Running" | "Starting" | "Resuming"));

    let snapshot_id = chv_common::gen_resource_id();
    let snapshot_path = state
        .agent_runtime_dir
        .join("vms")
        .join(&vm_id)
        .join("snapshots")
        .join(&snapshot_id);
    let snapshot_path = snapshot_path.to_string_lossy().to_string();

    sqlx::query(
        r#"
        INSERT INTO vm_snapshots
            (snapshot_id, vm_id, name, description, includes_memory, snapshot_path, status)
        VALUES (?, ?, ?, ?, ?, ?, 'creating')
        "#,
    )
    .bind(&snapshot_id)
    .bind(&vm_id)
    .bind(&name)
    .bind(&description)
    .bind(includes_memory as i64)
    .bind(&snapshot_path)
    .execute(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to insert snapshot: {}", e)))?;

    // Dispatch snapshot operation through control plane. A failed dispatch must
    // not leave a permanent `creating` zombie (nothing reaps it), but the
    // rollback differs by failure class: only a definite rejection proves the
    // control plane never accepted the mutation. On an ambiguous transport
    // failure (timeout after accept) the agent may still materialize the
    // artifact, so keep the row — marked `failed` — as the discoverable pointer
    // to `snapshot_path` for cleanup.
    let response = match state
        .mutations
        .snapshot_vm(vm_id.clone(), snapshot_path.clone(), claims.sub.clone())
        .await
    {
        Ok(response) => response,
        Err(e) => {
            let failure = classify_snapshot_dispatch_failure(&e);
            if let Err(cleanup) =
                reconcile_failed_snapshot_dispatch(&state.pool, &snapshot_id, failure).await
            {
                tracing::warn!(
                    snapshot_id = %snapshot_id,
                    failure = ?failure,
                    error = %cleanup,
                    "failed to reconcile snapshot row after dispatch failure"
                );
            }
            return Err(BffError::Internal(format!(
                "failed to dispatch snapshot: {:?}",
                e
            )));
        }
    };

    // Audit only accepted dispatches. A definite rejection never mutated
    // anything, so it gets no audit entry. An ambiguous failure may have been
    // accepted/materialized; its kept `failed` row is the discoverable record
    // of that attempt instead of an audit entry. (Delete is the mirror image:
    // it audits after removing the artifact but before deleting the row,
    // because that removal is a real mutation that must be recorded even if
    // the row delete then fails.)
    crate::audit::log_mutation(&claims.sub, "create_snapshot", "vm", &vm_id);

    tracing::info!(
        snapshot_id = %snapshot_id,
        vm_id = %vm_id,
        task_id = %response.task_id,
        "snapshot dispatched"
    );
    state.cache.invalidate("vms:").await;
    state.cache.invalidate("overview").await;

    let warning = if vm_is_running && !includes_memory {
        Some("VM is running; snapshot may not be crash-consistent. Consider pausing the VM or including memory for a consistent snapshot.")
    } else {
        None
    };

    Ok(Json(json!({
        "accepted": true,
        "task_id": response.task_id,
        "snapshot_id": snapshot_id,
        "vm_id": vm_id,
        "summary": format!("Snapshot '{}' creation accepted for VM {}", name, vm_id),
        "next_refresh_path": format!("/v1/tasks/{}", response.task_id),
        "warning": warning,
    })))
}

/// Validate and resolve a snapshot artifact path.
///
/// The path is produced by `create_snapshot`, but treat the stored value as
/// untrusted on the way out: refuse `..` components, anything outside the VM's
/// own `snapshots/` directory, the snapshots directory itself, and any
/// symlinked ancestor from the artifact up to the agent runtime dir (including
/// `<root>/vms` and `<root>/vms/<vm_id>`). A corrupt row therefore cannot turn
/// a delete into an arbitrary filesystem removal or redirect it through a
/// symlink. A final-component symlink is allowed here: removal uses
/// `symlink_metadata` + `remove_file`, which unlinks the link itself rather
/// than following it.
///
/// Validation failures are about the STORED row, not the caller's request, so
/// they surface as [`BffError::Internal`] (the client asked to delete a real
/// snapshot; the server's stored path is corrupt).
fn snapshot_artifact_path(
    root: &std::path::Path,
    vm_id: &str,
    snapshot_path: &str,
) -> Result<std::path::PathBuf, BffError> {
    use std::path::Component;

    let path = std::path::PathBuf::from(snapshot_path);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        tracing::warn!(
            snapshot_path = %snapshot_path,
            "refusing to remove snapshot artifact containing '..'"
        );
        return Err(BffError::Internal(
            "stored snapshot artifact path contains '..'".into(),
        ));
    }

    let expected_root = root.join("vms").join(vm_id).join("snapshots");
    if path == expected_root || !path.starts_with(&expected_root) {
        tracing::warn!(
            snapshot_path = %snapshot_path,
            root = %expected_root.display(),
            "refusing to remove snapshot artifact outside the VM snapshot directory"
        );
        return Err(BffError::Internal(
            "stored snapshot artifact path is outside the VM snapshot directory".into(),
        ));
    }

    // Lexical confinement is not enough: an attacker-controlled symlink at an
    // ancestor (inside the snapshots tree, or the `<vm_id>` component above it)
    // would make `remove_dir_all`/`remove_file` operate outside the intended
    // tree. Walk every ancestor up to (but not including) the trusted agent
    // runtime dir and reject symlinked components. A missing ancestor is fine
    // (the artifact cannot exist through it).
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                tracing::warn!(
                    snapshot_path = %snapshot_path,
                    symlink = %dir.display(),
                    "refusing to remove snapshot artifact through a symlinked directory"
                );
                return Err(BffError::Internal(
                    "stored snapshot artifact path traverses a symlinked directory".into(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(BffError::Internal(format!(
                    "failed to inspect snapshot artifact ancestor: {e}"
                )))
            }
        }
        current = dir.parent();
    }

    Ok(path)
}

/// Remove a validated artifact path. A missing artifact is success (an earlier
/// create may have failed before materializing it). A final-component symlink
/// is unlinked, never followed.
async fn remove_snapshot_artifact_path(path: &std::path::Path) -> Result<(), BffError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_dir() => tokio::fs::remove_dir_all(path)
            .await
            .map_err(|e| BffError::Internal(format!("failed to remove snapshot directory: {e}"))),
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|e| BffError::Internal(format!("failed to remove snapshot file: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(BffError::Internal(format!(
            "failed to inspect snapshot artifact: {e}"
        ))),
    }
}

/// Remove a snapshot's backing artifact before its row is deleted.
///
/// A refused/unsafe path is an error, not a silent success: the row is kept so
/// the leak is visible and retryable instead of leaving an orphan artifact
/// forever. A missing artifact remains success.
async fn remove_snapshot_artifact(
    state: &AppState,
    vm_id: &str,
    snapshot_path: &str,
) -> Result<(), BffError> {
    if snapshot_path.is_empty() {
        return Ok(());
    }
    let path = snapshot_artifact_path(&state.agent_runtime_dir, vm_id, snapshot_path)?;
    remove_snapshot_artifact_path(&path).await
}

pub async fn delete_snapshot(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let snapshot_id = payload
        .get("snapshot_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing snapshot_id".into()))?
        .to_string();

    // Fetch the snapshot to get its vm_id and verify it exists before deleting
    let snapshot = sqlx::query_as::<_, SnapshotRow>(
        "SELECT snapshot_id, vm_id, name, description, size_bytes, \
         includes_memory, snapshot_path, status, created_at \
         FROM vm_snapshots WHERE snapshot_id = ?",
    )
    .bind(&snapshot_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to look up snapshot: {}", e)))?
    .ok_or_else(|| BffError::NotFound(format!("snapshot {} not found", snapshot_id)))?;

    // Verify the user owns the VM this snapshot belongs to
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    super::vms::require_vm_owner(
        &mut conn,
        &snapshot.vm_id,
        &claims.sub,
        claims.role == "admin",
    )
    .await?;

    // Remove the backing artifact before the row that points at it. If the
    // removal fails, keep the row so a retry is possible; deleting the row
    // first would orphan the artifact with no way to find it again.
    remove_snapshot_artifact(&state, &snapshot.vm_id, &snapshot.snapshot_path).await?;

    crate::audit::log_mutation(&claims.sub, "delete_snapshot", "snapshot", &snapshot_id);

    sqlx::query("DELETE FROM vm_snapshots WHERE snapshot_id = ?")
        .bind(&snapshot_id)
        .execute(&state.pool)
        .await
        .map_err(|e| BffError::Internal(format!("failed to delete snapshot: {}", e)))?;
    state.cache.invalidate("vms:").await;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "accepted": true,
        "snapshot_id": snapshot_id,
        "summary": format!("Snapshot {} deleted", snapshot_id),
    })))
}

pub async fn restore_snapshot(
    crate::auth::BearerToken(claims): crate::auth::BearerToken,
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<Value>,
) -> Result<Json<Value>, BffError> {
    crate::auth::require_operator_or_admin(&claims)?;
    let snapshot_id = payload
        .get("snapshot_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BffError::BadRequest("missing snapshot_id".into()))?
        .to_string();

    let row = sqlx::query_as::<_, SnapshotRow>(
        r#"
        SELECT snapshot_id, vm_id, name, description, size_bytes,
               includes_memory, snapshot_path, status, created_at
        FROM vm_snapshots
        WHERE snapshot_id = ?
        "#,
    )
    .bind(&snapshot_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to look up snapshot: {}", e)))?;

    let snapshot =
        row.ok_or_else(|| BffError::NotFound(format!("snapshot {} not found", snapshot_id)))?;

    // Verify the caller owns the VM this snapshot belongs to
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| BffError::Internal(format!("db connection: {e}")))?;
    super::vms::require_vm_owner(
        &mut conn,
        &snapshot.vm_id,
        &claims.sub,
        claims.role == "admin",
    )
    .await?;

    // Check VM power state — must be stopped before restoring a snapshot
    let (runtime_status, health_status): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT runtime_status, health_status FROM vm_observed_state WHERE vm_id = ?",
    )
    .bind(&snapshot.vm_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| BffError::Internal(format!("failed to get vm power state: {}", e)))?
    .unwrap_or((None, None));

    // Fail closed on unknown observed state: core-managed agents report
    // desired state as runtime_status with health "Unknown", and a restore
    // must never proceed on an unverified power state (a desired-Stopped
    // fragment while the guest genuinely runs would otherwise pass).
    if health_status.as_deref() == Some("Unknown") {
        return Err(BffError::BadRequest(
            "VM observed power state is unknown (core-managed); verify the VM is stopped before restoring a snapshot".into(),
        ));
    }

    let is_running = runtime_status
        .as_deref()
        .map(|s| matches!(s, "Running" | "Starting" | "Resuming"))
        .unwrap_or(false);

    if is_running {
        return Err(BffError::BadRequest(
            "VM must be stopped before restoring a snapshot".into(),
        ));
    }

    crate::audit::log_mutation(&claims.sub, "restore_snapshot", "snapshot", &snapshot_id);

    // Dispatch restore operation through control plane
    let response = state
        .mutations
        .restore_snapshot(
            snapshot.vm_id.clone(),
            snapshot.snapshot_path.clone(),
            claims.sub.clone(),
        )
        .await
        .map_err(|e| BffError::Internal(format!("failed to dispatch restore: {:?}", e)))?;

    tracing::info!(
        snapshot_id = %snapshot_id,
        vm_id = %snapshot.vm_id,
        task_id = %response.task_id,
        "restore dispatched"
    );
    state.cache.invalidate("vms:").await;
    state.cache.invalidate("overview").await;

    Ok(Json(json!({
        "accepted": true,
        "task_id": response.task_id,
        "snapshot_id": snapshot_id,
        "vm_id": snapshot.vm_id,
        "summary": format!("Snapshot restore accepted for VM {}", snapshot.vm_id),
        "next_refresh_path": format!("/v1/tasks/{}", response.task_id),
    })))
}

#[cfg(test)]
mod tests {
    use super::{
        classify_snapshot_dispatch_failure, reconcile_failed_snapshot_dispatch,
        remove_snapshot_artifact_path, snapshot_artifact_path, SnapshotDispatchFailure,
    };
    use crate::BffError;
    use std::path::Path;

    fn snapshots_root(root: &Path, vm_id: &str) -> std::path::PathBuf {
        root.join("vms").join(vm_id).join("snapshots")
    }

    fn assert_internal(result: Result<std::path::PathBuf, BffError>, context: &str) {
        match result {
            Err(BffError::Internal(_)) => {}
            other => panic!("{context} must be a server-side corrupt-row error, got {other:?}"),
        }
    }

    #[test]
    fn artifact_path_rejects_parent_dir_components() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let err = snapshot_artifact_path(root, "vm-1", "vms/vm-1/snapshots/../../escape")
            .expect_err("'..' must be rejected");
        assert!(
            format!("{err:?}").contains(".."),
            "unexpected error: {err:?}"
        );
        assert!(
            matches!(err, BffError::Internal(_)),
            "a corrupt stored path is a server error, not a client 400: {err:?}"
        );
    }

    #[test]
    fn artifact_path_rejects_paths_outside_the_vm_snapshot_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let other_vm = snapshots_root(root, "vm-2").join("snap-1");
        assert_internal(
            snapshot_artifact_path(root, "vm-1", &other_vm.to_string_lossy()),
            "another VM's snapshot tree",
        );

        let outside = root.join("elsewhere").join("snap-1");
        assert_internal(
            snapshot_artifact_path(root, "vm-1", &outside.to_string_lossy()),
            "a path outside the runtime dir",
        );

        let tree_root = snapshots_root(root, "vm-1");
        assert_internal(
            snapshot_artifact_path(root, "vm-1", &tree_root.to_string_lossy()),
            "the snapshots directory itself",
        );

        assert_internal(
            snapshot_artifact_path(root, "vm-1", "relative/snap-1"),
            "a relative path",
        );
    }

    #[test]
    fn artifact_path_accepts_a_valid_snapshot_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let valid = snapshots_root(root, "vm-1").join("snap-1");
        assert_eq!(
            snapshot_artifact_path(root, "vm-1", &valid.to_string_lossy()).unwrap(),
            valid
        );
    }

    #[test]
    fn artifact_path_rejects_a_symlinked_intermediate_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inside_elsewhere = root.join("outside-target");
        std::fs::create_dir_all(&inside_elsewhere).unwrap();

        let snapshots = snapshots_root(root, "vm-1");
        std::fs::create_dir_all(&snapshots).unwrap();
        let link = snapshots.join("linked-dir");
        std::os::unix::fs::symlink(&inside_elsewhere, &link).unwrap();

        let through_link = link.join("snap-1");
        let err = snapshot_artifact_path(root, "vm-1", &through_link.to_string_lossy())
            .expect_err("a symlinked intermediate directory must be rejected");
        assert!(
            format!("{err:?}").contains("symlink"),
            "unexpected error: {err:?}"
        );
        assert!(
            matches!(err, BffError::Internal(_)),
            "a symlinked stored path is a server error, not a client 400: {err:?}"
        );
    }

    /// A symlink ABOVE the snapshots directory (`<root>/vms/<vm_id>`) is just
    /// as dangerous as one inside it: `remove_dir_all` on the artifact would
    /// operate through the link. The walk must cover the whole ancestor chain
    /// up to the agent runtime dir, not stop at `<root>/vms/<vm_id>/snapshots`.
    #[test]
    fn artifact_path_rejects_a_symlinked_vm_id_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("vms")).unwrap();

        // `<root>/vms/vm-1` is a symlink to an attacker-controlled directory
        // containing a real `snapshots/` subtree.
        let outside_vm = root.join("outside-vm");
        std::fs::create_dir_all(outside_vm.join("snapshots")).unwrap();
        std::os::unix::fs::symlink(&outside_vm, root.join("vms").join("vm-1")).unwrap();

        let through_vm_link = snapshots_root(root, "vm-1").join("snap-1");
        let err = snapshot_artifact_path(root, "vm-1", &through_vm_link.to_string_lossy())
            .expect_err("a symlinked <vm_id> ancestor must be rejected");
        assert!(
            format!("{err:?}").contains("symlink"),
            "unexpected error: {err:?}"
        );
        assert!(
            matches!(err, BffError::Internal(_)),
            "a symlinked stored path is a server error, not a client 400: {err:?}"
        );
    }

    #[tokio::test]
    async fn removes_a_file_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshots = snapshots_root(tmp.path(), "vm-1");
        std::fs::create_dir_all(&snapshots).unwrap();
        let file = snapshots.join("snap-1");
        std::fs::write(&file, b"data").unwrap();

        remove_snapshot_artifact_path(&file).await.unwrap();
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn removes_a_directory_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshots = snapshots_root(tmp.path(), "vm-1");
        let dir = snapshots.join("snap-1");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested").join("state"), b"data").unwrap();

        remove_snapshot_artifact_path(&dir).await.unwrap();
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn missing_artifact_is_success() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = snapshots_root(tmp.path(), "vm-1").join("never-created");
        remove_snapshot_artifact_path(&missing).await.unwrap();
    }

    #[tokio::test]
    async fn final_component_symlink_is_unlinked_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshots = snapshots_root(tmp.path(), "vm-1");
        std::fs::create_dir_all(&snapshots).unwrap();

        let target = tmp.path().join("target-file");
        std::fs::write(&target, b"must survive").unwrap();
        let link = snapshots.join("snap-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let validated = snapshot_artifact_path(tmp.path(), "vm-1", &link.to_string_lossy())
            .expect("a final-component symlink is allowed");
        remove_snapshot_artifact_path(&validated).await.unwrap();

        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the symlink itself must be removed"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"must survive",
            "removal must not follow the final symlink"
        );
    }

    #[test]
    fn dispatch_failure_classification_splits_rejection_from_ambiguity() {
        for error in [
            BffError::NotFound("snapshot target missing".into()),
            BffError::BadRequest("invalid request".into()),
            BffError::Conflict("state conflict".into()),
            BffError::Unauthorized("no token".into()),
            BffError::Forbidden("wrong role".into()),
        ] {
            assert_eq!(
                classify_snapshot_dispatch_failure(&error),
                SnapshotDispatchFailure::DefiniteRejection,
                "{error:?} must delete the creating row: the mutation never happened"
            );
        }

        for error in [
            BffError::Internal("dispatch timeout after accept".into()),
            BffError::TooManyRequests("retry later".into()),
        ] {
            assert_eq!(
                classify_snapshot_dispatch_failure(&error),
                SnapshotDispatchFailure::Ambiguous,
                "{error:?} must keep the row: the artifact may have materialized"
            );
        }
    }

    async fn migrated_pool() -> sqlx::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        chv_controlplane_store::run_migrations(&pool, None)
            .await
            .expect("run migrations");
        pool
    }

    async fn seed_creating_snapshot(pool: &sqlx::SqlitePool, snapshot_id: &str) {
        sqlx::query("INSERT INTO vms (vm_id, display_name) VALUES ('vm-1', 'vm 1')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO vm_snapshots \
                 (snapshot_id, vm_id, name, includes_memory, snapshot_path, status) \
             VALUES (?, 'vm-1', 'snap', 0, ?, 'creating')",
        )
        .bind(snapshot_id)
        .bind(format!("/var/lib/chv/vms/vm-1/snapshots/{snapshot_id}"))
        .execute(pool)
        .await
        .unwrap();
    }

    /// A definite rejection means the control plane never accepted the
    /// mutation, so no artifact exists and the zombie `creating` row must go.
    #[tokio::test]
    async fn definite_rejection_removes_the_creating_row() {
        let pool = migrated_pool().await;
        seed_creating_snapshot(&pool, "snap-definite").await;

        reconcile_failed_snapshot_dispatch(
            &pool,
            "snap-definite",
            SnapshotDispatchFailure::DefiniteRejection,
        )
        .await
        .unwrap();

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM vm_snapshots WHERE snapshot_id = 'snap-definite'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 0, "a definite rejection must delete the row");
    }

    /// An ambiguous (transport) failure may have been accepted by the control
    /// plane; the row is kept as `failed` so its `snapshot_path` stays
    /// discoverable for cleanup, but it is no longer an in-flight zombie.
    #[tokio::test]
    async fn ambiguous_dispatch_failure_keeps_the_row_as_failed() {
        let pool = migrated_pool().await;
        seed_creating_snapshot(&pool, "snap-ambiguous").await;

        reconcile_failed_snapshot_dispatch(
            &pool,
            "snap-ambiguous",
            SnapshotDispatchFailure::Ambiguous,
        )
        .await
        .unwrap();

        let (status, path): (String, String) = sqlx::query_as(
            "SELECT status, snapshot_path FROM vm_snapshots WHERE snapshot_id = 'snap-ambiguous'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            status, "failed",
            "an ambiguous failure must stop the row looking in-flight"
        );
        assert_eq!(
            path, "/var/lib/chv/vms/vm-1/snapshots/snap-ambiguous",
            "the artifact path must survive for discovery/cleanup"
        );
    }
}
