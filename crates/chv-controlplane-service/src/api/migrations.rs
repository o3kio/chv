//! Admin API for migration operations.
//!
//! Currently exposes a single endpoint: `POST /admin/migrations/{id}/cancel`,
//! which sets the cooperative cancel flag for an in-flight migration. The
//! migration loop polls the flag at safe points and rolls back when it fires.
//!
//! See [`crate::migration::request_migration_cancel`] for the underlying
//! semantics, including idempotency and terminal-phase no-op behavior.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chv_webui_bff::AppState;

use crate::migration::{request_migration_cancel, CancelMigrationOutcome};

/// Request a cooperative cancel for an in-flight migration.
///
/// Returns 202 Accepted regardless of whether the cancel will actually be
/// honored — the cancel is best-effort by design (it must be observed at a
/// safe point by the migration loop). The response body indicates the outcome
/// so callers can distinguish a fresh request from a no-op.
pub async fn cancel_migration(
    Path(migration_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match request_migration_cancel(&state.pool, &migration_id).await {
        Ok(CancelMigrationOutcome::Requested) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "migration_id": migration_id,
                "outcome": "requested",
                "message": "cancel flag set; migration will roll back at next safe point"
            })),
        ),
        Ok(CancelMigrationOutcome::AlreadyRequested) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "migration_id": migration_id,
                "outcome": "already_requested",
                "message": "cancel was already requested; this call is a no-op"
            })),
        ),
        Ok(CancelMigrationOutcome::AlreadyTerminal) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "migration_id": migration_id,
                "outcome": "already_terminal",
                "message": "migration is already terminal; Completed is a no-op, Failed/RolledBack queue an agent-side cancel for any surviving detached task"
            })),
        ),
        Ok(CancelMigrationOutcome::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "migration_id": migration_id,
                "outcome": "not_found",
                "message": "no migration with the given id"
            })),
        ),
        Err(e) => {
            tracing::error!(
                migration_id = %migration_id,
                error = %e,
                "cancel_migration failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "migration_id": migration_id,
                    "outcome": "error",
                    "message": "failed to set cancel flag"
                })),
            )
        }
    }
}
