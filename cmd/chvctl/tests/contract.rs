//! #372 contract-test harness — chvctl's real request path against the
//! real BFF router (design §4 Option B, adopted 2026-10-06 §5.1 DP1;
//! PR 1 of the #372 decomposition).
//!
//! Root cause this harness kills (issue #372): chvctl builds request
//! bodies inline in each command handler and the BFF reads them with
//! ad-hoc `payload.get(...)` extraction — nothing shares or pins the
//! vocabulary, so a renamed field or route on either side keeps
//! compiling and the failure surfaces only at runtime (400 / 404 / a
//! silent drop / a hang). No test anywhere drove chvctl's own
//! request-building code; these rows do.
//!
//! Architecture:
//! - `AppState` is built exactly like the BFF's own integration suite
//!   (in-memory SQLite + `run_migrations` + a recording
//!   `MutationService` stub — the `tests/volume_snapshot_clone.rs`
//!   pattern, same shape in 21 BFF test files).
//! - The server is the control plane's
//!   `admin_router(state, metrics, webui)` (the Web UI stays disabled —
//!   the `[webui]` fail-closed default; these rows drive the API
//!   surface only), NOT the plain `bff_router`: admin_router merges
//!   bff_router and
//!   additionally mounts `/admin/migrations/{id}/cancel`
//!   (`chv-controlplane-service/src/api/router.rs`), the route the
//!   repointed `migrate cancel` row (design §2.3/DP4) needs — a
//!   bff_router-only listener would 404 that row even post-fix. It
//!   reuses the same `AppState`, `admin_middleware` accepts the seeded
//!   BFF JWTs, and constructing it needs a `SharedConvergenceMetrics`
//!   (a default `convergence_metrics::new_shared()` instance suffices).
//! - The listener is a real ephemeral TCP socket
//!   (`TcpListener::bind("127.0.0.1:0")` + `axum::serve`) because
//!   chvctl's `BffClient` is a reqwest wrapper — the operator's actual
//!   wire path, including method, path, headers, and body.
//! - Each row invokes `commands::<group>::execute(&client, args,
//!   &format)` — the operator's actual command code.
//!
//! Per-row assertions (design §4 step 4):
//! - the HTTP status is not 404/405 (route + method exist) and not a
//!   `missing <field>` 400 (field names accepted) — implied by the
//!   command returning `Ok(())` for green rows;
//! - `RecordingMutations` received the expected method:args where a
//!   mutation is forwarded;
//! - every display column chvctl prints exists in a response item
//!   (kills the §2.7(a) display-drift class). The column lists below
//!   deliberately duplicate the `print_list` literals in
//!   `cmd/chvctl/src/commands/*.rs` — that duplication IS the pin: if
//!   either chvctl's columns or the BFF's response shape drift, the row
//!   fails.
//!
//! Row taxonomy (design §2 is the authority):
//! - GREEN rows: the command's route, method, field names, mutation
//!   forwarding, and display columns all match the BFF.
//! - PINNED-BROKEN rows: known drift, pinned as it behaves TODAY with a
//!   TODO referencing the design §2 section and the PR that flips the
//!   row. The harness must pass at main — red-where-known means
//!   asserting the current broken behavior, not failing. After PR 4
//!   there are NONE left: every row in this file is green (49 rows),
//!   which is the campaign's terminal state — any future drift fails
//!   the suite outright instead of needing a new pin.
//!
//! PR 2 (the #372 live-path fixes) flipped the fixable red pins green
//! and added the DP9 rows:
//! - `user delete` now sends `user_id` (§2.1/DP2) — green, end-to-end;
//! - `task watch` polls the new `POST /v1/tasks/get` (§2.5/DP6) and the
//!   row asserts COMPLETION of a seeded terminal task;
//! - `network create --vlan` — the flag is removed (§2.6/DP7); the row
//!   is now a CLI-arg-level assertion that `--vlan` no longer parses;
//! - the four display-drift rows (§2.7(a)/DP10) assert the corrected
//!   columns PRESENT;
//! - new DP9 rows pin `vm create --node/--storage-class/
//!   --disk-size-gb/--cloud-init` (green on a local-reporting node, and
//!   the unoffered-class 400 with zero journaled rows).
//!
//! PR 3 (landed, #520 — the #372 dead-group removals) removed the
//! `storage` and `backup` command groups (design §2.2/DP3 and §2.4/DP5):
//! every subcommand 404'd on routes that do not exist, and the removals
//! are CLI-surface only — the BFF's `/v1/backups/*` routes stay (the
//! UI's backup catalog page calls them), and the BackupWorker
//! scaffold is untouched CP machinery. The BFF's `/v1/storage-pools`
//! routes were later removed with the whole phantom catalog (#514:
//! the table was connected to no provisioning, and the UI's storage
//! page had been an unreachable redirect to /volumes since long
//! before #372 (d6836446, 2026-04-17)). Their 6 rows (storage ×4,
//! backup ×2) left this file with the groups; `chvctl storage ...` / `chvctl backup ...` now fail at
//! argument parsing with "unrecognized subcommand" — truthful (design
//! residual risk 2).
//!
//! PR 4 (this change, the campaign's final PR) repointed the `migrate`
//! group (design §2.3/DP4 + DP4b) — the last four red pins, all green
//! now, zero pinned-broken rows remain:
//! - `migrate start` sends the vm-mutate migrate body
//!   (`POST /v1/vms/mutate`, `target_node_id` — the path `vm migrate`
//!   already drove; the old `POST /v1/migrations` route never existed);
//! - `migrate cancel` calls the CP admin-tier
//!   `POST /admin/migrations/{id}/cancel` (admin token; the harness has
//!   served `admin_router` for exactly this row since PR 1);
//! - `migrate status`/`list` read the new viewer-tier
//!   `GET /v1/migrations[/{id}]` routes (DP4b, pinned server-side by
//!   `crates/chv-webui-bff/tests/migrations_read_routes.rs`).
//!
//! The vm-mutate path is consumed unchanged; the admin router is
//! consumed with ONE registration fix this row forced (see the
//! `api/router.rs` comment): the cancel route was spelled
//! `/admin/migrations/{id}/cancel`, and axum 0.7's matchit has no brace
//! path-param syntax — the route registered a literal `{id}` segment
//! and could never match, so the pre-fix row 404'd against the CP's
//! own fallback. Now `:id`; the two new read routes are additive and
//! viewer-tier.
//!
//! #513 PR 3 (the volume-create CLI, DP9 of the adopted design) added
//! the `volume create` rows: a green create on a node whose inventory
//! reports `["local"]` (route, field names, and the journaled row
//! shapes — owner stamping, the bytes-denominated capacity, the
//! trimmed class, the 'data' kind, the standalone Pending desired
//! state, and the Accepted `CreateVolume` operation keyed
//! `create-volume-{volume_id}`), the unoffered-class 400 with zero
//! journaled rows, and the blank-node 400 (the server-side half of
//! the flag's clap-required-ness). The suite goes 39 → 42 rows, still
//! zero pinned-broken — an unpinned new command is the exact #372
//! failure mode DP9 exists to prevent.
//!
//! #502 (terminal-failure cause surfacing) added the
//! `task_watch_failure_cause_row` (42 → 43 rows): the BFF's
//! task-carrying responses gained the journaled
//! `error_code`/`error_message` pair (NULL until a failure records
//! them), `chvctl task watch` prints the cause of a terminally-failed
//! task, and the row pins both halves — the /v1/tasks/get detail
//! carries the seeded cause verbatim, and the watch command exits
//! non-zero on the terminal failure it now explains.
//!
//! #522 PR 3 (the volume-delete CLI, DP11 of the adopted design) added
//! the `volume delete` rows (43 → 46 rows, still zero pinned-broken):
//! a green delete on a seeded standalone `volume_kind = 'data'`
//! volume (the command's `{"volume_id"}` body against POST
//! /v1/volumes/delete, the journaled tombstone — `volume_desired_
//! state` flipped to 'Deleting' with the generation bump and the
//! owner stamped as `updated_by`, the `volumes` row retained (DP1),
//! and the Accepted `DeleteVolume` operation keyed
//! `delete-volume-{volume_id}` — plus the five-key response shape
//! pinned through the #406 idempotent re-post and the failed-task
//! story: the accepted task, flipped to a terminal failure with a
//! recorded cause, surfaces that cause through `task watch` and
//! exits non-zero, the #530 discipline), the attached-volume 400
//! (the loud detach-first message the CLI surfaces verbatim, zero
//! journaling), and the in-flight-operation 409 (the loud message
//! naming the in-flight operation id, zero journaling).
//!
//! #532 (the vm-import CLI) added the `vm import` rows (46 → 49
//! rows, still zero pinned-broken) — the first in-tree client for
//! `POST /v1/vms/import`, and therefore the first chvctl request on
//! the multipart arm of the CSRF gate (#531): a green import
//! end-to-end (the command's multipart request — the `name`/`file`
//! part names and the file content, pinned by the journaled rows and
//! the written disk — plus the non-empty `x-csrf-token` header the
//! gate requires, captured by a harness middleware on the command's
//! own wire path, the #386/#481 ownership stamping, and the response
//! shape pinned through a re-drive of the command's own client
//! method), the CSRF-gate rejection row (the same multipart request
//! WITHOUT the header — and with a blank one — is 403
//! `CSRF_REJECTED` with zero journaling, sent with a fully valid
//! operator JWT so the rejection is provably the CSRF layer), and
//! the non-qcow2 400 (the handler's magic check on the file part's
//! content, surfaced through the command's error path, zero
//! journaling). The harness's `AppState` gained a temp
//! `agent_runtime_dir` for these rows (the import handler writes the
//! uploaded image there; the JSON routes never touch it — the
//! `csrf_multipart_import.rs` pattern).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::Request;
use axum::middleware::{self, Next};
use chv_common::SystemClock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetworkRepository, NodeRepository, NotificationOutboxRepository, ObservedStateRepository,
    OperationRepository, TopologyRepository,
};
use chv_webui_bff::mutations::MutationService;
use chv_webui_bff::{AppState, BffError};
use chvctl::client::{BffClient, CliError};
use chvctl::commands::{auth, health, image, migrate, network, node, task, user, vm, volume};
use chvctl::output::OutputFormat;
use clap::Parser as _;
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// Recording mutation-service stub (the BFF test-suite pattern)
// ---------------------------------------------------------------------------

/// Records every mutation call as `method:arg:arg` for assertions.
#[derive(Default)]
struct RecordingMutations {
    calls: Mutex<Vec<String>>,
}

impl RecordingMutations {
    fn record(&self, entry: String) {
        self.calls.lock().unwrap().push(entry);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Assert the exact `method:args` entry reached the mutation service.
    fn assert_recorded(&self, expected: &str) {
        let calls = self.calls();
        assert!(
            calls.iter().any(|c| c == expected),
            "mutation service did not receive {expected:?}; calls: {calls:?}"
        );
    }
}

fn vm_response(vm_id: &str) -> chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse {
    chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse {
        accepted: true,
        task_id: format!("op-{vm_id}"),
        vm_id: vm_id.to_string(),
        summary: "recorded".to_string(),
    }
}

fn node_response(node_id: &str) -> chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse {
    chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse {
        accepted: true,
        task_id: format!("op-{node_id}"),
        node_id: node_id.to_string(),
        summary: "recorded".to_string(),
    }
}

fn volume_response(volume_id: &str) -> chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse {
    chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse {
        accepted: true,
        task_id: format!("op-{volume_id}"),
        volume_id: volume_id.to_string(),
        summary: "recorded".to_string(),
    }
}

fn network_response(
    network_id: &str,
) -> chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse {
    chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse {
        accepted: true,
        task_id: format!("op-{network_id}"),
        network_id: network_id.to_string(),
        summary: "recorded".to_string(),
    }
}

#[async_trait]
impl MutationService for RecordingMutations {
    async fn mutate_vm(
        &self,
        vm_id: String,
        action: String,
        force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        self.record(format!("mutate_vm:{vm_id}:{action}:{force}"));
        Ok(vm_response(&vm_id))
    }
    async fn migrate_vm(
        &self,
        vm_id: String,
        target_node_id: String,
        pause_first: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        self.record(format!(
            "migrate_vm:{vm_id}:{target_node_id}:{}",
            if pause_first { "pause_first" } else { "live" }
        ));
        Ok(vm_response(&vm_id))
    }
    async fn snapshot_vm(
        &self,
        vm_id: String,
        destination: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        self.record(format!("snapshot_vm:{vm_id}:{destination}"));
        Ok(vm_response(&vm_id))
    }
    async fn restore_snapshot(
        &self,
        vm_id: String,
        source: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVmResponse, BffError> {
        self.record(format!("restore_snapshot:{vm_id}:{source}"));
        Ok(vm_response(&vm_id))
    }
    async fn mutate_node(
        &self,
        node_id: String,
        action: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNodeResponse, BffError> {
        self.record(format!("mutate_node:{node_id}:{action}"));
        Ok(node_response(&node_id))
    }
    async fn mutate_volume(
        &self,
        volume_id: String,
        action: String,
        force: bool,
        _resize_bytes: Option<u64>,
        _vm_id: Option<String>,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!("mutate_volume:{volume_id}:{action}:{force}"));
        Ok(volume_response(&volume_id))
    }
    async fn snapshot_volume(
        &self,
        volume_id: String,
        snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!("snapshot_volume:{volume_id}:{snapshot_name}"));
        Ok(volume_response(&volume_id))
    }
    async fn restore_volume_snapshot(
        &self,
        volume_id: String,
        snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!(
            "restore_volume_snapshot:{volume_id}:{snapshot_name}"
        ));
        Ok(volume_response(&volume_id))
    }
    async fn delete_volume_snapshot(
        &self,
        volume_id: String,
        snapshot_name: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!(
            "delete_volume_snapshot:{volume_id}:{snapshot_name}"
        ));
        Ok(volume_response(&volume_id))
    }
    async fn clone_volume(
        &self,
        source_volume_id: String,
        target_volume_id: String,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateVolumeResponse, BffError> {
        self.record(format!(
            "clone_volume:{source_volume_id}:{target_volume_id}"
        ));
        Ok(volume_response(&target_volume_id))
    }
    async fn mutate_network(
        &self,
        network_id: String,
        action: String,
        force: bool,
        _requested_by: String,
    ) -> Result<chv_webui_bff_api::chv_webui_bff_v1::MutateNetworkResponse, BffError> {
        self.record(format!("mutate_network:{network_id}:{action}:{force}"));
        Ok(network_response(&network_id))
    }
}

// ---------------------------------------------------------------------------
// Harness: AppState + admin_router on a real ephemeral TCP listener
// ---------------------------------------------------------------------------

struct Harness {
    url: String,
    pool: sqlx::SqlitePool,
    jwt_secret: String,
    mutations: Arc<RecordingMutations>,
    /// Number of `POST /v1/tasks/get` requests the server has served —
    /// the `task watch` row's poll-hit assertion (design §2.5/DP6: the
    /// fixed command polls the single-task get route).
    task_polls: Arc<AtomicUsize>,
    /// Every `POST /v1/vms/import` request the server has seen, as
    /// `(content-type, x-csrf-token)` — the `vm import` rows' pin of
    /// the command's own wire shape (#532): the multipart content type
    /// and the NON-EMPTY CSRF marker the gate requires (#531),
    /// captured before the gate itself runs (this middleware is the
    /// outermost layer).
    import_requests: ImportRequests,
    /// The per-harness agent runtime dir the `AppState` was built with
    /// (a tempdir — the import handler writes uploaded images there;
    /// the JSON routes never touch it, so only the `vm import` rows
    /// read this). `_runtime_dir` keeps it alive for the row.
    agent_runtime_dir: std::path::PathBuf,
    _runtime_dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        chv_controlplane_store::run_migrations(&pool, None)
            .await
            .expect("run migrations");

        // A per-harness temp runtime dir (#532, the
        // `csrf_multipart_import.rs` pattern): the vm-import handler
        // writes the uploaded image under `agent_runtime_dir`. The
        // pre-#532 harness hardcoded `/var/lib/chv/agent` — fine for
        // the JSON routes (which never touch the filesystem) but not
        // creatable/writable on an arbitrary CI runner.
        let runtime_dir = tempfile::tempdir().expect("agent runtime tempdir");
        let agent_runtime_dir = runtime_dir.path().to_path_buf();

        let mutations = Arc::new(RecordingMutations::default());
        let state = build_state(pool.clone(), mutations.clone(), agent_runtime_dir.clone());

        // A default/empty convergence-metrics instance suffices for these
        // rows — the harness never exercises convergence paths. The Web
        // UI stays disabled (the [webui] fail-closed default): these
        // rows drive the API surface only.
        let metrics = chv_controlplane_service::convergence_metrics::new_shared();
        let app = chv_controlplane_service::api::router::admin_router(
            state,
            metrics,
            chv_config::WebUiConfig::default(),
        );

        let task_polls = Arc::new(AtomicUsize::new(0));
        let counter = task_polls.clone();
        let import_requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = import_requests.clone();
        let app = app.layer(middleware::from_fn(move |req: Request, next: Next| {
            let counter = counter.clone();
            let recorder = recorder.clone();
            async move {
                if req.uri().path() == "/v1/tasks/get" {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                if req.uri().path() == "/v1/vms/import" {
                    let csrf = req
                        .headers()
                        .get("x-csrf-token")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    let content_type = req
                        .headers()
                        .get(axum::http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    recorder.lock().unwrap().push((content_type, csrf));
                }
                next.run(req).await
            }
        }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral listener");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("contract harness server");
        });

        Self {
            url: format!("http://127.0.0.1:{port}"),
            pool,
            jwt_secret: "test-secret".to_string(),
            mutations,
            task_polls,
            import_requests,
            agent_runtime_dir,
            _runtime_dir: runtime_dir,
        }
    }

    /// The `(content-type, x-csrf-token)` pairs of every
    /// `/v1/vms/import` request the harness has served (#532).
    fn import_requests(&self) -> Vec<(String, Option<String>)> {
        self.import_requests.lock().unwrap().clone()
    }

    fn client(&self, token: Option<String>) -> BffClient {
        BffClient::new(self.url.clone(), token)
    }

    /// The BFF test-suite `seed_jwt_as` pattern: a user row plus a signed
    /// token for the harness's own JWT secret. `sub` is `u-{role}`.
    async fn seed_jwt_as(&self, role: &str) -> String {
        let user_id = format!("u-{role}");
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
             VALUES (?, ?, 'x', ?, 0)",
        )
        .bind(&user_id)
        .bind(role)
        .bind(role)
        .execute(&self.pool)
        .await
        .expect("seed user");

        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let claims = chv_webui_bff::auth::Claims {
            sub: user_id,
            username: role.to_string(),
            role: role.to_string(),
            exp,
            must_change_password: false,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        )
        .expect("encode test token")
    }

    async fn seed_node(&self) {
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
        )
        .execute(&self.pool)
        .await
        .expect("seed node");
    }

    /// A node with a recorded certificate serial — the revocation
    /// command's default target (`nodes.certificate_serial`).
    async fn seed_node_with_serial(&self, node_id: &str, serial: &str) {
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name, certificate_serial) \
             VALUES (?, 'h', 'h', ?)",
        )
        .bind(node_id)
        .bind(serial)
        .execute(&self.pool)
        .await
        .expect("seed node with serial");
    }

    /// A node plus a HEALTHY observed-state row — the vm-import
    /// handler's placement rule (it joins `node_observed_state` on
    /// `health_status = 'healthy'`, unlike the JSON create paths'
    /// first-enrolled-node default that plain [`Harness::seed_node`]
    /// serves). The `csrf_multipart_import.rs` seeding shape (#532).
    async fn seed_healthy_node(&self) {
        sqlx::query(
            "INSERT INTO nodes (node_id, hostname, display_name) VALUES ('n-1', 'h-1', 'Node 1')",
        )
        .execute(&self.pool)
        .await
        .expect("seed node");
        sqlx::query(
            "INSERT INTO node_observed_state (node_id, observed_generation, observed_state, \
             health_status, runtime_status) VALUES ('n-1', 1, 'TenantReady', 'healthy', 'Running')",
        )
        .execute(&self.pool)
        .await
        .expect("seed node observed state");
    }

    /// An enrolled node plus an inventory row advertising exactly these
    /// storage classes — the JSON array of strings the inventory paths
    /// write (the `vm_create_storage_class.rs` seeding shape). #516's
    /// create-side capability check is live, so the DP9 rows exercise
    /// BOTH directions against a reporting node.
    async fn seed_node_with_storage_classes(&self, node_id: &str, classes: &[&str]) {
        sqlx::query("INSERT INTO nodes (node_id, hostname, display_name) VALUES (?, 'h', 'h')")
            .bind(node_id)
            .execute(&self.pool)
            .await
            .expect("seed node");
        sqlx::query(
            "INSERT INTO node_inventory (node_id, architecture, cpu_count, memory_bytes, storage_classes) \
             VALUES (?, 'x86_64', 1, 1024, ?)",
        )
        .bind(node_id)
        .bind(serde_json::to_string(classes).unwrap())
        .execute(&self.pool)
        .await
        .expect("seed node inventory");
    }

    /// A user row (no token) — a `user delete` victim.
    async fn seed_user(&self, user_id: &str, username: &str) {
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
             VALUES (?, ?, 'x', 'viewer', 0)",
        )
        .bind(user_id)
        .bind(username)
        .execute(&self.pool)
        .await
        .expect("seed user");
    }

    /// A VM owned by the seeded operator (`u-operator`), on node `n-1`.
    async fn seed_vm(&self, vm_id: &str) {
        sqlx::query("INSERT INTO vms (vm_id, node_id, display_name, owner_id) VALUES (?, 'n-1', ?, 'u-operator')")
            .bind(vm_id)
            .bind(format!("VM {vm_id}"))
            .execute(&self.pool)
            .await
            .expect("seed vm");
    }

    /// Desired-state row for [`Harness::seed_vm`] — required by the
    /// resize/delete handlers (generation bump, delta quota math).
    async fn seed_vm_desired_state(&self, vm_id: &str) {
        sqlx::query(
            "INSERT INTO vm_desired_state \
             (vm_id, desired_generation, desired_status, desired_power_state, target_node_id, cpu_count, memory_bytes) \
             VALUES (?, 1, 'Active', 'Running', 'n-1', 2, 2147483648)",
        )
        .bind(vm_id)
        .execute(&self.pool)
        .await
        .expect("seed vm desired state");
    }

    /// A volume owned by the seeded operator, on node `n-1` (the
    /// `volume_snapshot_clone.rs` seeding shape).
    async fn seed_volume(&self, volume_id: &str) {
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, updated_at) \
             VALUES (?, 'n-1', ?, 'u-operator', 10737418240, '2026-01-01T00:00:00Z')",
        )
        .bind(volume_id)
        .bind(format!("{volume_id}-disk"))
        .execute(&self.pool)
        .await
        .expect("seed volume");
    }

    /// A DELETABLE standalone volume (the #522 delete-route suite's
    /// `seed_data_volume` shape, contract-tier): `volume_kind = 'data'`
    /// (DP6's gate), owned by the seeded operator, on node `n-1`, with
    /// an `Active` desired state at generation 1 — optionally attached
    /// to a VM (DP5's guard input; the caller seeds the VM and its
    /// desired state, an `Active` VM making the attachment count).
    async fn seed_data_volume(&self, volume_id: &str, attached_vm_id: Option<&str>) {
        sqlx::query(
            "INSERT INTO volumes (volume_id, node_id, display_name, owner_id, capacity_bytes, volume_kind, updated_at) \
             VALUES (?, 'n-1', ?, 'u-operator', 1073741824, 'data', strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
        )
        .bind(volume_id)
        .bind(volume_id)
        .execute(&self.pool)
        .await
        .expect("seed data volume");
        sqlx::query(
            "INSERT INTO volume_desired_state (volume_id, desired_generation, desired_status, requested_by, attached_vm_id) \
             VALUES (?, 1, 'Active', 'u-operator', ?)",
        )
        .bind(volume_id)
        .bind(attached_vm_id)
        .execute(&self.pool)
        .await
        .expect("seed data volume desired state");
    }

    /// An operation row for a volume with an explicit status (the #522
    /// delete-route suite's `seed_volume_operation` shape, DP7's
    /// in-flight guard input). `operation_type` is caller-chosen so the
    /// row can be an in-flight `CreateVolume` (the volume whose create
    /// never dispatched — the DP7 case the in-flight guard exists to
    /// cover).
    async fn seed_volume_operation(
        &self,
        operation_id: &str,
        volume_id: &str,
        operation_type: &str,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by) \
             VALUES (?, ?, 'volume', ?, ?, ?, 'u-operator')",
        )
        .bind(operation_id)
        .bind(format!("seed-{operation_id}"))
        .bind(volume_id)
        .bind(operation_type)
        .bind(status)
        .execute(&self.pool)
        .await
        .expect("seed volume operation");
    }

    /// A bridge network owned by the seeded operator, with a desired-state
    /// row (the `network_resolution.rs` seeding shape).
    async fn seed_network(&self, network_id: &str) {
        sqlx::query(
            "INSERT INTO networks (network_id, node_id, display_name, network_class, owner_id, created_at, updated_at) \
             VALUES (?, 'n-1', ?, 'bridge', 'u-operator', strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
        )
        .bind(network_id)
        .bind(format!("net {network_id}"))
        .execute(&self.pool)
        .await
        .expect("seed network");
        sqlx::query(
            "INSERT INTO network_desired_state (network_id, desired_generation, desired_status, cidr, gateway, dhcp_enabled, ipam_mode, is_default, requested_at, updated_at) \
             VALUES (?, 1, 'Pending', '10.70.0.0/24', '10.70.0.1', 1, 'internal', 0, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
        )
        .bind(network_id)
        .execute(&self.pool)
        .await
        .expect("seed network desired state");
    }

    async fn seed_image(&self, image_id: &str) {
        sqlx::query(
            "INSERT INTO images \
             (image_id, display_name, image_type, format, size_bytes, checksum, source_url, os, version, usage_count, status, node_id, created_at, updated_at) \
             VALUES (?, ?, 'disk', 'qcow2', 1073741824, NULL, ?, '', '', 0, 'available', NULL, datetime('now'), datetime('now'))",
        )
        .bind(image_id)
        .bind(format!("img {image_id}"))
        .bind(format!("/tmp/{image_id}.img"))
        .execute(&self.pool)
        .await
        .expect("seed image");
    }

    /// Seed one operation row (the `list_tasks` seeding shape) — the row
    /// `task watch` polls for.
    async fn seed_operation(&self, operation_id: &str) {
        self.seed_operation_ext(operation_id, "Succeeded", None, None)
            .await;
    }

    /// The #502 shape: an operation row in an arbitrary status, optionally
    /// carrying the journaled terminal-failure cause
    /// (`error_code`/`error_message`, the #498/#500 fast-fail columns the
    /// BFF now surfaces).
    async fn seed_operation_ext(
        &self,
        operation_id: &str,
        status: &str,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by, error_code, error_message, requested_at, created_at, updated_at) \
             VALUES (?, ?, 'vm', 'vm-1', 'CreateVm', ?, 'u-operator', ?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(operation_id)
        .bind(format!("contract-{operation_id}"))
        .bind(status)
        .bind(error_code)
        .bind(error_message)
        .execute(&self.pool)
        .await
        .expect("seed operation");
    }

    /// A migration row for the migrate status/cancel/list rows (the
    /// `0038_migration_operations.sql` shape): VM `vm-1` moving `n-1`
    /// → `n-2`, referencing its own operation row. `phase` must be
    /// NON-terminal for the cancel row (`request_migration_cancel`
    /// answers `AlreadyTerminal` — still a 2xx, but the row asserts
    /// the flag actually landed).
    async fn seed_migration(&self, migration_id: &str, phase: &str) {
        sqlx::query(
            "INSERT INTO operations (operation_id, idempotency_key, resource_kind, resource_id, operation_type, status, requested_by, requested_at, created_at, updated_at) \
             VALUES (?, ?, 'vm', 'vm-1', 'MigrateVm', 'Running', 'u-operator', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(format!("op-{migration_id}"))
        .bind(format!("contract-{migration_id}"))
        .execute(&self.pool)
        .await
        .expect("seed migration operation");
        sqlx::query(
            "INSERT INTO migrations (migration_id, operation_id, vm_id, source_node_id, destination_node_id, phase, bytes_transferred, total_bytes, convergence_round, dirty_blocks_remaining) \
             VALUES (?, ?, 'vm-1', 'n-1', 'n-2', ?, 500, 1000, 2, 7)",
        )
        .bind(migration_id)
        .bind(format!("op-{migration_id}"))
        .bind(phase)
        .execute(&self.pool)
        .await
        .expect("seed migration");
    }
}

/// The `(content-type, x-csrf-token)` pairs of every
/// `/v1/vms/import` request the harness has served (#532).
type ImportRequests = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// The BFF test-suite `AppState` builder (`tests/volume_snapshot_clone.rs`
/// pattern): in-memory pool, all repositories, recording mutation stub,
/// and (since #532) a caller-provided agent runtime dir — the vm-import
/// handler writes uploaded images there, so the harness passes a
/// tempdir (the JSON routes never touch it).
fn build_state(
    pool: sqlx::SqlitePool,
    mutations: Arc<RecordingMutations>,
    agent_runtime_dir: std::path::PathBuf,
) -> AppState {
    AppState {
        node_repo: NodeRepository::new(pool.clone()),
        operation_repo: OperationRepository::new(pool.clone()),
        event_repo: EventRepository::new(pool.clone()),
        alert_repo: AlertRepository::new(pool.clone()),
        alert_rules: Arc::new(AlertRuleRepository::new(pool.clone())),
        notification_outbox: Arc::new(NotificationOutboxRepository::new(pool.clone())),
        alerting_max_rules: 200,
        notification_channels: chv_webui_bff::NotificationChannels {
            webhook: false,
            slack: false,
        },
        desired_state_repo: DesiredStateRepository::new(pool.clone()),
        observed_state_repo: ObservedStateRepository::new(pool.clone()),
        backup_repo: BackupRepository::new(pool.clone()),
        topology_repo: TopologyRepository::new(pool.clone()),
        network_repo: NetworkRepository::new(pool.clone()),
        image_repo: ImageRepository::new(pool.clone()),
        apply_runs: Arc::new(ApplyRunRepository::new(pool.clone())),
        drift_reports: Arc::new(DriftReportRepository::new(pool.clone())),
        netbox_config: Arc::new(
            chv_controlplane_store::NetboxProjectionConfigRepository::new(pool.clone()),
        ),
        netbox_runs: Arc::new(chv_controlplane_store::NetboxProjectionRunRepository::new(
            pool.clone(),
        )),
        mutations,
        jwt_secret: "test-secret".to_string(),
        agent_runtime_dir,
        monitoring: None,
        monitoring_health: chv_webui_bff::MonitoringHealth::new(),
        cache: chv_webui_bff::BffCache::new(5),
        clock: Arc::new(SystemClock),
        pool,
    }
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

/// Fetch a list endpoint's items through the same client the command
/// used (the command itself only prints; the row re-fetches to inspect
/// the response shape the columns render from).
async fn list_items(client: &BffClient, path: &str) -> Vec<Value> {
    let resp = client
        .post(path, &serde_json::json!({}))
        .await
        .expect("list request through the harness server");
    resp.get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// The GET form of [`list_items`] — for the REST-shaped read routes
/// (`GET /v1/migrations`, #372 DP4b).
async fn get_items(client: &BffClient, path: &str) -> Vec<Value> {
    let resp = client
        .get(path)
        .await
        .expect("list request through the harness server");
    resp.get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Every column chvctl's `print_list` prints exists as a key in every
/// response item (the §2.7(a) display-drift guard, green form).
fn assert_columns_present(items: &[Value], columns: &[&str]) {
    assert!(
        !items.is_empty(),
        "expected at least one item to check display columns {columns:?}"
    );
    for (i, item) in items.iter().enumerate() {
        for col in columns {
            assert!(
                item.get(*col).is_some(),
                "item {i} is missing column {col:?} — chvctl prints it, the BFF does not \
                 serve it (display drift)"
            );
        }
    }
}

/// Assert a command failed with exactly the given API status (the
/// pinned-broken rows' 400/404 pins).
fn assert_api_error(result: Result<(), CliError>, status: u16) {
    match result {
        Err(CliError::Api { status: s, .. }) => assert_eq!(
            s, status,
            "expected the pinned HTTP {status} drift behavior"
        ),
        other => panic!("expected HTTP {status}, got {other:?}"),
    }
}

/// Minimal qcow2-looking payload: the import handler only checks the
/// 4-byte magic before accepting the stream (the
/// `csrf_multipart_import.rs` shape, #532).
fn qcow2_bytes() -> Vec<u8> {
    let mut bytes = b"QFI\xfb".to_vec();
    bytes.extend_from_slice(b"\x00\x00\x00\x03rest-of-disk");
    bytes
}

/// (vm rows, volume rows, operation rows) — the zero-journaling
/// assertion for rejected requests (the `csrf_multipart_import.rs`
/// discipline, contract-tier; #532).
async fn journal_counts(pool: &sqlx::SqlitePool) -> (i64, i64, i64) {
    let vms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vms")
        .fetch_one(pool)
        .await
        .expect("count vms");
    let volumes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volumes")
        .fetch_one(pool)
        .await
        .expect("count volumes");
    let ops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operations")
        .fetch_one(pool)
        .await
        .expect("count operations");
    (vms, volumes, ops)
}

// ---------------------------------------------------------------------------
// Rows — auth
// ---------------------------------------------------------------------------

/// `chvctl login` — GREEN. Fields match `handlers/auth.rs` (`username`,
/// `password` → `token`); the credentials write is redirected to a
/// tempdir via `XDG_CONFIG_HOME`. The login rate limiter is a static
/// in-process map; a single row stays far under its window.
#[tokio::test]
async fn login_row() {
    let h = Harness::start().await;
    let hash = bcrypt::hash("contract-login-password", 12).expect("hash login password");
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, role, must_change_password) \
         VALUES ('u-login', 'loginuser', ?, 'viewer', 0)",
    )
    .bind(&hash)
    .execute(&h.pool)
    .await
    .expect("seed login user");

    // `config::save_credentials` writes under `dirs::config_dir()`, which
    // honors XDG_CONFIG_HOME on Linux. Restore the previous value after
    // the row (review round 1: the process-global set was never cleared —
    // harmless today since only this row reads the config dir, but a
    // later row must not inherit this tempdir silently).
    let dir = tempfile::tempdir().expect("login config tempdir");
    let prior_xdg = std::env::var("XDG_CONFIG_HOME").ok();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let client = h.client(None);
    auth::execute(
        &client,
        auth::LoginArgs {
            username: "loginuser".to_string(),
            password: "contract-login-password".to_string(),
        },
    )
    .await
    .expect("chvctl login against /v1/auth/login");

    let credentials = std::fs::read_to_string(dir.path().join("chvctl/credentials"))
        .expect("credentials file written under XDG_CONFIG_HOME");
    assert!(!credentials.trim().is_empty(), "token was persisted");

    match prior_xdg {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
}

// ---------------------------------------------------------------------------
// Rows — vm (all GREEN at main)
// ---------------------------------------------------------------------------

/// `chvctl vm list` — GREEN. Columns match `handlers/vms.rs::list_vms`.
#[tokio::test]
async fn vm_list_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    vm::execute(&client, vm::VmCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl vm list against POST /v1/vms");

    let items = list_items(&client, "/v1/vms").await;
    assert_columns_present(
        &items,
        &["vm_id", "name", "power_state", "node_id", "cpu", "memory"],
    );
}

/// `chvctl vm get <id>` — GREEN.
#[tokio::test]
async fn vm_get_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Get {
            vm_id: "vm-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm get against POST /v1/vms/get");
}

/// `chvctl vm create` — GREEN. Field names (`name`, `cpu_count`,
/// `memory_bytes`, `image_ref`, `network_id`) are the BFF create_vm
/// contract; the create needs an enrolled node (the handler's
/// default-placement rule). §2.7(b) recorded the capability gap (no
/// `--node`/`--storage-class`/`--disk-size-gb`/`--cloud-init` flags) —
/// the DP9 flags landed with their own rows below.
#[tokio::test]
async fn vm_create_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    vm::execute(
        &client,
        vm::VmCommands::Create {
            name: "contract-vm".to_string(),
            cpu: Some(2),
            memory: Some("512M".to_string()),
            image: Some("default".to_string()),
            network: Some("default".to_string()),
            node: None,
            storage_class: None,
            disk_size_gb: None,
            cloud_init: None,
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm create against POST /v1/vms/create");

    // The VM actually landed (route + field names accepted end-to-end).
    let items = list_items(&client, "/v1/vms").await;
    let vm = items
        .iter()
        .find(|i| i.get("name").and_then(Value::as_str) == Some("contract-vm"))
        .expect("created VM is visible in vm list");
    // Optional-field probes (review round 1): `cpu_count`/`memory_bytes`
    // are optional-with-default in create_vm, so a BFF-side rename would
    // silently default them and keep a name-only check green. vm list
    // serves them display-formatted (cpu as a string, memory
    // human-readable) — assert the created shape round-tripped exactly
    // what chvctl sent (512M = 536870912 bytes = "512.0 MiB").
    assert_eq!(
        vm.get("cpu").and_then(Value::as_str),
        Some("2"),
        "cpu_count must round-trip — a BFF rename would silently default it"
    );
    assert_eq!(
        vm.get("memory").and_then(Value::as_str),
        Some("512.0 MiB"),
        "memory_bytes must round-trip — a BFF rename would silently default it"
    );
    // `image_ref`/`network_id` are also optional-with-default but are not
    // served by vm list (only cpu/memory are); they stay name-probed here
    // — the DP9 flags add their own rows with shape assertions below.
}

/// `chvctl vm create --node --storage-class --disk-size-gb --cloud-init`
/// — GREEN (#372 DP9, design §2.7(b)). The flags map to the BFF
/// create_vm contract fields `node_id`/`storage_class`/`volume_size_gb`/
/// `cloud_init_userdata`; the node's inventory advertises `["local"]`,
/// so #516's create-side capability check accepts the offered class.
/// Shape probes mirror the base row: a BFF-side rename of any of the
/// four field names would silently default it and fail the round-trip
/// assertions below. The `--storage-class` input deliberately carries a
/// trailing space: the Create arm trims the flag value before
/// validating and before sending (parity with the BFF's own
/// trim-before-check — e.g. a shell tab-completion trailing space), and
/// this row is the trim's end-to-end pin — dropping the arm's trim
/// flips this row red (the exact-match client-side validator rejects
/// the untrimmed value; #519 second-pass review NIT).
#[tokio::test]
async fn vm_create_storage_class_row() {
    let h = Harness::start().await;
    h.seed_node_with_storage_classes("n-local", &["local"])
        .await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    vm::execute(
        &client,
        vm::VmCommands::Create {
            name: "contract-vm-local".to_string(),
            cpu: Some(1),
            memory: Some("512M".to_string()),
            image: Some("default".to_string()),
            network: Some("default".to_string()),
            node: Some("n-local".to_string()),
            // Trailing whitespace ON PURPOSE — see the doc comment: the
            // trimmed value ("local") is what must reach the wire and
            // the journaled boot volume.
            storage_class: Some("local ".to_string()),
            disk_size_gb: Some(5),
            cloud_init: Some("#cloud-config\n".to_string()),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm create --storage-class local against POST /v1/vms/create");

    // `node_id` round-trips through vm list.
    let items = list_items(&client, "/v1/vms").await;
    let vm = items
        .iter()
        .find(|i| i.get("name").and_then(Value::as_str) == Some("contract-vm-local"))
        .expect("created VM is visible in vm list");
    assert_eq!(
        vm.get("node_id").and_then(Value::as_str),
        Some("n-local"),
        "node_id must round-trip — a BFF rename would fall back to default placement"
    );
    // `storage_class`/`volume_size_gb`/`cloud_init_userdata` are not
    // served by vm list — assert the journaled rows directly (the
    // create tx writes volumes + vm_desired_state).
    let class: Option<String> = sqlx::query_scalar(
        "SELECT storage_class FROM volumes WHERE display_name = 'contract-vm-local-disk'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("boot volume row");
    assert_eq!(
        class.as_deref(),
        Some("local"),
        "storage_class must round-trip onto the boot volume"
    );
    let capacity: Option<i64> = sqlx::query_scalar(
        "SELECT capacity_bytes FROM volumes WHERE display_name = 'contract-vm-local-disk'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("boot volume capacity");
    assert_eq!(
        capacity,
        Some(5 * 1024 * 1024 * 1024),
        "volume_size_gb must round-trip (5 GiB)"
    );
    let userdata: Option<String> = sqlx::query_scalar(
        "SELECT cloud_init_userdata FROM vm_desired_state \
         WHERE vm_id = (SELECT vm_id FROM vms WHERE display_name = 'contract-vm-local')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("vm desired state row");
    assert_eq!(
        userdata.as_deref(),
        Some("#cloud-config\n"),
        "cloud_init_userdata must round-trip"
    );
}

/// `chvctl vm create --storage-class lvm` on a node that reports only
/// `["local"]` — the DP9 REJECTION row, mirroring the BFF-tier
/// `vm_create_storage_class.rs` tests at the contract tier: #516's
/// create-side capability check rejects the unoffered class with 400
/// BEFORE the create transaction, so nothing is journaled.
#[tokio::test]
async fn vm_create_storage_class_rejection_row() {
    let h = Harness::start().await;
    h.seed_node_with_storage_classes("n-local", &["local"])
        .await;
    let token = h.seed_jwt_as("operator").await;

    // `lvm` is a valid DP3 vocabulary class, so it passes chvctl's
    // client-side check and the rejection comes from the BFF.
    let result = vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Create {
            name: "contract-vm-lvm".to_string(),
            cpu: Some(1),
            memory: Some("512M".to_string()),
            image: Some("default".to_string()),
            network: Some("default".to_string()),
            node: Some("n-local".to_string()),
            storage_class: Some("lvm".to_string()),
            disk_size_gb: Some(5),
            cloud_init: None,
        },
        &OutputFormat::Json,
    )
    .await;
    assert_api_error(result, 400);

    // A rejected create journals nothing (every table the create tx
    // writes — the `vm_create_storage_class.rs` discipline).
    for table in [
        "vms",
        "vm_desired_state",
        "volumes",
        "volume_desired_state",
        "operations",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&h.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected create must not journal {table}");
    }
}

/// `chvctl vm start` — GREEN; forwarded to the mutation service.
#[tokio::test]
async fn vm_start_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Start {
            vm_id: "vm-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm start against POST /v1/vms/mutate");
    h.mutations.assert_recorded("mutate_vm:vm-1:start:false");
}

/// `chvctl vm stop` — GREEN; forwarded to the mutation service.
#[tokio::test]
async fn vm_stop_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Stop {
            vm_id: "vm-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm stop against POST /v1/vms/mutate");
    h.mutations.assert_recorded("mutate_vm:vm-1:stop:false");
}

/// `chvctl vm reboot` — GREEN; the wire action is `restart` (the BFF's
/// mutation vocabulary), forwarded to the mutation service.
#[tokio::test]
async fn vm_reboot_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Reboot {
            vm_id: "vm-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm reboot against POST /v1/vms/mutate");
    h.mutations.assert_recorded("mutate_vm:vm-1:restart:false");
}

/// `chvctl vm delete` — GREEN. The delete is a BFF-internal journaled
/// path (tombstone + operation row), not a mutation-service forward.
#[tokio::test]
async fn vm_delete_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    h.seed_vm_desired_state("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Delete {
            vm_id: "vm-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm delete against POST /v1/vms/delete");
}

/// `chvctl vm migrate --pause-first` — GREEN; forwarded to the mutation
/// service with the `target_node_id` field name (this is the path
/// `migrate start` should delegate to — design §2.3). Issue #656 makes
/// pause-first mandatory, so the only positive row sends `true`.
#[tokio::test]
async fn vm_migrate_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Migrate {
            vm_id: "vm-1".to_string(),
            to: "n-2".to_string(),
            pause_first: true,
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm migrate --pause-first against POST /v1/vms/mutate");
    h.mutations
        .assert_recorded("migrate_vm:vm-1:n-2:pause_first");
}

/// Issue #656: `chvctl vm migrate` without `--pause-first` fails closed
/// client-side. The live path must never reach the BFF (no mutation call
/// recorded), and the error must tell the operator to pass the flag.
#[tokio::test]
async fn vm_migrate_without_pause_first_is_rejected_locally() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    let err = vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Migrate {
            vm_id: "vm-1".to_string(),
            to: "n-2".to_string(),
            pause_first: false,
        },
        &OutputFormat::Json,
    )
    .await
    .expect_err("the live migration path must fail closed");
    match err {
        CliError::Usage(msg) => assert!(
            msg.contains("#394") && msg.contains("--pause-first"),
            "the local rejection must explain the gate and remedy: {msg}"
        ),
        other => panic!("expected a usage error, got {other:?}"),
    }
    assert!(
        h.mutations.calls().is_empty(),
        "no HTTP mutation may be recorded for a locally rejected live migration: {:?}",
        h.mutations.calls()
    );
}

/// `chvctl vm migrate --pause-first` — the issue #394 Option C opt-in
/// must survive the whole thread: CLI flag → mutate body → BFF handler
/// → mutation service.
#[tokio::test]
async fn vm_migrate_pause_first_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Migrate {
            vm_id: "vm-1".to_string(),
            to: "n-2".to_string(),
            pause_first: true,
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm migrate --pause-first against POST /v1/vms/mutate");
    h.mutations
        .assert_recorded("migrate_vm:vm-1:n-2:pause_first");
}

/// `chvctl vm resize` — GREEN (`cpu_count` / `memory_bytes` field names).
#[tokio::test]
async fn vm_resize_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    h.seed_vm_desired_state("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Resize {
            vm_id: "vm-1".to_string(),
            cpu: Some(4),
            memory: Some("1G".to_string()),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm resize against POST /v1/vms/resize");
}

/// `chvctl vm import <name> --file <qcow2>` — GREEN (#532, the route's
/// first in-tree client). The command's request shape is the route's
/// whole contract: multipart POST `/v1/vms/import` with a `name` text
/// field and a `file` binary field, PLUS the non-empty `x-csrf-token`
/// header the #531 CSRF gate requires on multipart (chvctl's only
/// multipart request — every other mutation is JSON, which the gate
/// admits on content type alone). The row pins, through the command's
/// own wire path: the header presence (captured by the harness
/// middleware on the command's actual request — the one thing a
/// client regression could silently drop, and the exact 403 the gate
/// answers without it is pinned by the rejection row below), the
/// part names and content (the journaled `display_name` rows for the
/// `name` field; the byte-identical written disk for the `file`
/// field), the #386/#481 ownership stamping (the importing operator
/// on the vm, volume, and operation rows), the journaled row shapes
/// (the `-disk` volume at the uploaded byte count, attached to the
/// imported VM; the Accepted `CreateVm` operation keyed
/// `import-vm-{vm_id}`), and the response shape — pinned through a
/// re-drive of the command's own client method (`post_multipart`,
/// the `volume_delete_row` re-drive discipline; unlike that route
/// the import is not idempotent, so the re-drive uses a fresh name
/// and asserts the journal grew by exactly one of each row).
#[tokio::test]
async fn vm_import_row() {
    let h = Harness::start().await;
    h.seed_healthy_node().await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    // The upload: a real on-disk qcow2 file — the operator's actual
    // path (the command reads --file from disk).
    let dir = tempfile::tempdir().expect("import upload tempdir");
    let disk = dir.path().join("disk.qcow2");
    std::fs::write(&disk, qcow2_bytes()).expect("write upload file");

    vm::execute(
        &client,
        vm::VmCommands::Import {
            name: "imported-vm".to_string(),
            file: disk.to_string_lossy().to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl vm import against POST /v1/vms/import");

    // The command's own wire shape (#531's gate contract, captured on
    // the command's actual request): multipart content type plus a
    // NON-EMPTY x-csrf-token header — the one thing the gate adds for
    // multipart. Dropping the header from the client flips this red.
    let requests = h.import_requests();
    assert_eq!(
        requests.len(),
        1,
        "exactly one import request so far: {requests:?}"
    );
    assert!(
        requests[0].0.starts_with("multipart/form-data"),
        "the import request must be multipart: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .1
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty()),
        "the command's client must send the non-empty x-csrf-token the gate requires: {:?}",
        requests[0]
    );

    // The journaled row shapes (the #386/#481 ownership stamping, the
    // `volume_delete_row` discipline): the VM stamped with the
    // importing operator — and its display_name is the `name` part's
    // pin (a mis-keyed part would have 400'd "missing name").
    let (vm_id, vm_owner): (String, String) =
        sqlx::query_as("SELECT vm_id, owner_id FROM vms WHERE display_name = 'imported-vm'")
            .fetch_one(&h.pool)
            .await
            .expect("imported vm row");
    assert_eq!(
        vm_owner, "u-operator",
        "the import must stamp the importing operator on the vm row (#386)"
    );

    // Its disk volume: the derived `-disk` display name, the uploaded
    // byte count as the capacity, the same owner, attached to the
    // imported VM.
    let (volume_id, vol_owner, capacity): (String, String, i64) = sqlx::query_as(
        "SELECT volume_id, owner_id, capacity_bytes FROM volumes WHERE display_name = 'imported-vm-disk'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("imported volume row");
    assert_eq!(
        vol_owner, "u-operator",
        "the import must stamp the importing operator on the volume row (#386)"
    );
    assert_eq!(
        capacity,
        qcow2_bytes().len() as i64,
        "the volume capacity must be the uploaded file's size"
    );
    let (attached_vm, vds_status): (String, String) = sqlx::query_as(
        "SELECT attached_vm_id, desired_status FROM volume_desired_state WHERE volume_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&h.pool)
    .await
    .expect("imported volume desired state");
    assert_eq!(
        attached_vm, vm_id,
        "the imported disk attaches to the imported VM"
    );
    assert_eq!(
        vds_status, "Pending",
        "the imported volume's desired status is Pending"
    );

    // The Accepted CreateVm operation with the handler's idempotency
    // key, stamped with the importing operator.
    let (op_type, op_status, op_requester, op_key): (String, String, String, String) =
        sqlx::query_as(
            "SELECT operation_type, status, requested_by, idempotency_key \
             FROM operations WHERE resource_kind = 'vm' AND resource_id = ?",
        )
        .bind(&vm_id)
        .fetch_one(&h.pool)
        .await
        .expect("import operation row");
    assert_eq!(op_type, "CreateVm");
    assert_eq!(op_status, "Accepted");
    assert_eq!(op_requester, "u-operator");
    assert_eq!(op_key, format!("import-vm-{vm_id}"));

    // The `file` part's content pin: the uploaded image lands under
    // the agent runtime dir byte-identical to what --file held.
    let written = tokio::fs::read(
        h.agent_runtime_dir
            .join("vms")
            .join(&vm_id)
            .join("disk.qcow2"),
    )
    .await
    .expect("the uploaded image lands under the agent runtime dir");
    assert_eq!(
        written,
        qcow2_bytes(),
        "the written disk must be byte-identical to the uploaded file part"
    );

    // The response shape (the command only prints; the row re-drives
    // the command's own client method with a fresh name — the import
    // is not idempotent, so a re-drive is a second VM, and the row
    // asserts the journal grew by exactly one of each row).
    let resp = client
        .post_multipart(
            "/v1/vms/import",
            "imported-vm-2",
            "disk.qcow2",
            qcow2_bytes(),
        )
        .await
        .expect("re-drive of the command's multipart client method");
    assert_eq!(
        resp.get("name").and_then(Value::as_str),
        Some("imported-vm-2"),
        "the import response must round-trip the name: {resp}"
    );
    assert!(
        resp.get("id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "the import response must carry the server-minted id: {resp}"
    );
    assert_eq!(
        resp.get("image_id").and_then(Value::as_str),
        Some("imported"),
        "the import response must carry the imported-image sentinel: {resp}"
    );
    assert_eq!(
        resp.get("desired_state").and_then(Value::as_str),
        Some("Pending"),
        "the import response must carry the Pending desired state: {resp}"
    );
    assert!(
        resp.get("disk_path")
            .and_then(Value::as_str)
            .is_some_and(|s| s.ends_with("disk.qcow2")),
        "the import response must carry the written disk path: {resp}"
    );
    assert_eq!(
        journal_counts(&h.pool).await,
        (2, 2, 2),
        "the re-drive journaled exactly one more of each row"
    );
    // Both captured requests carried the gate's marker (the re-drive
    // goes through the same client method the command used).
    for (content_type, csrf) in &h.import_requests() {
        assert!(content_type.starts_with("multipart/form-data"));
        assert!(
            csrf.as_deref().is_some_and(|v| !v.trim().is_empty()),
            "every import request through the client must carry the marker"
        );
    }

    // Observability: the imported VM renders in `vm list` with the
    // display columns chvctl prints (the §2.7(a) guard).
    let items = list_items(&client, "/v1/vms").await;
    assert!(items
        .iter()
        .any(|i| i.get("name").and_then(Value::as_str) == Some("imported-vm")));
    assert_columns_present(
        &items,
        &["vm_id", "name", "power_state", "node_id", "cpu", "memory"],
    );
}

/// `POST /v1/vms/import` multipart WITHOUT the `x-csrf-token` header —
/// the #531 gate's rejection, pinned at the same listener the command
/// talks to (#532). The request is shaped exactly like the command's
/// (same `name`/`file` parts, same fully valid operator JWT) minus the
/// one header the client sends — so a 403 here is provably the CSRF
/// layer, not auth, and a future client regression (dropping the
/// header) fails loudly as `403 CSRF_REJECTED` instead of being
/// misattributed to auth (the #532 issue's exact risk). The blank
/// header is pinned too: the gate requires NON-EMPTY. The header's
/// presence on the command's own path is pinned by `vm_import_row`'s
/// capture — this row pins what its absence buys.
#[tokio::test]
async fn vm_import_csrf_rejection_row() {
    let h = Harness::start().await;
    h.seed_healthy_node().await;
    let token = h.seed_jwt_as("operator").await;

    // The command's multipart shape minus the CSRF marker: a raw
    // reqwest request (the client method under test always sends the
    // header — that presence is pinned by vm_import_row's capture).
    let http = reqwest::Client::new();
    for csrf_header in [None, Some("   ")] {
        let file_part = reqwest::multipart::Part::bytes(qcow2_bytes())
            .file_name("disk.qcow2")
            .mime_str("application/octet-stream")
            .expect("static mime type");
        let form = reqwest::multipart::Form::new()
            .text("name", "csrf-less-vm")
            .part("file", file_part);
        let mut req = http
            .post(format!("{}/v1/vms/import", h.url))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(marker) = csrf_header {
            req = req.header("x-csrf-token", marker);
        }
        let resp = req
            .multipart(form)
            .send()
            .await
            .expect("multipart POST without the CSRF marker");
        assert_eq!(
            resp.status().as_u16(),
            403,
            "a multipart import without a usable CSRF marker must be rejected before routing"
        );
        let body: Value = resp.json().await.expect("rejection body");
        assert_eq!(
            body.get("code").and_then(Value::as_str),
            Some("CSRF_REJECTED"),
            "the rejection must name the CSRF layer: {body}"
        );
    }

    // The gate fired before the handler: nothing journaled, even
    // though the JWT was fully valid (the rejection is the CSRF
    // layer's, not auth's). The same JWT succeeds on a JSON route —
    // making that attribution airtight rather than contextual
    // (review-folded: the gate runs before auth, so the 403 alone
    // would not distinguish a CSRF rejection from an auth failure).
    let json_resp = http
        .post(format!("{}/v1/tasks", h.url))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("JSON probe with the same JWT");
    assert_eq!(
        json_resp.status().as_u16(),
        200,
        "the same JWT must succeed on a JSON route — isolating the 403 to the CSRF layer"
    );
    assert_eq!(
        journal_counts(&h.pool).await,
        (0, 0, 0),
        "a CSRF-rejected import must not journal anything"
    );

    // And the harness saw exactly the two rejected requests, neither
    // carrying a usable marker (absent / blank).
    let requests = h.import_requests();
    assert_eq!(
        requests.len(),
        2,
        "exactly the two probe requests: {requests:?}"
    );
    for (content_type, csrf) in &requests {
        assert!(content_type.starts_with("multipart/form-data"));
        assert!(
            csrf.as_deref().is_none_or(|v| v.trim().is_empty()),
            "neither rejection carried a usable marker: {requests:?}"
        );
    }
}

/// `chvctl vm import` with a non-qcow2 file — the `file` part's
/// CONTENT contract: the handler checks the qcow2 magic on the
/// uploaded bytes, so a file that is not a qcow2 image is a loud 400
/// naming it (surfaced verbatim through the command's error path),
/// and nothing is journaled. Discriminating against a mis-wired
/// client: if the bytes went under the wrong part (or the handler
/// stopped reading the magic), the message this row matches changes
/// ("missing file or file too small to be qcow2" / a 200).
#[tokio::test]
async fn vm_import_non_qcow2_rejection_row() {
    let h = Harness::start().await;
    h.seed_healthy_node().await;
    let token = h.seed_jwt_as("operator").await;

    let dir = tempfile::tempdir().expect("import upload tempdir");
    let disk = dir.path().join("not-qcow2.img");
    std::fs::write(&disk, b"\x00\x00\x00\x00definitely-not-qcow2").expect("write non-qcow2 file");

    let result = vm::execute(
        &h.client(Some(token)),
        vm::VmCommands::Import {
            name: "not-qcow2-vm".to_string(),
            file: disk.to_string_lossy().to_string(),
        },
        &OutputFormat::Json,
    )
    .await;
    match &result {
        Err(CliError::Api { status, message }) => {
            assert_eq!(*status, 400, "a non-qcow2 upload rejects with 400");
            assert!(
                message.contains("not a valid qcow2 image"),
                "the 400 the CLI surfaces must name the qcow2 magic check: {message}"
            );
        }
        other => panic!("expected the non-qcow2 400, got {other:?}"),
    }
    assert_eq!(
        journal_counts(&h.pool).await,
        (0, 0, 0),
        "a rejected import must not journal anything"
    );
}

// ---------------------------------------------------------------------------
// Rows — node (all GREEN at main)
// ---------------------------------------------------------------------------

/// `chvctl node list` — GREEN. Columns match `handlers/nodes.rs`.
#[tokio::test]
async fn node_list_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    node::execute(&client, node::NodeCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl node list against POST /v1/nodes");

    let items = list_items(&client, "/v1/nodes").await;
    assert_columns_present(
        &items,
        &["node_id", "name", "state", "health", "cpu", "memory"],
    );
}

/// `chvctl node get <id>` — GREEN.
#[tokio::test]
async fn node_get_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    let token = h.seed_jwt_as("operator").await;

    node::execute(
        &h.client(Some(token)),
        node::NodeCommands::Get {
            node_id: "n-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl node get against POST /v1/nodes/get");
}

/// `chvctl node drain` — GREEN; forwarded to the mutation service.
#[tokio::test]
async fn node_drain_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("operator").await;

    node::execute(
        &h.client(Some(token)),
        node::NodeCommands::Drain {
            node_id: "n-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl node drain against POST /v1/nodes/mutate");
    h.mutations.assert_recorded("mutate_node:n-1:drain");
}

/// `chvctl node maintenance --enable` — GREEN; action
/// `enter_maintenance` forwarded to the mutation service.
#[tokio::test]
async fn node_maintenance_enter_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("operator").await;

    node::execute(
        &h.client(Some(token)),
        node::NodeCommands::Maintenance {
            node_id: "n-1".to_string(),
            enable: true,
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl node maintenance (enter) against POST /v1/nodes/mutate");
    h.mutations
        .assert_recorded("mutate_node:n-1:enter_maintenance");
}

/// `chvctl node maintenance` (no --enable) — GREEN; action
/// `exit_maintenance` forwarded to the mutation service.
#[tokio::test]
async fn node_maintenance_exit_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("operator").await;

    node::execute(
        &h.client(Some(token)),
        node::NodeCommands::Maintenance {
            node_id: "n-1".to_string(),
            enable: false,
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl node maintenance (exit) against POST /v1/nodes/mutate");
    h.mutations
        .assert_recorded("mutate_node:n-1:exit_maintenance");
}

/// `chvctl node revoke-certificate` — GREEN (#661). Revokes the node's
/// recorded serial, is idempotent on a second call, and writes exactly one
/// denylist row.
#[tokio::test]
async fn node_revoke_certificate_row() {
    let h = Harness::start().await;
    h.seed_node_with_serial("n-1", "0a3f7b2c").await;
    let token = h.seed_jwt_as("admin").await;
    let client = h.client(Some(token));

    let command = || node::NodeCommands::RevokeCertificate {
        node_id: "n-1".to_string(),
        certificate_serial: None,
        reason: "host compromise".to_string(),
        expires_at: None,
    };

    node::execute(&client, command(), &OutputFormat::Json)
        .await
        .expect("chvctl node revoke-certificate against POST /v1/nodes/revoke-certificate");
    node::execute(&client, command(), &OutputFormat::Json)
        .await
        .expect("repeat revocation must be idempotent");

    let repo = chv_controlplane_store::CertificateRevocationRepository::new(h.pool.clone());
    let rows = repo.list_for_node("n-1").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].certificate_serial, "0a3f7b2c");
    assert_eq!(rows[0].reason, "host compromise");
}

// ---------------------------------------------------------------------------
// Rows — image (route/field GREEN; `image list` display-pinned)
// ---------------------------------------------------------------------------

/// `chvctl image list` — GREEN. Columns are the keys the BFF serves
/// (#372 DP10/§2.7(a)): the phantom `format` column was dropped —
/// chvctl's list and the BFF's items now agree on
/// image_id/name/size/status.
#[tokio::test]
async fn image_list_row() {
    let h = Harness::start().await;
    h.seed_image("img-1").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    image::execute(&client, image::ImageCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl image list against POST /v1/images");

    let items = list_items(&client, "/v1/images").await;
    assert_columns_present(&items, &["image_id", "name", "size", "status"]);
}

/// `chvctl image import` — GREEN. `source_url` is the BFF import
/// contract key; the redundant dual-key `url` send (design §2.1
/// residual / DP10) is dropped — the server alias stays server-side.
#[tokio::test]
async fn image_import_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    image::execute(
        &client,
        image::ImageCommands::Import {
            name: "contract-image".to_string(),
            url: "/tmp/chv-contract-row.img".to_string(),
            format: "qcow2".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl image import against POST /v1/images/import");
}

/// `chvctl image delete` — GREEN.
#[tokio::test]
async fn image_delete_row() {
    let h = Harness::start().await;
    h.seed_image("img-1").await;
    let token = h.seed_jwt_as("operator").await;

    image::execute(
        &h.client(Some(token)),
        image::ImageCommands::Delete {
            image_id: "img-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl image delete against POST /v1/images/delete");
}

// ---------------------------------------------------------------------------
// Rows — volume (route/field GREEN; `volume list` display-pinned)
// ---------------------------------------------------------------------------

/// `chvctl volume list` — GREEN. Columns are the keys the BFF serves
/// (#372 DP10/§2.7(a)): the phantom `attached_to` column was replaced
/// by the real `attached_vm_id`/`attached_vm_name` pair.
#[tokio::test]
async fn volume_list_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_volume("vol-1").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    volume::execute(&client, volume::VolumeCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl volume list against POST /v1/volumes");

    let items = list_items(&client, "/v1/volumes").await;
    assert_columns_present(
        &items,
        &[
            "volume_id",
            "name",
            "size",
            "status",
            "attached_vm_id",
            "attached_vm_name",
        ],
    );
}

/// `chvctl volume snapshot` — GREEN; `snapshot_name` field name (#373's
//  fix) forwarded to the mutation service.
#[tokio::test]
async fn volume_snapshot_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_volume("vol-1").await;
    let token = h.seed_jwt_as("operator").await;

    volume::execute(
        &h.client(Some(token)),
        volume::VolumeCommands::Snapshot {
            volume_id: "vol-1".to_string(),
            name: "snap-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl volume snapshot against POST /v1/volumes/snapshot");
    h.mutations.assert_recorded("snapshot_volume:vol-1:snap-1");
}

/// `chvctl volume clone` — GREEN; `source_volume_id`/`target_volume_id`
/// field names (#373's fix) forwarded to the mutation service.
#[tokio::test]
async fn volume_clone_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_volume("vol-1").await;
    let token = h.seed_jwt_as("operator").await;

    volume::execute(
        &h.client(Some(token)),
        volume::VolumeCommands::Clone {
            volume_id: "vol-1".to_string(),
            name: "vol-2".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl volume clone against POST /v1/volumes/clone");
    h.mutations.assert_recorded("clone_volume:vol-1:vol-2");
}

/// `chvctl volume create <name> --node --size [--storage-class]` —
/// GREEN (#513 DP9, PR 3 of the adopted decomposition). The flags map
/// to the BFF volume-create contract fields `name`/`node_id`/
/// `capacity_bytes`/`storage_class`; the node's inventory advertises
/// `["local"]`, so the accept-time capability check accepts the
/// offered class. The `--size` input is bytes-denominated ("1G" =
/// 1073741824 — deliberately unlike `vm create`'s GiB-valued
/// `--disk-size-gb`), and the `--storage-class` input deliberately
/// carries a trailing space: the Create arm trims the flag value
/// before validating and before sending (the #519 discipline, parity
/// with `vm create --storage-class` and the BFF's trim-before-check),
/// and this row is that trim's end-to-end pin — dropping the arm's
/// trim flips this row red client-side. The journaled row shapes are
/// asserted directly (the BFF suite's `volume_create_route.rs`
/// discipline, contract-tier): this create is BFF-direct journaling
/// (DP1), not a mutation-service forward.
#[tokio::test]
async fn volume_create_row() {
    let h = Harness::start().await;
    h.seed_node_with_storage_classes("n-local", &["local"])
        .await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    volume::execute(
        &client,
        volume::VolumeCommands::Create {
            name: "contract-vol".to_string(),
            node: "n-local".to_string(),
            size: "1G".to_string(),
            // Trailing whitespace ON PURPOSE — see the doc comment: the
            // trimmed value ("local") is what must reach the wire and
            // the journaled volume row.
            storage_class: Some("local ".to_string()),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl volume create against POST /v1/volumes/create");

    // The volume actually landed (route + field names accepted
    // end-to-end) with its server-minted volume_id — the id the
    // create response carries, re-fetched through the list route.
    let items = list_items(&client, "/v1/volumes").await;
    let vol = items
        .iter()
        .find(|i| i.get("name").and_then(Value::as_str) == Some("contract-vol"))
        .expect("created volume is visible in volume list");
    let volume_id = vol
        .get("volume_id")
        .and_then(Value::as_str)
        .expect("volume list serves volume_id")
        .to_string();
    assert!(
        !volume_id.is_empty(),
        "the volume id is server-minted, never client-supplied"
    );
    assert_eq!(
        vol.get("node_id").and_then(Value::as_str),
        Some("n-local"),
        "node_id must round-trip — a BFF rename would 400 as missing"
    );

    // volumes row: owner stamped with claims.sub (the #386 lesson — an
    // unstamped volume is admin-only via require_volume_owner), the
    // bytes-denominated capacity verbatim, the TRIMMED class (the
    // trim pin), and volume_kind 'data' (DP8).
    let volume: (String, i64, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT owner_id, capacity_bytes, storage_class, volume_kind FROM volumes WHERE volume_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&h.pool)
    .await
    .expect("volume row");
    assert_eq!(
        volume.0, "u-operator",
        "owner_id must be stamped with claims.sub (the #386 lesson)"
    );
    assert_eq!(
        volume.1, 1073741824,
        "capacity_bytes must round-trip (1G = 1073741824 bytes, the bytes-denominated contract)"
    );
    assert_eq!(
        volume.2.as_deref(),
        Some("local"),
        "the trimmed storage_class must round-trip onto the volume row"
    );
    assert_eq!(
        volume.3.as_deref(),
        Some("data"),
        "volume_kind must be stamped 'data' (DP8)"
    );

    // volume_desired_state row: born standalone — Pending, NULL
    // attached_vm_id (DP4), requested by the creator.
    let vds: (String, Option<String>, String) = sqlx::query_as(
        "SELECT desired_status, attached_vm_id, requested_by FROM volume_desired_state WHERE volume_id = ?",
    )
    .bind(&volume_id)
    .fetch_one(&h.pool)
    .await
    .expect("volume desired state row");
    assert_eq!(
        vds.0, "Pending",
        "a fresh create journals a Pending desired state"
    );
    assert_eq!(
        vds.1, None,
        "attached_vm_id must be NULL — v1 creates standalone volumes"
    );
    assert_eq!(vds.2, "u-operator");

    // operations row: the Accepted CreateVolume operation (the PR 1
    // dispatch carrier's producer) with the design's idempotency key.
    let op: (String, String, String) = sqlx::query_as(
        "SELECT operation_type, status, idempotency_key FROM operations WHERE resource_id = ? AND resource_kind = 'volume'",
    )
    .bind(&volume_id)
    .fetch_one(&h.pool)
    .await
    .expect("operation row");
    assert_eq!(op.0, "CreateVolume");
    assert_eq!(op.1, "Accepted");
    assert_eq!(op.2, format!("create-volume-{volume_id}"));
}

/// `chvctl volume create --storage-class lvm` on a node that reports
/// only `["local"]` — the #513 DP5 REJECTION row, the volume twin of
/// `vm_create_storage_class_rejection_row`: `lvm` is a valid shared
/// vocabulary class, so it passes chvctl's client-side check and the
/// 400 comes from the BFF's node-capability check
/// (`node_storage_class_rejection`, the #516 composition) BEFORE the
/// create transaction — nothing is journaled.
#[tokio::test]
async fn volume_create_storage_class_rejection_row() {
    let h = Harness::start().await;
    h.seed_node_with_storage_classes("n-local", &["local"])
        .await;
    let token = h.seed_jwt_as("operator").await;

    let result = volume::execute(
        &h.client(Some(token)),
        volume::VolumeCommands::Create {
            name: "contract-vol-lvm".to_string(),
            node: "n-local".to_string(),
            size: "1G".to_string(),
            storage_class: Some("lvm".to_string()),
        },
        &OutputFormat::Json,
    )
    .await;
    assert_api_error(result, 400);

    // A rejected create journals nothing (every table the create tx
    // writes — the `volume_create_route.rs` discipline).
    for table in ["volumes", "volume_desired_state", "operations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&h.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected create must not journal {table}");
    }
}

/// `chvctl volume create` with a blank `--node` — the #513 DP3
/// required-node 400. The flag itself is clap-required (a node-less
/// invocation fails at argument parsing, so the CLI never sends a
/// node-less create), which is why this row pins the SERVER-side half
/// through the blank value the flag can still carry: the BFF trims
/// `node_id` and rejects empty with 400 `missing node_id` — there is
/// no first-enrolled-node default for storage placement (silent
/// placement of storage is worse than silent placement of a VM).
#[tokio::test]
async fn volume_create_requires_node_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    let token = h.seed_jwt_as("operator").await;

    let result = volume::execute(
        &h.client(Some(token)),
        volume::VolumeCommands::Create {
            name: "contract-vol-nonode".to_string(),
            // Whitespace ON PURPOSE — the BFF trims and rejects; an
            // untrimmed-but-nonempty id would instead journal (and
            // fail at dispatch), which is a different row's story.
            node: "   ".to_string(),
            size: "1G".to_string(),
            storage_class: None,
        },
        &OutputFormat::Json,
    )
    .await;
    assert_api_error(result, 400);

    // The 400 fires before the transaction — nothing is journaled.
    for table in ["volumes", "volume_desired_state", "operations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&h.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected create must not journal {table}");
    }
}

/// `chvctl volume delete <volume_id>` — GREEN (#522 DP11, PR 3 of the
/// adopted decomposition). The command's request shape is the route's
/// whole contract: verb-POST `/v1/volumes/delete` with the single
/// `{"volume_id"}` body (the DP11 shape, `vm delete`'s twin) — a
/// renamed field or path 400s/404s through the command's own wire
/// path, which is what makes the row discriminating against pre-PR
/// code (there was no `Delete` arm to drive). The journaled row shapes
/// are asserted directly (the `volume_delete_route.rs` discipline,
/// contract-tier — this delete is BFF-direct journaling, DP1): the
/// tombstone, the retained `volumes` row, and the Accepted
/// `DeleteVolume` operation keyed `delete-volume-{volume_id}`. The
/// response's five keys are pinned through the #406 idempotent
/// re-post (the command only prints; the row re-drives the exact body
/// the command sent and inspects the replayed outcome), and the
/// failed-task story is pinned end-to-end: the accepted task flipped
/// to a terminal failure with a recorded cause surfaces that cause
/// through `task watch` (#530) with the unchanged non-zero exit.
#[tokio::test]
async fn volume_delete_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_data_volume("vol-del", None).await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    volume::execute(
        &client,
        volume::VolumeCommands::Delete {
            volume_id: "vol-del".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl volume delete against POST /v1/volumes/delete");

    // The tombstone (DP1): desired_status flipped to 'Deleting', the
    // generation bumped 1 → 2, the operator stamped as updated_by.
    let vds: (i64, String, String) = sqlx::query_as(
        "SELECT desired_generation, desired_status, updated_by FROM volume_desired_state WHERE volume_id = ?",
    )
    .bind("vol-del")
    .fetch_one(&h.pool)
    .await
    .expect("volume desired state row");
    assert_eq!(
        vds.0, 2,
        "the tombstone must bump the generation exactly once"
    );
    assert_eq!(vds.1, "Deleting", "the tombstone must journal 'Deleting'");
    assert_eq!(
        vds.2, "u-operator",
        "the tombstone must stamp the requester as updated_by"
    );

    // The `volumes` row is RETAINED (DP1 — claim-time class resolution
    // and clone replay depend on living rows; the tombstone is the
    // only delete).
    let volumes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM volumes WHERE volume_id = 'vol-del'")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(volumes, 1, "a delete must not remove the volumes row");

    // The Accepted DeleteVolume operation (the PR 1 dispatch carrier's
    // producer) with the design's idempotency key.
    let op: (String, String, String, String) = sqlx::query_as(
        "SELECT operation_id, operation_type, status, idempotency_key FROM operations WHERE resource_kind = 'volume' AND resource_id = ?",
    )
    .bind("vol-del")
    .fetch_one(&h.pool)
    .await
    .expect("operation row");
    let (operation_id, operation_type, operation_status, idempotency_key) = op;
    assert_eq!(operation_type, "DeleteVolume");
    assert_eq!(operation_status, "Accepted");
    assert_eq!(idempotency_key, "delete-volume-vol-del");

    // The response shape (the create route's five keys) — pinned
    // through the #406 idempotent re-post of the exact body the
    // command sent: a retried delete replays the recorded outcome,
    // which is the response the command prints on a retry (and the
    // same five keys the fresh accept carries).
    let retry = client
        .post(
            "/v1/volumes/delete",
            &serde_json::json!({ "volume_id": "vol-del" }),
        )
        .await
        .expect("idempotent re-post of the delete through the harness server");
    assert_eq!(
        retry.get("accepted").and_then(Value::as_bool),
        Some(true),
        "the delete response must carry accepted"
    );
    assert_eq!(
        retry.get("task_id").and_then(Value::as_str),
        Some(operation_id.as_str()),
        "the delete response must carry the journaled operation as task_id"
    );
    assert_eq!(
        retry.get("volume_id").and_then(Value::as_str),
        Some("vol-del"),
        "the delete response must round-trip volume_id"
    );
    assert!(
        retry
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "the delete response must carry a summary"
    );
    assert_eq!(
        retry.get("next_refresh_path").and_then(Value::as_str),
        Some(format!("/api/v1/tasks/{operation_id}").as_str()),
        "the delete response must carry the task's next_refresh_path"
    );
    // The replay re-journals nothing: still exactly one DeleteVolume
    // operation, still one tombstone (generation 2, not 3).
    let delete_ops: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM operations WHERE operation_type = 'DeleteVolume' AND resource_id = 'vol-del'",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(delete_ops, 1, "an idempotent retry must not re-journal");

    // The failed-task story (#530, the DP11 taskwatch ergonomics): the
    // accepted task flipped to a terminal failure with a journaled
    // cause — the shape the orchestrator writes on a refused dispatch
    // — must surface that cause through `chvctl task watch <task_id>`
    // (the task_id the delete response carried) with the unchanged
    // non-zero exit.
    sqlx::query(
        "UPDATE operations SET status = 'Failed', error_code = ?, error_message = ? WHERE operation_id = ?",
    )
    .bind("AGENT_REJECTED")
    .bind("destroy_volume refused: an open stord session exists for volume vol-del")
    .bind(&operation_id)
    .execute(&h.pool)
    .await
    .expect("flip the delete operation to a terminal failure");
    let watch = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        task::execute(
            &client,
            task::TaskCommands::Watch {
                task_id: operation_id.clone(),
                timeout: 20,
            },
            &OutputFormat::Json,
        ),
    )
    .await
    .expect("task watch must complete, not hang");
    match watch {
        Err(CliError::Task(msg)) => assert!(
            msg.contains("Failed"),
            "the non-zero exit must still name the terminal status: {msg}"
        ),
        other => panic!("expected the terminal-failure non-zero exit, got {other:?}"),
    }
    // The cause line the watch prints: the detail fetched through the
    // same client carries the journaled cause verbatim, and the exact
    // formatter the command prints through renders it.
    let detail = client
        .post(
            "/v1/tasks/get",
            &serde_json::json!({ "task_id": operation_id }),
        )
        .await
        .expect("task detail through the harness server")
        .get("detail")
        .cloned()
        .expect("tasks/get nests the payload under \"detail\"");
    assert_eq!(
        task::failure_cause(&detail).as_deref(),
        Some("AGENT_REJECTED — destroy_volume refused: an open stord session exists for volume vol-del"),
        "the delete's failed task must surface the journaled cause through task watch"
    );
}

/// `chvctl volume delete` on an ATTACHED volume — the #522 DP5
/// REJECTION row, the delete twin of
/// `volume_create_storage_class_rejection_row`: the CLI has no flags
/// that could bypass the guard (deliberately — no `--force`, DP5's
/// letter), so the 400 comes from the BFF's in-tx attached guard and
/// the row pins the CLI's failure-mode rendering: the loud message —
/// naming the
/// detach-first path — surfaces verbatim through the command's error
/// path, and nothing is journaled (the zero-journal discipline).
#[tokio::test]
async fn volume_delete_attached_rejection_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    // An ACTIVE VM — the attachment counts (the DP5 refinement only
    // spares an attachment to a VM whose own desired_status is
    // 'Deleting').
    h.seed_vm("vm-1").await;
    h.seed_vm_desired_state("vm-1").await;
    h.seed_data_volume("vol-att", Some("vm-1")).await;
    let token = h.seed_jwt_as("operator").await;

    let result = volume::execute(
        &h.client(Some(token)),
        volume::VolumeCommands::Delete {
            volume_id: "vol-att".to_string(),
        },
        &OutputFormat::Json,
    )
    .await;
    match &result {
        Err(CliError::Api { status, message }) => {
            assert_eq!(*status, 400, "the attached guard rejects with 400");
            assert!(
                message.contains("detach it via POST /v1/volumes/mutate"),
                "the 400 the CLI surfaces must name the detach-first path loudly: {message}"
            );
        }
        other => panic!("expected the attached-volume 400, got {other:?}"),
    }

    // A rejected delete journals nothing (every table the delete tx
    // writes — the `volume_delete_route.rs` discipline): the desired
    // state is untombstoned at its original generation, the volumes
    // row is intact, and no DeleteVolume operation exists.
    let vds: (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status FROM volume_desired_state WHERE volume_id = 'vol-att'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("volume desired state row");
    assert_eq!(
        vds.1.as_deref(),
        Some("Active"),
        "a rejected delete must not tombstone the desired state"
    );
    assert_eq!(vds.0, 1, "a rejected delete must not bump the generation");
    let volumes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM volumes WHERE volume_id = 'vol-att'")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(
        volumes, 1,
        "a rejected delete must not remove the volumes row"
    );
    let delete_ops: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM operations WHERE operation_type = 'DeleteVolume' AND resource_id = 'vol-att'",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(
        delete_ops, 0,
        "a rejected delete must not journal a DeleteVolume operation"
    );
}

/// `chvctl volume delete` on a volume with an IN-FLIGHT operation —
/// the #522 DP7(i) rejection row: an `Accepted` `CreateVolume` (the
/// volume whose create never dispatched, the DP7 case the guard
/// exists to cover) blocks the delete with a 409 whose loud message
/// names the in-flight operation id, surfaced verbatim through the
/// CLI's error path, and nothing is journaled.
#[tokio::test]
async fn volume_delete_in_flight_rejection_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_data_volume("vol-inflight", None).await;
    h.seed_volume_operation(
        "op-create-inflight",
        "vol-inflight",
        "CreateVolume",
        "Accepted",
    )
    .await;
    let token = h.seed_jwt_as("operator").await;

    let result = volume::execute(
        &h.client(Some(token)),
        volume::VolumeCommands::Delete {
            volume_id: "vol-inflight".to_string(),
        },
        &OutputFormat::Json,
    )
    .await;
    match &result {
        Err(CliError::Api { status, message }) => {
            assert_eq!(
                *status, 409,
                "the in-flight guard rejects with 409 (a transient condition, not a validation error)"
            );
            assert!(
                message.contains("op-create-inflight"),
                "the 409 the CLI surfaces must name the in-flight operation loudly: {message}"
            );
        }
        other => panic!("expected the in-flight-operation 409, got {other:?}"),
    }

    // A rejected delete journals nothing.
    let vds: (i64, Option<String>) = sqlx::query_as(
        "SELECT desired_generation, desired_status FROM volume_desired_state WHERE volume_id = 'vol-inflight'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("volume desired state row");
    assert_eq!(
        vds.1.as_deref(),
        Some("Active"),
        "a rejected delete must not tombstone the desired state"
    );
    assert_eq!(vds.0, 1, "a rejected delete must not bump the generation");
    let delete_ops: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM operations WHERE operation_type = 'DeleteVolume' AND resource_id = 'vol-inflight'",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(
        delete_ops, 0,
        "a rejected delete must not journal a DeleteVolume operation"
    );
}

// ---------------------------------------------------------------------------
// Rows — network (route/field GREEN; list display-pinned; --vlan pinned)
// ---------------------------------------------------------------------------

/// `chvctl network list` — GREEN. Columns are the keys the BFF serves
/// (#372 DP10/§2.7(a)): the phantom `cidr`/`vlan`/`status` columns were
/// replaced by the real scope/health/exposure/ipam_mode/is_default.
#[tokio::test]
async fn network_list_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_network("net-1").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    network::execute(&client, network::NetworkCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl network list against POST /v1/networks");

    let items = list_items(&client, "/v1/networks").await;
    assert_columns_present(
        &items,
        &[
            "network_id",
            "name",
            "scope",
            "health",
            "exposure",
            "ipam_mode",
            "is_default",
        ],
    );
}

/// `chvctl network create` (no --vlan) — GREEN.
#[tokio::test]
async fn network_create_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    network::execute(
        &client,
        network::NetworkCommands::Create {
            name: "contract-net".to_string(),
            cidr: "10.60.0.0/24".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl network create against POST /v1/networks/create");

    let items = list_items(&client, "/v1/networks").await;
    let net = items
        .iter()
        .find(|i| i.get("name").and_then(Value::as_str) == Some("contract-net"))
        .expect("created network is visible in network list");
    // Optional-field probe (review round 1): `cidr` is optional in
    // create_network and is NOT served by network list (a §2.7(a)
    // phantom column), so a name-only check can't see a BFF-side cidr
    // rename (it would silently default). Fetch the detail route — which
    // DOES serve cidr from network_desired_state — and assert the value
    // round-tripped exactly.
    let network_id = net
        .get("network_id")
        .and_then(Value::as_str)
        .expect("network list serves network_id");
    let detail = client
        .post(
            "/v1/networks/get",
            &serde_json::json!({ "network_id": network_id }),
        )
        .await
        .expect("network detail through the harness server");
    let detail = detail
        .get("detail")
        .expect("networks/get nests the payload under \"detail\"");
    assert_eq!(
        detail.get("cidr").and_then(Value::as_str),
        Some("10.60.0.0/24"),
        "cidr must round-trip — a BFF rename would silently default it"
    );
}

/// `chvctl network create --vlan` — the flag is REMOVED (#372 DP7,
/// design §2.6): the capability does not exist at any layer (no
/// `vlan_id` column, no BFF field, no UI field) and the flag was
/// silently dropped by the server — a flag that does nothing is the
/// exact failure mode #372 names. #517 tracks the real implementation.
/// The pre-PR-2 row pinned the silent drop (request accepted, `vlan`
/// read by nothing); with the field gone from the `Create` variant the
/// row is now a CLI-arg-level assertion — clap rejects `--vlan` as an
/// unknown argument, so the flag cannot silently do nothing ever again.
#[tokio::test]
async fn network_create_vlan_flag_removed_row() {
    // A tiny parser over the real `NetworkCommands` subcommand enum —
    // the same clap derive the binary mounts, driven at the arg level.
    #[derive(clap::Parser)]
    struct NetworkCli {
        #[command(subcommand)]
        command: network::NetworkCommands,
    }

    // The flag no longer parses: clap errors on the unknown argument.
    // (The parser wraps `NetworkCommands` directly, so argv starts at
    // the `create` subcommand.)
    let rejected = NetworkCli::try_parse_from([
        "chvctl",
        "create",
        "contract-vlan-net",
        "--cidr",
        "10.61.0.0/24",
        "--vlan",
        "42",
    ]);
    assert!(
        rejected.is_err(),
        "--vlan must not parse — the flag was removed (#372 DP7); got {rejected:?}",
        rejected = rejected.err().map(|e| e.to_string())
    );

    // And the create without it still parses (the green wire row is
    // `network_create_row` above).
    let accepted =
        NetworkCli::try_parse_from(["chvctl", "create", "contract-net", "--cidr", "10.60.0.0/24"]);
    assert!(accepted.is_ok(), "--vlan-free create must still parse");
}

/// `chvctl network delete` — GREEN.
#[tokio::test]
async fn network_delete_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_network("net-1").await;
    let token = h.seed_jwt_as("operator").await;

    network::execute(
        &h.client(Some(token)),
        network::NetworkCommands::Delete {
            network_id: "net-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl network delete against POST /v1/networks/delete");
}

// ---------------------------------------------------------------------------
// Rows — task (list display-pinned; watch hang-pinned)
// ---------------------------------------------------------------------------

/// `chvctl task list` — GREEN. Columns are the keys the BFF serves
/// (#372 DP10/§2.7(a)): the phantom `type`/`created_at` columns were
/// replaced by the real `operation` and `started_unix_ms`.
#[tokio::test]
async fn task_list_row() {
    let h = Harness::start().await;
    h.seed_operation("op-1").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    task::execute(&client, task::TaskCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl task list against POST /v1/tasks");

    let items = list_items(&client, "/v1/tasks").await;
    assert_columns_present(
        &items,
        &[
            "task_id",
            "status",
            "operation",
            "resource_id",
            "started_unix_ms",
        ],
    );
    // #502: the terminal-failure cause pair is part of the list item
    // shape now — NULL for this row's seeded Succeeded op (the key
    // must exist; the value must not be fabricated), same drift
    // discipline as the display columns above.
    for item in &items {
        assert!(
            item.get("error_code").is_some() && item["error_code"].is_null(),
            "task list items must carry error_code (NULL until a failure records it)"
        );
        assert!(
            item.get("error_message").is_some() && item["error_message"].is_null(),
            "task list items must carry error_message (NULL until a failure records it)"
        );
    }
}

/// `chvctl task watch` — GREEN since the DP6 fix (design §2.5): the
/// command polls the new `POST /v1/tasks/get` (the house /get
/// convention), matches the REAL status vocabulary (capitalized
/// `Succeeded`/`Failed`/… — `OperationStatus`, never the old lowercase
/// `completed`), and is bounded by `--timeout` (default 15 min). The
/// pre-PR-2 row pinned the HANG (the command polled the list route,
/// whose handler ignores `task_id`, and looped forever printing
/// `Status: unknown`); the row now asserts COMPLETION: a seeded
/// terminal task (`Succeeded`) makes the command exit with success,
/// and the poll middleware counted the polls that reached the route.
#[tokio::test]
async fn task_watch_row() {
    let h = Harness::start().await;
    h.seed_operation("op-watch").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    // The timeout wrapper stays (a regression to a hang must still fail
    // the row loudly) — but it must NOT fire: the command completes.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        task::execute(
            &client,
            task::TaskCommands::Watch {
                task_id: "op-watch".to_string(),
                // A short, explicit cap — well above one poll+print
                // cycle, far below the row's outer wrapper.
                timeout: 20,
            },
            &OutputFormat::Json,
        ),
    )
    .await;

    let inner = result.expect("task watch must complete, not hang (design §2.5/DP6)");
    inner.expect("watching a Succeeded task exits with success");
    assert!(
        h.task_polls.load(Ordering::SeqCst) >= 1,
        "at least one poll reached POST /v1/tasks/get"
    );
}

/// `chvctl task watch` on a terminally-FAILED task — GREEN (#502). The
/// BFF's `/v1/tasks/get` detail now carries the journaled
/// terminal-failure cause (`error_code`/`error_message`, the
/// #498/#500 fast-fail columns — NULL until a failure records them),
/// and the watch command prints it (`Cause: <code> — <message>`)
/// before its non-zero exit, so the operator waiting on a failed op
/// sees WHY it failed instead of a bare `Failed`. The row pins both
/// halves end-to-end: the detail fetched through the same client
/// carries the seeded cause verbatim, `task::failure_cause` (the exact
/// formatter the command prints through) renders it, and the command
/// still exits non-zero on the terminal status (the #372 DP6
/// semantics, unchanged).
#[tokio::test]
async fn task_watch_failure_cause_row() {
    let h = Harness::start().await;
    h.seed_operation_ext(
        "op-watch-failed",
        "Failed",
        Some("UNSUPPORTED_BY_AGENT"),
        Some("snapshot_volume is unsupported in core-managed mode"),
    )
    .await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        task::execute(
            &client,
            task::TaskCommands::Watch {
                task_id: "op-watch-failed".to_string(),
                timeout: 20,
            },
            &OutputFormat::Json,
        ),
    )
    .await;

    let inner = result.expect("task watch must complete, not hang");
    // The #372 DP6 exit semantics are unchanged: a terminal failure
    // exits non-zero — the cause line is added context, not a softened
    // exit.
    match inner {
        Err(CliError::Task(msg)) => assert!(
            msg.contains("Failed"),
            "the non-zero exit must still name the terminal status: {msg}"
        ),
        other => panic!("expected the terminal-failure non-zero exit, got {other:?}"),
    }
    assert!(
        h.task_polls.load(Ordering::SeqCst) >= 1,
        "at least one poll reached POST /v1/tasks/get"
    );

    // The row the command printed, re-fetched through the same client
    // (the harness's display-drift discipline): the detail carries the
    // seeded cause verbatim, and the exact formatter the command
    // prints through renders it.
    let detail = client
        .post(
            "/v1/tasks/get",
            &serde_json::json!({ "task_id": "op-watch-failed" }),
        )
        .await
        .expect("task detail through the harness server")
        .get("detail")
        .cloned()
        .expect("tasks/get nests the payload under \"detail\"");
    assert_eq!(
        detail.get("error_code").and_then(Value::as_str),
        Some("UNSUPPORTED_BY_AGENT"),
        "the journaled error_code must surface verbatim — never fabricated"
    );
    assert_eq!(
        detail.get("error_message").and_then(Value::as_str),
        Some("snapshot_volume is unsupported in core-managed mode"),
        "the agents' refusal text must surface verbatim"
    );
    assert_eq!(
        task::failure_cause(&detail).as_deref(),
        Some("UNSUPPORTED_BY_AGENT — snapshot_volume is unsupported in core-managed mode"),
        "the watch command's cause line must render code + refusal text"
    );
}

// ---------------------------------------------------------------------------
// Rows — user (list/create GREEN; delete 400-pinned)
// ---------------------------------------------------------------------------

/// `chvctl user list` — GREEN (admin tier). Columns match
/// `handlers/users.rs` — `user_id` is included so the delete
/// contract's key is discoverable (#372 DP2).
#[tokio::test]
async fn user_list_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("admin").await;
    let client = h.client(Some(token));

    user::execute(&client, user::UserCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl user list against POST /v1/users");

    let items = list_items(&client, "/v1/users").await;
    assert_columns_present(&items, &["user_id", "username", "role", "created_at"]);
}

/// `chvctl user create` — GREEN (admin tier).
#[tokio::test]
async fn user_create_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("admin").await;

    user::execute(
        &h.client(Some(token)),
        user::UserCommands::Create {
            username: "contract-user".to_string(),
            password: "contract-password-123".to_string(),
            role: "viewer".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl user create against POST /v1/users/create");
}

/// `chvctl user delete` — GREEN since the DP2 fix (design §2.1): the
/// positional arg is `user_id` and the body carries `user_id`, the
/// field the BFF's delete handler requires (the pre-PR-2 command sent
/// `username`, which the handler rejected with 400 `missing user_id`
/// on every invocation — the command had never worked, so there was no
/// compat surface to break). The row asserts the delete actually works
/// end-to-end: seed a user, delete by user_id, verify it is gone from
/// the user list.
#[tokio::test]
async fn user_delete_row() {
    let h = Harness::start().await;
    h.seed_user("u-victim", "victim").await;
    let token = h.seed_jwt_as("admin").await;
    let client = h.client(Some(token));

    user::execute(
        &client,
        user::UserCommands::Delete {
            user_id: "u-victim".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl user delete against POST /v1/users/delete");

    let items = list_items(&client, "/v1/users").await;
    assert!(
        !items
            .iter()
            .any(|i| i.get("user_id").and_then(Value::as_str) == Some("u-victim")),
        "deleted user must be gone from the user list"
    );
}

// ---------------------------------------------------------------------------
// Rows — migrate group (all GREEN since the PR 4 repoint; design
// §2.3/DP4 + DP4b)
// ---------------------------------------------------------------------------

/// `chvctl migrate start` — GREEN since the PR 4 repoint (design
/// §2.3/DP4): the command drives the vm-mutate migrate path —
/// `POST /v1/vms/mutate` with `{vm_id, action:"migrate",
/// target_node_id}` — the exact path `chvctl vm migrate` already drove
/// (the pre-fix command called `POST /v1/migrations` with
/// `{vm_id, target_node}`, a route and field that never existed).
/// Forwarded to the mutation service like `vm migrate`; issue #656 makes
/// pause-first mandatory.
#[tokio::test]
async fn migrate_start_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    migrate::execute(
        &h.client(Some(token)),
        migrate::MigrateCommands::Start {
            vm_id: "vm-1".to_string(),
            target_node: "n-2".to_string(),
            pause_first: true,
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl migrate start --pause-first against POST /v1/vms/mutate");
    h.mutations
        .assert_recorded("migrate_vm:vm-1:n-2:pause_first");
}

/// Issue #656: `chvctl migrate start` without `--pause-first` fails
/// closed client-side, before any HTTP request is recorded.
#[tokio::test]
async fn migrate_start_without_pause_first_is_rejected_locally() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    let token = h.seed_jwt_as("operator").await;

    let err = migrate::execute(
        &h.client(Some(token)),
        migrate::MigrateCommands::Start {
            vm_id: "vm-1".to_string(),
            target_node: "n-2".to_string(),
            pause_first: false,
        },
        &OutputFormat::Json,
    )
    .await
    .expect_err("the live migration path must fail closed");
    match err {
        CliError::Usage(msg) => assert!(
            msg.contains("#394") && msg.contains("--pause-first"),
            "the local rejection must explain the gate and remedy: {msg}"
        ),
        other => panic!("expected a usage error, got {other:?}"),
    }
    assert!(
        h.mutations.calls().is_empty(),
        "no HTTP mutation may be recorded for a locally rejected live migration: {:?}",
        h.mutations.calls()
    );
}

/// `chvctl migrate status` — GREEN since the PR 4 repoint (design
/// §2.3/DP4b): the command's `GET /v1/migrations/{id}` target finally
/// exists as a viewer-tier read route over the real `migrations`
/// table (pinned server-side by
/// `crates/chv-webui-bff/tests/migrations_read_routes.rs`).
#[tokio::test]
async fn migrate_status_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    h.seed_migration("mig-1", "PreCopyDisk").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    migrate::execute(
        &client,
        migrate::MigrateCommands::Status {
            migration_id: "mig-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl migrate status against GET /v1/migrations/{id}");

    // The row the command printed, re-fetched through the same client:
    // the migrations table's own column names.
    let resp = client
        .get("/v1/migrations/mig-1")
        .await
        .expect("migration detail through the harness server");
    assert_eq!(
        resp.get("migration_id").and_then(Value::as_str),
        Some("mig-1")
    );
    assert_eq!(
        resp.get("phase").and_then(Value::as_str),
        Some("PreCopyDisk")
    );
}

/// `chvctl migrate cancel` — GREEN since the PR 4 repoint (design
/// §2.3/DP4): the command calls the CP admin-tier
/// `POST /admin/migrations/{id}/cancel` — which this harness has
/// served since PR 1 for exactly this row — with an admin token (an
/// operator token would 403). The cancel is cooperative and
/// best-effort; the row asserts it actually landed: the flag column
/// the migration loop polls is set on the seeded (non-terminal)
/// migration.
#[tokio::test]
async fn migrate_cancel_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    h.seed_migration("mig-1", "PreCopyDisk").await;
    let token = h.seed_jwt_as("admin").await;

    migrate::execute(
        &h.client(Some(token)),
        migrate::MigrateCommands::Cancel {
            migration_id: "mig-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl migrate cancel against POST /admin/migrations/{id}/cancel");

    let flagged: Option<String> = sqlx::query_scalar(
        "SELECT cancel_requested_at FROM migrations WHERE migration_id = 'mig-1'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("seeded migration row");
    assert!(
        flagged.is_some(),
        "the cancel request must set the flag the migration loop polls"
    );
}

/// `chvctl migrate list` — GREEN since the PR 4 repoint (design
/// §2.3/DP4b): the viewer-tier `GET /v1/migrations` list route; the
/// columns are the migrations table's own names (the pre-fix command
/// read a `migrations` key the BFF never served and printed phantom
/// `source_node`/`target_node`/`status`/`progress` columns).
#[tokio::test]
async fn migrate_list_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    h.seed_vm("vm-1").await;
    h.seed_migration("mig-1", "PreCopyDisk").await;
    h.seed_migration("mig-2", "ConvergingDisk").await;
    let token = h.seed_jwt_as("operator").await;
    let client = h.client(Some(token));

    migrate::execute(&client, migrate::MigrateCommands::List, &OutputFormat::Json)
        .await
        .expect("chvctl migrate list against GET /v1/migrations");

    let items = get_items(&client, "/v1/migrations").await;
    assert_columns_present(
        &items,
        &[
            "migration_id",
            "vm_id",
            "source_node_id",
            "destination_node_id",
            "phase",
            "cancel_requested",
        ],
    );
}

// ---------------------------------------------------------------------------
// Rows — health (all GREEN at main; fixed under #320)
// ---------------------------------------------------------------------------

/// `chvctl health check` — GREEN (`GET /v1/health`).
#[tokio::test]
async fn health_check_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("operator").await;

    health::execute(
        &h.client(Some(token)),
        health::HealthCommands::Check,
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl health check against GET /v1/health");
}

/// `chvctl health cluster` — GREEN (`GET /v1/cluster/health`).
#[tokio::test]
async fn health_cluster_row() {
    let h = Harness::start().await;
    let token = h.seed_jwt_as("operator").await;

    health::execute(
        &h.client(Some(token)),
        health::HealthCommands::Cluster,
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl health cluster against GET /v1/cluster/health");
}

/// `chvctl health report <node>` — GREEN (`GET /v1/nodes/{id}/health`).
#[tokio::test]
async fn health_report_row() {
    let h = Harness::start().await;
    h.seed_node().await;
    let token = h.seed_jwt_as("operator").await;

    health::execute(
        &h.client(Some(token)),
        health::HealthCommands::Report {
            node_id: "n-1".to_string(),
        },
        &OutputFormat::Json,
    )
    .await
    .expect("chvctl health report against GET /v1/nodes/{id}/health");
}
