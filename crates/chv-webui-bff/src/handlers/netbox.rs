//! NetBox projection BFF handlers — issue #239, PR 5 of 8.
//!
//! Implements the eight POST-only endpoints of
//! `docs/specs/architecture-designer/contracts/netbox-api-contract.md`:
//! config get/upsert/delete, dry-run, export, runs list/get/retry. The
//! contract is the binding spec for every wire shape, error code, and
//! event emitted here.
//!
//! # Path style
//!
//! Same POST-only verb-path convention as every other handler in this
//! crate (see the note in [`crate::handlers::architectures`]):
//! `/v1/architectures/netbox/<verb>`.
//!
//! # Roles
//!
//! All eight endpoints are mounted under the **operator** middleware
//! (the contract: Viewer has no access to any NetBox projection
//! endpoint). Two data-level escalations happen inside the handlers:
//!
//! - **Production export** (and every other mutating config / retry
//!   action against a production-tagged topology) escalates to Admin
//!   with `PRODUCTION_REQUIRES_ADMIN`, mirroring the apply/destroy rule
//!   via [`enforce_production_guard`]. Config reads, dry-run, and run
//!   history stay operator-accessible on production rows (the contract
//!   escalates *export* only; reads follow the architecture read
//!   conventions).
//! - **`delete` retention policy** requires Admin (contract, config
//!   upsert section). The contract defines no dedicated code for that
//!   refusal, so it surfaces as a plain 403 `FORBIDDEN` — the same flat
//!   shape every other plain 403 in this crate uses — with an explicit
//!   message. Documented deviation-by-choice; see
//!   [`netbox_config_upsert`].
//!
//! # Ownership (Security H6)
//!
//! Reads (config/get, dry-run, runs/list, runs/get) use the architecture
//! read conventions — [`get_topology_authorized`] only: system-owned
//! starter rows stay readable, foreign rows answer 403, missing rows
//! 404. Writes (config/upsert, config/delete, export, runs/retry)
//! additionally pass [`require_owner_or_admin`], exactly like
//! update/archive/apply on the same resource.
//!
//! # Secrets
//!
//! The NetBox API token is write-only. It is accepted on config/upsert,
//! handed to the store for encryption, and **never** returned, logged,
//! or embedded in an error or event. Config responses carry
//! `token_set: bool` only. Dry-run decrypts the token in-memory
//! (fail-closed: an undecryptable ciphertext answers 400
//! `NETBOX_TOKEN_MISSING`, never the ciphertext) and feeds it to
//! [`NetBoxToken`], whose `Debug`/`Display` are redacted.
//!
//! # Dry-run vs export
//!
//! Dry-run runs synchronously on the caller's request thread, bounded by
//! the NetBox client's request timeout (the plan's accepted trade-off).
//! Export only **enqueues** a run row (trigger `manual`, mode `export`);
//! the PR-4 projection worker picks it up — the BFF never executes an
//! export inline.

use axum::{extract::State, Json};
use chv_architecture_validate::model::CHVArchitecture;
use chv_controlplane_store::{
    is_active_run_conflict, EventAppendInput, NetboxProjectionConfigUpsertInput,
    NetboxProjectionRunCreateInput, StoreError, VersionRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureId, ArchitectureVersionId, NetboxProjectionConfig, NetboxProjectionMode,
    NetboxProjectionRun, NetboxProjectionRunId, NetboxProjectionRunStatus, NetboxProjectionTrigger,
    NetboxRetentionPolicy, RunStatus,
};
use chv_controlplane_types::domain::{EventSeverity, EventType};
use chv_netbox_adapter::{
    ownership::CustomFieldNames, plan::RetentionPolicy, ClientError, NetBoxClient, NetBoxToken,
    NetboxProjectionInput, NetboxProjectionRunner, RunnerError, DEFAULT_CUSTOM_FIELD_PREFIX,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{require_operator_or_admin, BearerToken, Claims, Role};
use crate::handlers::architectures::{
    enforce_production_guard, get_topology_authorized, parse_id, require_owner_or_admin,
};
use crate::router::AppState;
use crate::BffError;

/// Audit event names owned by the BFF (the worker owns
/// `architecture_netbox_export_succeeded` / `_failed`). Emitted as
/// `EventType::Audit` with the event name as the message and structured,
/// secret-free details — per the contract's events section.
pub const EVENT_NETBOX_CONFIG_UPDATED: &str = "architecture_netbox_config_updated";
pub const EVENT_NETBOX_DRY_RUN: &str = "architecture_netbox_dry_run";
pub const EVENT_NETBOX_EXPORT_RETRIED: &str = "architecture_netbox_export_retried";

/// Default page size for runs/list (contract: `limit: 20`).
const RUNS_LIST_DEFAULT_LIMIT: i64 = 20;
/// Upper bound for runs/list. The contract does not fix a cap; the value
/// is clamped (not rejected) so an oversized request still succeeds —
/// documented choice, consistent with how list endpoints elsewhere in
/// this crate bound their result sets.
const RUNS_LIST_MAX_LIMIT: i64 = 100;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Body for `POST /v1/architectures/netbox/config/get` (and the `{id}`
/// body shared by dry-run and export).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxConfigGetRequest {
    pub id: String,
}

/// Config summary — the contract's config/get / config/upsert response.
/// `token_set` is the only token signal on the wire: the row's
/// `token_ciphertext` column is NOT NULL, so a present row always means
/// a secret was stored (`token_set: true`). Whether that ciphertext
/// still decrypts is only knowable on the decrypt path (dry-run /
/// worker), which fails closed with `NETBOX_TOKEN_MISSING`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxConfigResponse {
    pub architecture_id: String,
    pub endpoint: String,
    pub token_secret_ref: String,
    pub token_set: bool,
    pub retention_policy: NetboxRetentionPolicy,
    pub enable_post_apply: bool,
    pub custom_field_prefix: String,
    pub site_name: Option<String>,
    pub updated_at: String,
}

impl From<NetboxProjectionConfig> for NetboxConfigResponse {
    fn from(c: NetboxProjectionConfig) -> Self {
        Self {
            architecture_id: c.architecture_id.into_inner(),
            endpoint: c.endpoint,
            token_secret_ref: c.token_secret_ref,
            // The config row always carries a ciphertext (NOT NULL
            // column), so row-present == token-set. See the struct doc.
            token_set: true,
            retention_policy: c.retention_policy,
            enable_post_apply: c.enable_post_apply,
            custom_field_prefix: c.custom_field_prefix,
            site_name: c.site_name,
            updated_at: c.updated_at.to_rfc3339(),
        }
    }
}

/// Body for `POST /v1/architectures/netbox/config/upsert`.
///
/// `token` is optional on update (omitted/null keeps the existing
/// secret). `retention_policy` is a plain string (not the typed enum) so
/// an invalid value answers the designer's flat 400 shape instead of
/// axum's serde rejection.
///
/// `custom_field_prefix` is an additive optional field: the contract's
/// upsert example omits it, but the config summary returns it, so it
/// must remain manageable. Omitted → keep the existing prefix on
/// update, `chv_` default on create (documented extension, same
/// keep-when-omitted semantics as `token`).
///
/// Deliberately **not** `derive(Debug)`: the struct carries the
/// plaintext token; the manual impl below redacts it so no `{:?}`
/// formatting (log lines, panic messages, test failures) can leak it.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxConfigUpsertRequest {
    pub id: String,
    /// Version of the topology row the client read; mismatch answers
    /// 409 `PLAN_EXPIRED` (the contract reuses the topology
    /// optimistic-concurrency rule of `/v1/architectures/update`).
    pub expected_version: i64,
    pub endpoint: String,
    #[serde(default)]
    pub token: Option<String>,
    pub token_secret_ref: String,
    /// `mark_stale` | `delete` (`delete` requires Admin).
    pub retention_policy: String,
    pub enable_post_apply: bool,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub custom_field_prefix: Option<String>,
}

impl std::fmt::Debug for NetboxConfigUpsertRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetboxConfigUpsertRequest")
            .field("id", &self.id)
            .field("expected_version", &self.expected_version)
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("token_secret_ref", &self.token_secret_ref)
            .field("retention_policy", &self.retention_policy)
            .field("enable_post_apply", &self.enable_post_apply)
            .field("site_name", &self.site_name)
            .field("custom_field_prefix", &self.custom_field_prefix)
            .finish()
    }
}

/// Body for `POST /v1/architectures/netbox/config/delete`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxConfigDeleteRequest {
    pub id: String,
}

/// Response for `POST /v1/architectures/netbox/config/delete`. The
/// contract does not fix the shape; `deleted: true` mirrors the
/// idempotent-delete convention used by discard-plan's `{status}`
/// response. An absent config answers 404 `NETBOX_NOT_CONFIGURED`
/// (same code as config/get — see the contract's error table, which
/// allows 404 for the code).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxConfigDeleteResponse {
    pub deleted: bool,
}

/// Body for `POST /v1/architectures/netbox/export` (same `{id}` shape as
/// config/get and dry-run).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxExportRequest {
    pub id: String,
}

/// Response for `POST /v1/architectures/netbox/export` — the contract's
/// enqueue acknowledgement. The worker (not the BFF) executes the run.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxExportResponse {
    pub run_id: String,
    pub architecture_id: String,
    pub status: String,
}

/// Response for `POST /v1/architectures/netbox/export/dry-run` — the
/// contract's deterministic, secret-free plan shape
/// (`mapping_version`, `architecture_id`, `architecture_version`,
/// `retention`, `summary`, `entries`). The adapter's
/// [`NetboxProjectionPlan`] serializes exactly that shape (plus the
/// `retention` field the plan type documents as must-not-drop), so it is
/// flattened verbatim rather than re-mapped field-by-field.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetboxDryRunResponse {
    #[serde(flatten)]
    pub plan: chv_netbox_adapter::NetboxProjectionPlan,
}

/// Body for `POST /v1/architectures/netbox/runs/list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxRunsListRequest {
    pub id: String,
    /// Optional page size; default 20, clamped to 100 (see
    /// [`RUNS_LIST_MAX_LIMIT`]).
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Response for `POST /v1/architectures/netbox/runs/list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxRunsListResponse {
    pub runs: Vec<NetboxRunSummaryDto>,
}

/// Run summary for runs/list — the contract's "id, trigger, status,
/// mode, summary, error_message, timestamps" (RFC3339 strings, like
/// [`crate::handlers::architectures::ApplyRunDto`]). `summary` is the
/// parsed `summary_json` when present. Never token material.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxRunSummaryDto {
    pub id: String,
    pub architecture_id: String,
    pub trigger: NetboxProjectionTrigger,
    pub status: NetboxProjectionRunStatus,
    pub mode: NetboxProjectionMode,
    pub summary: Option<Value>,
    pub error_message: Option<String>,
    pub attempt_count: i64,
    pub requested_by: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
}

/// Body for `POST /v1/architectures/netbox/runs/get`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxRunGetRequest {
    pub id: String,
    pub run_id: String,
}

/// Full run for runs/get — the summary fields plus `plan_json` and
/// `result_json`, parsed to JSON values when parseable (raw string
/// otherwise, per the contract) and the version/bookkeeping columns the
/// UI needs to render history.
///
/// # Result-envelope unwrap
///
/// The projection worker does not persist the adapter's outcome
/// verbatim: it wraps it in a provenance envelope
/// `{ "resolved_architecture_version_id": …, "result": … }`
/// (`NetboxProjectionWorker::result_envelope`, recording the version
/// that was actually projected). The API contract's runs/get serves the
/// per-entry outcome, so [`run_detail_dto`] unwraps the envelope before
/// serving: `result_json` carries the inner `result` (the flat outcome
/// — plan, entries, summary, error) and `resolved_architecture_version_id`
/// surfaces the envelope's version id as a first-class field. It is
/// null for rows without an envelope — legacy rows, raw-string columns,
/// and runs that failed before producing a result. The unwrap is
/// defensive: a parsed object without both envelope keys (a string
/// `resolved_architecture_version_id` and an object `result`) passes
/// through unchanged, so a future envelope shape is served verbatim
/// rather than guessed at.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxRunDetailDto {
    pub id: String,
    pub architecture_id: String,
    pub architecture_version_id: String,
    pub trigger: NetboxProjectionTrigger,
    pub status: NetboxProjectionRunStatus,
    pub mode: NetboxProjectionMode,
    pub plan_json: Option<Value>,
    pub result_json: Option<Value>,
    /// The architecture version the worker actually projected — lifted
    /// out of the result envelope (see the struct doc). Null when the
    /// row carries no envelope.
    pub resolved_architecture_version_id: Option<String>,
    pub summary: Option<Value>,
    pub error_message: Option<String>,
    pub attempt_count: i64,
    pub requested_by: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub next_attempt_at: Option<String>,
    pub created_at: String,
}

/// Body for `POST /v1/architectures/netbox/runs/retry`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxRunRetryRequest {
    pub id: String,
    pub run_id: String,
}

/// Response for `POST /v1/architectures/netbox/runs/retry` (contract
/// shape).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetboxRunRetryResponse {
    pub run_id: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Handlers — config
// ---------------------------------------------------------------------------

/// `POST /v1/architectures/netbox/config/get` — fetch the projection
/// config summary. Token material is never returned (`token_set` only).
///
/// Read semantics: system-owned starter rows stay readable; a foreign
/// row answers 403; a missing row 404; a missing config 404
/// `NETBOX_NOT_CONFIGURED` (contract).
pub async fn netbox_config_get(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxConfigGetRequest>,
) -> Result<Json<NetboxConfigResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    tracing::info!(architecture_id = %id, actor = %claims.sub, "netbox_config_get");

    // Read-convention ownership check (403 foreign / 404 missing) —
    // mirrors get_architecture on the same resource.
    get_topology_authorized(&state, &claims, &id).await?;

    let config =
        state
            .netbox_config
            .get(&id)
            .await?
            .ok_or_else(|| BffError::NetboxNotConfigured {
                architecture_id: id.to_string(),
            })?;
    Ok(Json(NetboxConfigResponse::from(config)))
}

/// `POST /v1/architectures/netbox/config/upsert` — create/update the
/// projection config (and set the token).
///
/// Guards, in order (ownership first so a foreign row answers the plain
/// 403 before any environment-based signal — Security F7):
///
/// 1. operator+ (routing layer, re-checked here),
/// 2. ownership (403 foreign / 404 missing) + system-row write denial,
/// 3. production guard from the *persisted* environment tag (mirrors
///    `update_architecture`),
/// 4. `retention_policy == "delete"` requires Admin — the contract
///    defines no dedicated stable code for this refusal, so it answers
///    the plain flat 403 (`FORBIDDEN`) with an explicit message,
///    matching how other handlers format plain 403s (documented
///    choice),
/// 5. endpoint scheme must be `https://` → 400 `NETBOX_HTTPS_REQUIRED`
///    (the BFF is the accept-time gate; the store persists verbatim),
/// 6. `expected_version` vs. the topology's current version → 409
///    `PLAN_EXPIRED`.
///
/// The token is optional on update (omitted keeps the existing secret —
/// store semantics). The response is the config summary; the
/// `architecture_netbox_config_updated` audit event is emitted
/// best-effort with the changed-field names — never the token.
pub async fn netbox_config_upsert(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxConfigUpsertRequest>,
) -> Result<Json<NetboxConfigResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    tracing::info!(
        architecture_id = %id,
        actor = %claims.sub,
        token_supplied = req.token.is_some(),
        "netbox_config_upsert"
    );

    // Ownership + production guard (persisted tag — an `environment:
    // null` request field cannot bypass it, same reasoning as
    // update_architecture).
    let topo = get_topology_authorized(&state, &claims, &id).await?;
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    enforce_production_guard(topo.environment.as_deref(), role)?;

    // Retention policy: plain string so an invalid value answers the
    // flat 400 instead of axum's serde rejection shape.
    let retention = parse_retention_policy(&req.retention_policy)?;
    // `delete` retention is destructive on the NetBox side — Admin only
    // (contract). Plain 403, see the handler doc.
    if matches!(retention, NetboxRetentionPolicy::Delete) && !role.meets(Role::Admin) {
        return Err(BffError::Forbidden(
            "delete retention policy requires admin".into(),
        ));
    }

    // Accept-time HTTPS gate (case-insensitive scheme check; full URL
    // validity is enforced fail-closed later by NetBoxClient
    // construction on the dry-run/worker paths).
    if !req
        .endpoint
        .trim()
        .to_ascii_lowercase()
        .starts_with("https://")
    {
        return Err(BffError::NetboxHttpsRequired);
    }

    // Optimistic concurrency against the topology row, mirroring
    // /v1/architectures/update's StaleVersion conflict but surfaced with
    // the contract's stable PLAN_EXPIRED code. Note: unlike the topology
    // UPDATE (a SQL-level CAS), this check reads the row loaded above —
    // a topology bump racing between the load and the config write is
    // not additionally blocked; the config row is not part of the
    // topology row, so the contract's "reject stale requests" rule is
    // satisfied at request-granularity.
    if req.expected_version != topo.version_number {
        return Err(BffError::NetboxStaleVersion {
            architecture_id: id.to_string(),
            current: topo.version_number,
            expected: req.expected_version,
        });
    }

    // Custom-field prefix: optional on the wire — omitted keeps the
    // existing value (create default `chv_`), supplied values are
    // validated with the adapter's pure validator.
    let previous = state.netbox_config.get(&id).await?;
    let custom_field_prefix = match req.custom_field_prefix.as_deref() {
        Some(prefix) => {
            chv_netbox_adapter::validate_custom_field_prefix(prefix)
                .map_err(BffError::BadRequest)?;
            prefix.to_string()
        }
        None => previous
            .as_ref()
            .map(|p| p.custom_field_prefix.clone())
            .unwrap_or_else(|| DEFAULT_CUSTOM_FIELD_PREFIX.to_string()),
    };

    // Captured before the upsert consumes the request: the audit event
    // records only the boolean, never the token. `token_usable` mirrors the
    // store's own normalization (empty/whitespace == absent) so the error
    // mapping below can tell "the caller supplied no token" apart from
    // "the store could not encrypt the supplied token" (#655).
    let token_supplied = req.token.is_some();
    let token_usable = req
        .token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    let config = state
        .netbox_config
        .upsert(NetboxProjectionConfigUpsertInput {
            architecture_id: id.clone(),
            endpoint: req.endpoint.trim().to_string(),
            // None/empty keeps the existing secret (store normalizes).
            token: req.token,
            token_secret_ref: req.token_secret_ref.clone(),
            retention_policy: retention,
            enable_post_apply: req.enable_post_apply,
            custom_field_prefix,
            site_name: req.site_name.clone(),
        })
        .await
        .map_err(|e| match e {
            // Create-without-token: the store refuses because the
            // ciphertext column is NOT NULL. The contract's "config
            // has no usable token" code covers it; the store's reason
            // is static and safe, but the dedicated variant renders
            // the BFF's own clean message instead.
            //
            // #655: `InvalidConfiguration` is also the fail-closed
            // credential-encryption error. When the caller DID supply a
            // usable token, reporting `NETBOX_TOKEN_MISSING` would hide
            // the real cause (e.g. `CHV_ENCRYPTION_KEY` unset) and blame
            // the caller; surface it as a server-side internal error and
            // let the logged reason name the encryption failure.
            StoreError::InvalidConfiguration { .. } if !token_usable => {
                BffError::NetboxTokenMissing {
                    architecture_id: id.to_string(),
                }
            }
            StoreError::InvalidConfiguration { reason } => BffError::Internal(format!(
                "failed to persist netbox config for architecture {}: {}",
                id, reason
            )),
            other => other.into(),
        })?;

    // Best-effort audit event: the config row is already persisted, so
    // an event-store failure must not fail the request (worker-parity).
    let mut changed: Vec<&str> = Vec::new();
    {
        let prev = previous.as_ref();
        if prev.map(|p| p.endpoint.as_str()) != Some(config.endpoint.as_str()) {
            changed.push("endpoint");
        }
        if prev.map(|p| p.token_secret_ref.as_str()) != Some(config.token_secret_ref.as_str()) {
            changed.push("token_secret_ref");
        }
        if prev.map(|p| p.retention_policy) != Some(config.retention_policy) {
            changed.push("retention_policy");
        }
        if prev.map(|p| p.enable_post_apply) != Some(config.enable_post_apply) {
            changed.push("enable_post_apply");
        }
        if prev.map(|p| p.custom_field_prefix.as_str()) != Some(config.custom_field_prefix.as_str())
        {
            changed.push("custom_field_prefix");
        }
        if prev.map(|p| p.site_name.as_deref()) != Some(config.site_name.as_deref()) {
            changed.push("site_name");
        }
        if prev.is_none() {
            changed.push("created");
        }
    }
    emit_netbox_event(
        &state,
        &claims,
        EVENT_NETBOX_CONFIG_UPDATED,
        EventSeverity::Info,
        serde_json::json!({
            "architecture_id": id.as_str(),
            "changed": changed,
            // Boolean only — the token value never reaches an event.
            "token_changed": token_supplied,
        }),
    )
    .await;

    Ok(Json(NetboxConfigResponse::from(config)))
}

/// `POST /v1/architectures/netbox/config/delete` — remove the projection
/// config. NetBox is untouched (no cleanup runs against the projection
/// target — store semantics). Same write guards as upsert. An absent
/// config answers 404 `NETBOX_NOT_CONFIGURED`; a removed one returns
/// `{deleted: true}`.
///
/// The contract's events section defines no separate event name for
/// config deletion — "every config mutation" emits
/// `architecture_netbox_config_updated`, so the delete reuses that event
/// with a `deleted: true` detail (documented interpretation).
pub async fn netbox_config_delete(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxConfigDeleteRequest>,
) -> Result<Json<NetboxConfigDeleteResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    tracing::info!(architecture_id = %id, actor = %claims.sub, "netbox_config_delete");

    let topo = get_topology_authorized(&state, &claims, &id).await?;
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    enforce_production_guard(topo.environment.as_deref(), role)?;

    let deleted = state.netbox_config.delete(&id).await?;
    if !deleted {
        return Err(BffError::NetboxNotConfigured {
            architecture_id: id.to_string(),
        });
    }
    emit_netbox_event(
        &state,
        &claims,
        EVENT_NETBOX_CONFIG_UPDATED,
        EventSeverity::Info,
        serde_json::json!({
            "architecture_id": id.as_str(),
            "deleted": true,
        }),
    )
    .await;
    Ok(Json(NetboxConfigDeleteResponse { deleted: true }))
}

// ---------------------------------------------------------------------------
// Handlers — dry-run
// ---------------------------------------------------------------------------

/// `POST /v1/architectures/netbox/export/dry-run` — compute the
/// projection plan synchronously and return it (no writes to NetBox).
///
/// Gates (order fixed): config exists (400 `NETBOX_NOT_CONFIGURED` —
/// the contract pins 400 on the action paths, vs. 404 on config/get),
/// token decrypts (400 `NETBOX_TOKEN_MISSING`, fail-closed), the most
/// recent `succeeded` apply run's version exists with a parseable
/// normalized model (400 `NETBOX_NOT_APPLIED` — never the editable
/// draft). Then the runner's `dry_run` executes against live NetBox
/// read endpoints, bounded by the client's request timeout.
///
/// Client/runner failures map per the contract: unreachable → 502
/// `NETBOX_UNREACHABLE`, token rejected → 502 `NETBOX_AUTH_FAILED`,
/// anything else → 500 (transport detail stays in the server log).
///
/// Read semantics on the topology (dry-run never writes): foreign row
/// 403, missing 404, system rows readable — the contract escalates
/// production to Admin for *export* only.
pub async fn netbox_dry_run(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxConfigGetRequest>,
) -> Result<Json<NetboxDryRunResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    tracing::info!(architecture_id = %id, actor = %claims.sub, "netbox_dry_run");

    get_topology_authorized(&state, &claims, &id).await?;

    let config = state.netbox_config.get(&id).await?.ok_or_else(|| {
        BffError::NetboxNotConfiguredPrecondition {
            architecture_id: id.to_string(),
        }
    })?;

    // Decrypt in-memory only, fail-closed: the store never returns the
    // ciphertext and neither does any error message here.
    let token = match state.netbox_config.read_token(&id).await {
        Ok(Some(token)) => token,
        Ok(None) => {
            return Err(BffError::NetboxTokenMissing {
                architecture_id: id.to_string(),
            })
        }
        Err(e) => {
            tracing::warn!(
                architecture_id = %id,
                error = %e,
                "netbox token decrypt failed (fail-closed)"
            );
            return Err(BffError::NetboxTokenMissing {
                architecture_id: id.to_string(),
            });
        }
    };

    let (version_id, model, version_number) = resolve_applied_version(&state, &id).await?;

    // HTTPS is enforced by the constructor (fail-closed); the endpoint
    // was already scheme-checked at accept time, so this is
    // belt-and-braces for configs written before that gate existed.
    // The construction goes through [`build_netbox_client`] — the
    // HTTPS-only `NetBoxClient::new` in production, the adapter's
    // plain-HTTP test constructor under this crate's dev-only
    // `test-http` feature (see the seam's doc).
    let client = build_netbox_client(&config.endpoint, NetBoxToken::new(token))
        .map_err(map_client_build_error)?;
    let runner = NetboxProjectionRunner::new(client);

    let input = NetboxProjectionInput {
        architecture: &model,
        architecture_id: id.as_str(),
        architecture_version: version_number as u64,
        snapshot: None,
        site_name: config.site_name.as_deref(),
        retention: retention_from_config(config.retention_policy),
        names: CustomFieldNames::new(&config.custom_field_prefix),
    };

    let plan = match runner.dry_run(&input).await {
        Ok(plan) => plan,
        Err(e) => return Err(map_dry_run_error(&id, e)),
    };

    tracing::info!(
        architecture_id = %id,
        version_id = %version_id,
        create = plan.summary.create,
        update = plan.summary.update,
        no_op = plan.summary.no_op,
        conflict = plan.summary.conflict,
        stale = plan.summary.stale,
        "netbox dry-run plan computed"
    );
    // The contract's event list includes architecture_netbox_dry_run;
    // the worker emits it for run-row dry-runs, the BFF for the
    // synchronous path. Per the contract's events section, events carry
    // the architecture id, run id, trigger, and summary — this path
    // persists no run row (the sync dry-run writes nothing), so
    // run_id is null, and the only way in is the API, hence trigger
    // "manual".
    emit_netbox_event(
        &state,
        &claims,
        EVENT_NETBOX_DRY_RUN,
        EventSeverity::Info,
        serde_json::json!({
            "architecture_id": id.as_str(),
            "run_id": serde_json::Value::Null,
            "trigger": "manual",
            "summary": serde_json::to_value(plan.summary)?,
        }),
    )
    .await;

    Ok(Json(NetboxDryRunResponse { plan }))
}

// ---------------------------------------------------------------------------
// Handlers — export
// ---------------------------------------------------------------------------

/// `POST /v1/architectures/netbox/export` — enqueue a projection run
/// (trigger `manual`, mode `export`). The worker executes it; the BFF
/// never runs an export inline.
///
/// Guards (ownership first — Security F7): ownership (403/404) →
/// production guard (403 `PRODUCTION_REQUIRES_ADMIN`, parity with
/// apply) → config exists (400 `NETBOX_NOT_CONFIGURED`) → applied
/// version resolvable (400 `NETBOX_NOT_APPLIED`). Enqueueing against an
/// already-active run answers 409 `NETBOX_RUN_ACTIVE` (the store's
/// one-active partial unique index).
///
/// No event here: the contract's events section defines no
/// export-requested event — the worker emits
/// `architecture_netbox_export_succeeded` / `_failed` on the run's
/// terminal transition.
pub async fn netbox_export(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxExportRequest>,
) -> Result<Json<NetboxExportResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    tracing::info!(architecture_id = %id, actor = %claims.sub, "netbox_export");

    let topo = get_topology_authorized(&state, &claims, &id).await?;
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    enforce_production_guard(topo.environment.as_deref(), role)?;

    let config_exists = state.netbox_config.get(&id).await?.is_some();
    if !config_exists {
        return Err(BffError::NetboxNotConfiguredPrecondition {
            architecture_id: id.to_string(),
        });
    }

    // Id-only resolution: the worker re-resolves (and parses) the model
    // itself; here only the version reference is persisted.
    let version_id = resolve_applied_version_id(&state, &id).await?;

    let run_id = NetboxProjectionRunId::new(chv_common::gen_resource_id())
        .map_err(|e| BffError::Internal(format!("failed to mint netbox run id: {e}")))?;
    let run = state
        .netbox_runs
        .create(NetboxProjectionRunCreateInput {
            id: run_id,
            architecture_id: id.clone(),
            architecture_version_id: version_id,
            trigger_kind: NetboxProjectionTrigger::Manual,
            mode: NetboxProjectionMode::Export,
            plan_json: None,
            requested_by: Some(claims.sub.clone()),
        })
        .await
        .map_err(|e| match e {
            // The one-active partial unique index: another queued or
            // running run exists for this architecture. The store also
            // reports a duplicate run id as `Conflict`, so the error
            // is classified through the store's shared
            // `is_active_run_conflict` helper (single-sourced with the
            // worker's sweep) — only the active-run conflict is the
            // caller's 409 `NETBOX_RUN_ACTIVE`; any other conflict
            // (e.g. a collision on our freshly minted run id) is an
            // internal error, never a mislabeled NETBOX_RUN_ACTIVE.
            err if is_active_run_conflict(&err) => BffError::NetboxRunActive {
                architecture_id: id.to_string(),
            },
            StoreError::Conflict {
                entity,
                id: conflict_id,
                reason,
            } => {
                tracing::error!(
                    architecture_id = %id,
                    entity = %entity,
                    conflict_id = %conflict_id,
                    reason = %reason,
                    "netbox run enqueue hit an unexpected conflict"
                );
                BffError::Internal("failed to enqueue netbox projection run".into())
            }
            other => other.into(),
        })?;

    tracing::info!(
        architecture_id = %id,
        run_id = %run.id,
        "netbox export run enqueued"
    );
    Ok(Json(NetboxExportResponse {
        run_id: run.id.into_inner(),
        architecture_id: id.into_inner(),
        status: "queued".to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Handlers — runs
// ---------------------------------------------------------------------------

/// `POST /v1/architectures/netbox/runs/list` — run history, newest
/// first (store ordering), default limit 20, clamped to 100. Read
/// semantics (foreign 403 / missing 404); run history can leak
/// operational detail, which is why the route sits in the operator
/// layer (same reasoning as `/v1/architectures/runs/list`).
pub async fn netbox_runs_list(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxRunsListRequest>,
) -> Result<Json<NetboxRunsListResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    tracing::info!(architecture_id = %id, actor = %claims.sub, "netbox_runs_list");

    get_topology_authorized(&state, &claims, &id).await?;

    let limit = req
        .limit
        .unwrap_or(RUNS_LIST_DEFAULT_LIMIT)
        .clamp(1, RUNS_LIST_MAX_LIMIT);
    let runs = state.netbox_runs.list_by_architecture(&id, limit).await?;
    let runs: Vec<NetboxRunSummaryDto> = runs.into_iter().map(run_summary_dto).collect();
    Ok(Json(NetboxRunsListResponse { runs }))
}

/// `POST /v1/architectures/netbox/runs/get` — one full run including
/// `plan_json` and per-entry `result_json` (parsed JSON when parseable,
/// raw string otherwise). The worker's provenance envelope around
/// `result_json` is unwrapped here, surfacing
/// `resolved_architecture_version_id` as a first-class field (see
/// [`NetboxRunDetailDto`]). A run that does not belong to this
/// architecture — or does not exist — answers 404, so run ids cannot be
/// used to probe other architectures.
pub async fn netbox_runs_get(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxRunGetRequest>,
) -> Result<Json<NetboxRunDetailDto>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    // Ownership first, run-id format second: the topology is loaded and
    // authorized before the run id is parsed, so a malformed run id can
    // never serve as a format-only oracle against a foreign topology
    // (the 403/404 must win over the 400).
    get_topology_authorized(&state, &claims, &id).await?;

    let run_id = NetboxProjectionRunId::new(req.run_id.clone())
        .map_err(|e| BffError::BadRequest(format!("invalid run id: {e}")))?;
    tracing::info!(architecture_id = %id, run_id = %run_id, "netbox_runs_get");

    let run = load_run_for_architecture(&state, &id, &run_id).await?;
    Ok(Json(run_detail_dto(run)))
}

/// `POST /v1/architectures/netbox/runs/retry` — re-enqueue a failed run
/// (failed → queued, keeping the attempt history; the store enforces
/// the failed-state and attempt-cap guards). Write guards like export:
/// ownership + production escalation. A refused retry answers 409
/// `PROJECTION_RUN_NOT_RETRYABLE`. Emits
/// `architecture_netbox_export_retried`.
pub async fn netbox_runs_retry(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<NetboxRunRetryRequest>,
) -> Result<Json<NetboxRunRetryResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;

    // Ownership first, run-id format second (same reasoning as
    // runs_get): all topology-level guards fire before the run id is
    // parsed, so a malformed run id is never observable against a
    // foreign topology.
    let topo = get_topology_authorized(&state, &claims, &id).await?;
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    enforce_production_guard(topo.environment.as_deref(), role)?;

    let run_id = NetboxProjectionRunId::new(req.run_id.clone())
        .map_err(|e| BffError::BadRequest(format!("invalid run id: {e}")))?;
    tracing::info!(architecture_id = %id, run_id = %run_id, actor = %claims.sub, "netbox_runs_retry");

    let run = load_run_for_architecture(&state, &id, &run_id).await?;
    let requeued = state
        .netbox_runs
        .requeue(&run.id)
        .await
        .map_err(|e| match e {
            StoreError::Conflict { reason, .. } => BffError::ProjectionRunNotRetryable {
                run_id: run_id.to_string(),
                reason: reason.to_string(),
            },
            other => other.into(),
        })?;

    emit_netbox_event(
        &state,
        &claims,
        EVENT_NETBOX_EXPORT_RETRIED,
        EventSeverity::Info,
        serde_json::json!({
            "run_id": run_id.as_str(),
            "architecture_id": id.as_str(),
            "attempt_count": requeued.attempt_count,
        }),
    )
    .await;
    Ok(Json(NetboxRunRetryResponse {
        run_id: requeued.id.into_inner(),
        status: "queued".to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse the wire `retention_policy` string. Kept manual (rather than a
/// typed enum on the DTO) so an invalid value answers the designer's
/// flat 400 shape instead of axum's serde rejection.
fn parse_retention_policy(s: &str) -> Result<NetboxRetentionPolicy, BffError> {
    match s {
        "mark_stale" => Ok(NetboxRetentionPolicy::MarkStale),
        "delete" => Ok(NetboxRetentionPolicy::Delete),
        other => Err(BffError::BadRequest(format!(
            "retention_policy must be \"mark_stale\" or \"delete\", got {other:?}"
        ))),
    }
}

/// Map the config's retention policy onto the adapter's plan-side enum
/// (the two as_str surfaces are pinned in sync by the types crate).
fn retention_from_config(policy: NetboxRetentionPolicy) -> RetentionPolicy {
    match policy {
        NetboxRetentionPolicy::MarkStale => RetentionPolicy::MarkStale,
        NetboxRetentionPolicy::Delete => RetentionPolicy::Delete,
    }
}

/// Resolve the projection source: the `architecture_version_id` of the
/// most recent `succeeded` apply run for the architecture — never the
/// editable `latest_yaml` draft (contract). Returns
/// `(version_id, parsed model, version_number)`; every failure mode
/// (no succeeded apply run, missing version row, absent or unparseable
/// normalized model) answers 400 `NETBOX_NOT_APPLIED`, mirroring the
/// PR-4 worker's resolution. Used by the dry-run path, which needs the
/// full model to compute the plan.
async fn resolve_applied_version(
    state: &AppState,
    architecture_id: &ArchitectureId,
) -> Result<(ArchitectureVersionId, CHVArchitecture, i64), BffError> {
    let not_applied = || BffError::NetboxNotApplied {
        architecture_id: architecture_id.to_string(),
    };
    let version_id = latest_succeeded_apply_version_id(state, architecture_id).await?;

    let version_repo = VersionRepository::new(state.pool.clone());
    let version = version_repo.get(&version_id, None).await.map_err(|e| {
        tracing::warn!(
            architecture_id = %architecture_id,
            version_id = %version_id,
            error = %e,
            "applied architecture version row could not be loaded"
        );
        not_applied()
    })?;
    let model_json = version
        .normalized_model_json
        .as_deref()
        .ok_or_else(not_applied)?;
    let model: CHVArchitecture = serde_json::from_str(model_json).map_err(|e| {
        tracing::warn!(
            architecture_id = %architecture_id,
            version_id = %version_id,
            error = %e,
            "applied architecture model could not be parsed"
        );
        not_applied()
    })?;
    Ok((version_id, model, version.version_number))
}

/// Resolve the projection source's version id only — the same
/// "most recent `succeeded` apply run → its version id, version row
/// must exist" algorithm as [`resolve_applied_version`], minus the
/// model deserialization. The export-enqueue path only persists the id
/// (the PR-4 worker re-resolves and parses the model itself), so
/// deserializing + validating the full `CHVArchitecture` there would
/// be wasted work. Failure modes answer 400 `NETBOX_NOT_APPLIED`,
/// identically to the full resolution.
async fn resolve_applied_version_id(
    state: &AppState,
    architecture_id: &ArchitectureId,
) -> Result<ArchitectureVersionId, BffError> {
    let not_applied = || BffError::NetboxNotApplied {
        architecture_id: architecture_id.to_string(),
    };
    let version_id = latest_succeeded_apply_version_id(state, architecture_id).await?;

    // Row-exists check only; whether the model parses is the worker's
    // concern (it fails the run there, never silently).
    let version_repo = VersionRepository::new(state.pool.clone());
    version_repo.get(&version_id, None).await.map_err(|e| {
        tracing::warn!(
            architecture_id = %architecture_id,
            version_id = %version_id,
            error = %e,
            "applied architecture version row could not be loaded"
        );
        not_applied()
    })?;
    Ok(version_id)
}

/// The shared "most recent succeeded apply run" lookup — the
/// `architecture_version_id` of the newest `succeeded` apply run for
/// the architecture. Unscoped: ownership was authorized by the caller;
/// the apply-run rows belong to the same architecture we already
/// loaded. No succeeded run answers 400 `NETBOX_NOT_APPLIED`.
async fn latest_succeeded_apply_version_id(
    state: &AppState,
    architecture_id: &ArchitectureId,
) -> Result<ArchitectureVersionId, BffError> {
    let apply_runs = state
        .apply_runs
        .list_for_architecture(architecture_id, None)
        .await?;
    // `list_for_architecture` orders created_at DESC, rowid DESC —
    // newest first, with SQLite's insertion-ordered `rowid` breaking
    // same-second ties; the first Succeeded row is the most recent
    // successful apply.
    let latest_succeeded = apply_runs
        .iter()
        .find(|apply| apply.status == RunStatus::Succeeded)
        .ok_or_else(|| BffError::NetboxNotApplied {
            architecture_id: architecture_id.to_string(),
        })?;
    Ok(latest_succeeded.architecture_version_id.clone())
}

/// Client construction seam for the synchronous dry-run.
///
/// Production always takes the HTTPS-only, fail-closed
/// [`NetBoxClient::new`]. Under this crate's `test-http` feature — a
/// dev-dependencies-only passthrough of the adapter's test feature,
/// mirroring how `chv-controlplane-service` enables it for its
/// wiremock suites — the adapter's plain-HTTP test constructor is used
/// instead, so the BFF's own wiremock tests can drive the real request
/// path through this handler. The feature is never enabled by
/// production dependents (only the self dev-dependency in Cargo.toml
/// turns it on for the integration-test build).
#[cfg(feature = "test-http")]
fn build_netbox_client(endpoint: &str, token: NetBoxToken) -> Result<NetBoxClient, ClientError> {
    NetBoxClient::new_unchecked_for_tests(endpoint, token)
}

/// Demo-mode variant of the seam (ADR-024 decision 5, issue #586):
/// plain HTTP is allowed **only** when the runtime half of the double
/// gate (`CHV_NETBOX_ALLOW_HTTP=1`) is also open — mirroring
/// `chv-controlplane-service`'s `netbox_demo::plain_http_client_factory`
/// for the worker path, so the BFF's synchronous dry-run carries the
/// same double gate. Fails closed with the stable
/// [`ClientError::HttpsRequired`] otherwise. Default-off feature,
/// never in release packaging; see this crate's Cargo.toml.
#[cfg(all(feature = "netbox-demo", not(feature = "test-http")))]
fn build_netbox_client(endpoint: &str, token: NetBoxToken) -> Result<NetBoxClient, ClientError> {
    if std::env::var("CHV_NETBOX_ALLOW_HTTP").ok().as_deref() != Some("1") {
        return Err(ClientError::HttpsRequired {
            endpoint: endpoint.to_string(),
        });
    }
    NetBoxClient::new_unchecked_for_tests(endpoint, token)
}

#[cfg(not(any(feature = "test-http", feature = "netbox-demo")))]
fn build_netbox_client(endpoint: &str, token: NetBoxToken) -> Result<NetBoxClient, ClientError> {
    NetBoxClient::new(endpoint, token)
}

/// Map a NetBox client construction failure. The endpoint scheme was
/// already gated at accept time; HttpsRequired here is belt-and-braces
/// for configs that predate the gate (mapped to the contract's 400
/// code), a malformed endpoint is a flat 400, anything else is internal.
fn map_client_build_error(e: ClientError) -> BffError {
    match e {
        ClientError::HttpsRequired { .. } => BffError::NetboxHttpsRequired,
        ClientError::InvalidEndpoint { reason } => {
            BffError::BadRequest(format!("invalid netbox endpoint: {reason}"))
        }
        other => BffError::Internal(format!("failed to build netbox client: {other}")),
    }
}

/// Map a synchronous dry-run failure to the contract's stable codes:
/// unreachable → 502 `NETBOX_UNREACHABLE`, token rejected → 502
/// `NETBOX_AUTH_FAILED`. Transport detail is logged server-side only —
/// never echoed into the response (it can carry the endpoint URL, and
/// the error discipline keeps messages minimal). Every other failure
/// (mapping/plan violations, contract-violating responses) is a flat
/// 500.
fn map_dry_run_error(architecture_id: &ArchitectureId, e: RunnerError) -> BffError {
    match &e {
        RunnerError::Client(ClientError::Unreachable { .. }) => {
            tracing::warn!(architecture_id = %architecture_id, error = %e, "netbox dry-run unreachable");
            BffError::NetboxUnreachable {
                architecture_id: architecture_id.to_string(),
            }
        }
        RunnerError::Client(ClientError::AuthFailed) => {
            tracing::warn!(architecture_id = %architecture_id, "netbox dry-run auth failed");
            BffError::NetboxAuthFailed {
                architecture_id: architecture_id.to_string(),
            }
        }
        RunnerError::Client(ClientError::HttpsRequired { .. }) => BffError::NetboxHttpsRequired,
        other => {
            tracing::warn!(architecture_id = %architecture_id, error = %other, "netbox dry-run failed");
            BffError::Internal("netbox dry-run failed".into())
        }
    }
}

/// Load a run and verify it belongs to `architecture_id`; a mismatch or
/// a missing run answers 404 (never the run's data — run ids must not
/// become cross-architecture probes).
async fn load_run_for_architecture(
    state: &AppState,
    architecture_id: &ArchitectureId,
    run_id: &NetboxProjectionRunId,
) -> Result<NetboxProjectionRun, BffError> {
    let run =
        state.netbox_runs.get(run_id).await?.ok_or_else(|| {
            BffError::NotFound(format!("netbox_projection_run {run_id} not found"))
        })?;
    if run.architecture_id != *architecture_id {
        return Err(BffError::NotFound(format!(
            "netbox_projection_run {run_id} not found"
        )));
    }
    Ok(run)
}

/// Map a run row onto the runs/list summary DTO. `summary_json` is
/// parsed when present; a parse failure degrades to `None` (the counts
/// are advisory, the row itself is the source of truth).
fn run_summary_dto(r: NetboxProjectionRun) -> NetboxRunSummaryDto {
    NetboxRunSummaryDto {
        id: r.id.into_inner(),
        architecture_id: r.architecture_id.into_inner(),
        trigger: r.trigger_kind,
        status: r.status,
        mode: r.mode,
        summary: parse_json_column(r.summary_json.as_deref()),
        error_message: r.error_message,
        attempt_count: r.attempt_count,
        requested_by: r.requested_by,
        started_at: r.started_at.map(|d| d.to_rfc3339()),
        finished_at: r.finished_at.map(|d| d.to_rfc3339()),
        created_at: r.created_at.to_rfc3339(),
    }
}

/// Map a run row onto the runs/get detail DTO — summary fields plus the
/// parsed `plan_json` / `result_json` (raw string when unparseable, per
/// the contract). `result_json` additionally goes through
/// [`unwrap_result_envelope`], which serves the worker's provenance
/// envelope's inner outcome and lifts the resolved version id onto the
/// DTO (see [`NetboxRunDetailDto`]'s doc).
fn run_detail_dto(r: NetboxProjectionRun) -> NetboxRunDetailDto {
    let (result_json, resolved_architecture_version_id) =
        unwrap_result_envelope(parse_json_or_raw(r.result_json.as_deref()));
    NetboxRunDetailDto {
        id: r.id.into_inner(),
        architecture_id: r.architecture_id.into_inner(),
        architecture_version_id: r.architecture_version_id.into_inner(),
        trigger: r.trigger_kind,
        status: r.status,
        mode: r.mode,
        plan_json: parse_json_or_raw(r.plan_json.as_deref()),
        result_json,
        resolved_architecture_version_id,
        summary: parse_json_column(r.summary_json.as_deref()),
        error_message: r.error_message,
        attempt_count: r.attempt_count,
        requested_by: r.requested_by,
        started_at: r.started_at.map(|d| d.to_rfc3339()),
        finished_at: r.finished_at.map(|d| d.to_rfc3339()),
        next_attempt_at: r.next_attempt_at.map(|d| d.to_rfc3339()),
        created_at: r.created_at.to_rfc3339(),
    }
}

/// Unwrap the projection worker's provenance envelope from a parsed
/// `result_json` column. The worker persists
/// `{ "resolved_architecture_version_id": <string>, "result": <object> }`
/// (`NetboxProjectionWorker::result_envelope` in `chv-controlplane-service`);
/// runs/get serves the inner outcome and surfaces the version id.
///
/// Defensive by design: the unwrap fires only when the parsed value is
/// an object carrying BOTH a string `resolved_architecture_version_id`
/// and an object `result`. Anything else — null, a raw string (already
/// wrapped as a JSON string by [`parse_json_or_raw`]), or an object
/// without both envelope keys — is returned unchanged with a null
/// version id, so unknown shapes pass through verbatim (forward
/// compatibility).
fn unwrap_result_envelope(parsed: Option<Value>) -> (Option<Value>, Option<String>) {
    let Some(Value::Object(envelope)) = &parsed else {
        return (parsed, None);
    };
    match (
        envelope.get("resolved_architecture_version_id"),
        envelope.get("result"),
    ) {
        (Some(Value::String(version_id)), Some(result @ Value::Object(_))) => {
            (Some(result.clone()), Some(version_id.clone()))
        }
        _ => (parsed, None),
    }
}

/// Parse an optional JSON column; `None` stays `None`, a parse failure
/// also degrades to `None` (used for advisory summary counts).
fn parse_json_column(raw: Option<&str>) -> Option<Value> {
    raw.and_then(|s| serde_json::from_str::<Value>(s).ok())
}

/// Parse an optional JSON column; a parse failure wraps the raw string
/// in a JSON string so the wire stays `{...}_json: <value>`-shaped
/// rather than dropping data (used for plan/result payloads).
fn parse_json_or_raw(raw: Option<&str>) -> Option<Value> {
    match raw {
        None => None,
        Some(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => Some(v),
            Err(_) => Some(Value::String(s.to_string())),
        },
    }
}

/// Append a NetBox projection audit event (`EventType::Audit`, message
/// = the event name, details = structured secret-free JSON). Best
/// effort: the mutated/enqueued state is the source of truth, and a
/// broken events table must not fail the request — the worker follows
/// the same swallow-and-log policy.
async fn emit_netbox_event(
    state: &AppState,
    claims: &Claims,
    event_name: &str,
    severity: EventSeverity,
    details: Value,
) {
    let input = EventAppendInput {
        occurred_unix_ms: state.clock.now().timestamp_millis(),
        event_type: EventType::Audit,
        severity,
        resource_kind: None,
        resource_id: None,
        node_id: None,
        operation_id: None,
        actor_id: None,
        requested_by: Some(claims.sub.clone()),
        correlation_id: None,
        message: event_name.to_string(),
        details: Some(details.to_string()),
    };
    if let Err(e) = state.event_repo.append(&input).await {
        tracing::warn!(
            event = event_name,
            error = %e,
            "failed to append netbox projection audit event"
        );
    }
}
