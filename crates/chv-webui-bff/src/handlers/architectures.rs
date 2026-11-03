//! Architecture Designer BFF handlers — Phase 0.
//!
//! This module wires the BFF surface for the Architecture Designer feature
//! described in `docs/specs/architecture-designer/`. Phase 0 covers ONLY the
//! topology CRUD lifecycle (list, get, create, update, archive). All other
//! verbs in the feature surface (validate, check-fleet, generate-yaml, plan,
//! apply, destroy, drift, runs, versions) are exposed here as stubs that
//! return `BffError::NotImplemented("phase 0")` so the routing surface is
//! complete and the UI can light up endpoint-by-endpoint as later phases
//! deliver real behavior.
//!
//! # Path style
//!
//! NOTE: Path style follows the existing CHV BFF POST-only convention
//! (`/v1/architectures/<verb>`) rather than the REST verbs in
//! `docs/specs/architecture-designer/contracts/api-contract.md`. Every other
//! handler in this crate (vms, networks, volumes, snapshots, …) uses POST
//! with verbs in the path, so introducing REST verbs here for one feature
//! would split the routing convention. Documented as an intentional
//! deviation; the contract document is the design of record, the routing
//! surface adapts to the host service.
//!
//! # Roles (Phase 0)
//!
//! Phase 0 reuses the existing 3-level role hierarchy
//! (Viewer / Operator / Admin) declared in [`crate::auth::Role`]. The
//! implementation plan called for fine-grained `architecture:*` permission
//! strings, but the current RBAC has no permission-string namespace; bolting
//! one on is out of scope for the Phase 0 skeleton and is tracked
//! separately.
//!
// TODO(arch-designer/perm-fine-grained): see <issue-link-tbd> for migration
// to architecture:* permission strings.
//
//! # Optimistic concurrency
//!
//! `update_architecture` echoes `expected_version` to the topology repo,
//! which executes `WHERE version_number = expected_version` in the UPDATE
//! and returns [`StoreError::StaleVersion`] on mismatch. That maps to
//! HTTP 409 Conflict here so UIs can re-fetch and prompt the user. See
//! `docs/specs/component/architecture-designer-data-model.md` for the
//! invariant.
//!
//! # Ownership model (Security H6 — cross-tenant object access)
//!
//! Every handler that takes an architecture id (or a plan id, which resolves
//! through its architecture) authorizes the caller against the row's
//! `owner_user_id` before returning or mutating anything:
//!
//! - **Admin**: sees and mutates all rows (store scope `None`).
//! - **Viewer / Operator**: see rows they own (`owner_user_id ==
//!   claims.sub`) plus system-owned rows (`owner_user_id IS NULL` — the
//!   pre-seeded starter topologies, which are shared by design). Mutating
//!   a topology row is narrower: only rows they own, plus system-owned
//!   rows for admin. The store write predicates drop the `IS NULL`
//!   carve-out, and [`require_owner_or_admin`] denies NULL-owned writes to
//!   non-admins, so starters are read-only templates that operators clone
//!   rather than edit.
//! - A non-admin request against a row owned by *another* user fails with
//!   403 Forbidden via [`require_owner_or_admin`], never with the row's
//!   data. The store layer independently re-enforces the same predicate
//!   (`visible_to_user`) so a handler bug cannot leak a foreign row.
//!
//! The 403-vs-404 disambiguation is deliberate: a scoped store read reports
//! a foreign row as `NotFound`; the handler then probes the row unscoped
//! (existence only — its contents are never returned) to decide between
//! "you may not see this" (403) and "this does not exist" (404).

use axum::{extract::State, Json};
use chv_architecture_reconcile::apply::{
    apply_plan, ApplyContext, ApplyError, ApplyOutcome, ConfirmationToken,
};
use chv_architecture_reconcile::drift::{compute_drift, DriftFinding, DriftReport, DriftSummary};
use chv_architecture_reconcile::FleetInventoryProvider;
use chv_architecture_validate::{
    fleet::check_fleet, parse_yaml as parse_arch_yaml, validate as validate_yaml_str,
    ValidationResult,
};
use chv_controlplane_store::{
    DriftReportCreateInput, InventorySnapshotCreateInput, PlanCreateInput, PlanRepository,
    PlanStatusUpdateInput, StoreError, TopologyCreateInput, TopologyListFilter,
    TopologyUpdateInput, VersionCreateInput, VersionRepository,
};
use chv_controlplane_types::architecture::{
    ArchitectureDriftReportId, ArchitectureId, ArchitecturePlanId, ArchitectureStatus,
    ArchitectureTopology, ArchitectureVersionId, DriftStatus, Finding, FleetCheckStatus,
    InventorySnapshotId, PlanChange, PlanMode, PlanStatus, RunStatus, Severity, ValidationStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{has_apply_permission, require_operator_or_admin, BearerToken, Claims, Role};
use crate::metrics_apply::{record_apply_status, ApplyStatusLabel, ApplyTimer};
use crate::metrics_drift::{record_drift_status, DriftStatusLabel};
use crate::router::AppState;
use crate::BffError;

// ---------------------------------------------------------------------------
// Permission plumbing for drift / inventory capture
// ---------------------------------------------------------------------------

/// Resolve the caller's `architecture:apply` permission for the drift /
/// inventory capture path. Fail-closed on unrecognised role strings — an
/// unknown role must NOT be granted apply (matches the security spirit of
/// M14: silent permission grants are worse than spurious drift findings).
///
/// Centralised here because the drift `PermissionChanged` heuristic in
/// `chv-architecture-reconcile` only fires when `snapshot.deploy_allowed`
/// is `false`. Hardcoding `true` (the previous shape of all three call
/// sites) made the heuristic dead code outside unit tests; this helper
/// puts the truth back on the wire.
fn caller_can_apply(claims: &Claims) -> bool {
    Role::parse(&claims.role)
        .map(|r| has_apply_permission(&r))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Ownership authorization (Security H6)
// ---------------------------------------------------------------------------

/// Resolve the caller's store-level ownership scope.
///
/// `None` means "no scoping" and is reserved for admins (and internal
/// callers that have already authorized the action). `Some(sub)` makes the
/// store apply the `(owner_user_id = sub OR owner_user_id IS NULL)`
/// predicate — see the module-level ownership model.
fn caller_visible_scope(claims: &Claims) -> Result<Option<String>, BffError> {
    let role = Role::parse(&claims.role)
        .ok_or_else(|| BffError::Internal("unparseable role on authenticated request".into()))?;
    Ok(if matches!(role, Role::Admin) {
        None
    } else {
        Some(claims.sub.clone())
    })
}

/// Object-level ownership gate used by every architecture handler.
///
/// Ownership model (Security H6):
///
/// - `role == admin` always passes (sees and mutates all rows).
/// - `owner_user_id == claims.sub` passes (the caller's own row).
/// - `owner_user_id == None` — system-owned starter rows. They stay
///   READABLE to every authenticated caller (the store read predicate
///   keeps its `IS NULL` carve-out), but WRITES to them are admin-only:
///   non-admins get 403 here, so starters are read-only templates that
///   operators clone instead of edit.
/// - any other owner fails with 403 Forbidden.
pub(crate) fn require_owner_or_admin(
    claims: &Claims,
    owner_user_id: Option<&str>,
) -> Result<(), BffError> {
    if claims.role == "admin" {
        return Ok(());
    }
    match owner_user_id {
        Some(owner) if owner == claims.sub => Ok(()),
        None => Err(BffError::Forbidden(
            "system-owned starter architectures are read-only; clone to edit".into(),
        )),
        Some(_) => Err(BffError::Forbidden(
            "you do not own this architecture".into(),
        )),
    }
}

/// Load a topology for the authenticated caller, distinguishing 403
/// (row exists but belongs to another user) from 404 (row does not exist).
///
/// The scoped store read never returns a foreign row; when it reports
/// `NotFound` we probe unscoped *for the existence/ownership decision only*
/// and discard the row contents — the caller's error carries no data.
pub(crate) async fn get_topology_authorized(
    state: &AppState,
    claims: &Claims,
    id: &ArchitectureId,
) -> Result<ArchitectureTopology, BffError> {
    let scope = caller_visible_scope(claims)?;
    match state.topology_repo.get(id, scope.as_deref()).await {
        Ok(topo) => Ok(topo),
        Err(StoreError::NotFound { .. }) if scope.is_some() => {
            match state.topology_repo.get(id, None).await {
                Ok(row) => match require_owner_or_admin(claims, row.owner_user_id.as_deref()) {
                    Ok(()) => Err(BffError::Internal(
                        "ownership helper passed on a scoped topology miss".into(),
                    )),
                    Err(forbidden) => Err(forbidden),
                },
                Err(_) => Err(BffError::NotFound(format!(
                    "architecture_topology {} not found",
                    id
                ))),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Load a plan for the authenticated caller with the same 403/404
/// disambiguation as [`get_topology_authorized`]. Plan ownership is
/// inherited from the plan's architecture.
async fn get_plan_authorized(
    state: &AppState,
    claims: &Claims,
    plan_id: &ArchitecturePlanId,
) -> Result<chv_controlplane_types::architecture::ArchitecturePlan, BffError> {
    let scope = caller_visible_scope(claims)?;
    let plan_repo = PlanRepository::new(state.pool.clone());
    match plan_repo.get(plan_id, scope.as_deref()).await {
        Ok(plan) => Ok(plan),
        Err(StoreError::NotFound { .. }) if scope.is_some() => {
            match plan_repo.get(plan_id, None).await {
                // The plan exists but its architecture belongs to another
                // user (or to no user, which the scoped predicate matches —
                // that case cannot land here).
                Ok(_) => Err(BffError::Forbidden(
                    "you do not own the architecture this plan belongs to".into(),
                )),
                Err(_) => Err(BffError::NotFound(format!(
                    "architecture_plan {} not found",
                    plan_id
                ))),
            }
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Lightweight summary returned by `list_architectures`. The API contract in
/// Phase 0 deliberately keeps the list payload close to the full topology so
/// the UI does not need a second round-trip for the dashboard view; later
/// phases may strip large fields (`design_graph_json`, `latest_yaml`) from
/// the list response if list payload size becomes a problem.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArchitectureSummary {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub environment: Option<String>,
    pub status: ArchitectureStatus,
    pub owner_user_id: Option<String>,
    pub last_validation_status: Option<ValidationStatus>,
    pub last_fleet_check_status: Option<FleetCheckStatus>,
    pub version_number: i64,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
}

impl From<ArchitectureTopology> for ArchitectureSummary {
    fn from(t: ArchitectureTopology) -> Self {
        Self {
            id: t.id.into_inner(),
            name: t.name,
            display_name: t.display_name,
            description: t.description,
            environment: t.environment,
            status: t.status,
            owner_user_id: t.owner_user_id,
            last_validation_status: t.last_validation_status,
            last_fleet_check_status: t.last_fleet_check_status,
            version_number: t.version_number,
            created_at: t.created_at.to_rfc3339(),
            updated_at: t.updated_at.to_rfc3339(),
            archived_at: t.archived_at.map(|d| d.to_rfc3339()),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ListArchitecturesRequest {
    /// When true, archived topologies are included in the result. Defaults
    /// to false because the dashboard view excludes archived items.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ListArchitecturesResponse {
    pub architectures: Vec<ArchitectureSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GetArchitectureRequest {
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GetArchitectureResponse {
    pub architecture: ArchitectureSummary,
    /// Full design graph JSON (kept on the get response so the editor can
    /// open without a second round-trip).
    pub design_graph_json: Option<String>,
    pub latest_yaml: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateArchitectureRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Optional initial design graph JSON. Most clients create then update.
    #[serde(default)]
    pub design_graph_json: Option<String>,
    #[serde(default)]
    pub latest_yaml: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateArchitectureResponse {
    pub architecture: ArchitectureSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpdateArchitectureRequest {
    pub id: String,
    /// Version the client read; server fails with 409 Conflict if the
    /// current row has moved on. See module-level optimistic-concurrency
    /// note.
    pub expected_version: i64,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub design_graph_json: Option<String>,
    #[serde(default)]
    pub latest_yaml: Option<String>,
    #[serde(default)]
    pub latest_version_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpdateArchitectureResponse {
    pub architecture: ArchitectureSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArchiveArchitectureRequest {
    pub id: String,
    /// Version the client read; mirrors `update`'s optimistic-concurrency
    /// contract so concurrent edits cannot be silently overwritten by an
    /// archive. Stale-version archives surface as 409 Conflict.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArchiveArchitectureResponse {
    pub architecture: ArchitectureSummary,
}

// ---------------------------------------------------------------------------
// CRUD handlers
// ---------------------------------------------------------------------------

/// List topologies. Viewer-accessible. Default scope excludes archived.
///
/// **Ownership scoping (Security H5):** non-admin callers see only rows
/// they own (`owner_user_id == claims.sub`) plus system-owned starter
/// topologies (`owner_user_id IS NULL`). Admins see every row regardless
/// of owner. Without this filter every viewer/operator would see every
/// other operator's drafts and admins' production-tagged topologies —
/// a multi-tenancy hole flagged by the Phase 7 review.
pub async fn list_architectures(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<ListArchitecturesRequest>,
) -> Result<Json<ListArchitecturesResponse>, BffError> {
    let role = Role::parse(&claims.role)
        .ok_or_else(|| BffError::Internal("unparseable role on authenticated request".into()))?;
    // Admin sees all rows (None lifts the ownership predicate). Operator and
    // Viewer get scoped to their own user_id; system-owned starters
    // (owner_user_id IS NULL) flow through to both via the OR-IS-NULL branch
    // in the store-layer SQL.
    let visible_to_user = if matches!(role, Role::Admin) {
        None
    } else {
        Some(claims.sub.clone())
    };
    tracing::info!(
        include_archived = req.include_archived,
        actor = %claims.sub,
        role = ?role,
        scoped = visible_to_user.is_some(),
        "list_architectures"
    );
    let topologies = state
        .topology_repo
        .list(TopologyListFilter {
            include_archived: req.include_archived,
            visible_to_user,
        })
        .await?;
    Ok(Json(ListArchitecturesResponse {
        architectures: topologies
            .into_iter()
            .map(ArchitectureSummary::from)
            .collect(),
    }))
}

/// Read a single topology including its design graph and latest yaml.
///
/// **Ownership (Security H6):** non-admin callers may only read rows they
/// own plus system-owned starters; a foreign row answers 403, not its data.
pub async fn get_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<GetArchitectureRequest>,
) -> Result<Json<GetArchitectureResponse>, BffError> {
    let id = parse_id(&req.id)?;
    tracing::info!(architecture_id = %id, "get_architecture");
    let topo = get_topology_authorized(&state, &claims, &id).await?;
    let design_graph_json = topo.design_graph_json.clone();
    let latest_yaml = topo.latest_yaml.clone();
    Ok(Json(GetArchitectureResponse {
        architecture: ArchitectureSummary::from(topo),
        design_graph_json,
        latest_yaml,
    }))
}

/// Create a new draft topology. Operator+ only.
pub async fn create_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<CreateArchitectureRequest>,
) -> Result<Json<CreateArchitectureResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    if req.name.trim().is_empty() {
        return Err(BffError::BadRequest("name must not be blank".into()));
    }
    // Production-environment write guard (Security F2): only admins may
    // tag an architecture as production. Without this gate an operator
    // could set environment="production" themselves and then bypass the
    // apply-time admin check by toggling the label off and on at will.
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    if is_production_environment(req.environment.as_deref()) && !role.meets(Role::Admin) {
        return Err(BffError::ProductionRequiresAdmin {
            environment: req.environment.as_deref().unwrap_or("").trim().to_string(),
        });
    }
    let id = ArchitectureId::new(chv_common::gen_resource_id())
        .map_err(|e| BffError::Internal(format!("failed to mint architecture id: {e}")))?;
    tracing::info!(architecture_id = %id, name = %req.name, "create_architecture");
    let topo = state
        .topology_repo
        .create(TopologyCreateInput {
            id,
            name: req.name,
            display_name: req.display_name,
            description: req.description,
            environment: req.environment,
            status: ArchitectureStatus::Draft,
            owner_user_id: Some(claims.sub.clone()),
            design_graph_json: req.design_graph_json,
            latest_yaml: req.latest_yaml,
        })
        .await?;
    Ok(Json(CreateArchitectureResponse {
        architecture: ArchitectureSummary::from(topo),
    }))
}

/// Update a topology with optimistic-concurrency check.
///
/// **Ownership (Security H6):** non-admin callers may only mutate rows
/// they own; system-owned starter rows are read-only templates (403 via
/// [`require_owner_or_admin`]) and a foreign row answers 403.
///
/// **Production guard (Security F2/F7):** the guard decision is made
/// against the *persisted* row's environment, not the request's
/// `environment` field. A non-admin cannot (a) tag a row as production,
/// nor (b) mutate a row that is *already* production-tagged in any way —
/// sending `environment: null` (or omitting it) does not bypass (b),
/// because the persisted tag is what fires the guard. Un-tagging a
/// production row to sneak an apply past `enforce_production_guard` is
/// therefore blocked too.
pub async fn update_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<UpdateArchitectureRequest>,
) -> Result<Json<UpdateArchitectureResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    let latest_version_id = match req.latest_version_id {
        None => None,
        Some(v) => Some(
            ArchitectureVersionId::new(v)
                .map_err(|e| BffError::BadRequest(format!("invalid latest_version_id: {e}")))?,
        ),
    };
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    // (a) Only admins may tag (or re-tag) an architecture as production via
    // update.
    if is_production_environment(req.environment.as_deref()) && !role.meets(Role::Admin) {
        return Err(BffError::ProductionRequiresAdmin {
            environment: req.environment.as_deref().unwrap_or("").trim().to_string(),
        });
    }
    tracing::info!(
        architecture_id = %id,
        expected_version = req.expected_version,
        "update_architecture"
    );

    // Ownership + production-guard load. For a NON-admin caller a foreign
    // row must surface the plain ownership 403 FIRST — never
    // PRODUCTION_REQUIRES_ADMIN, which would leak the persisted
    // environment tag of a row the caller may not even see (Security F7
    // information disclosure). Missing rows stay 404.
    let scope = caller_visible_scope(&claims)?;
    let topo = match state.topology_repo.get(&id, scope.as_deref()).await {
        Ok(topo) => topo,
        Err(StoreError::NotFound { .. }) if scope.is_some() => {
            // Probe unscoped for the existence decision only; the probed
            // row's contents are discarded so no row data leaks into the
            // error response.
            match state.topology_repo.get(&id, None).await {
                Ok(_) => {
                    return Err(BffError::Forbidden(
                        "you do not own this architecture".into(),
                    ));
                }
                Err(_) => {
                    return Err(BffError::NotFound(format!(
                        "architecture_topology {} not found",
                        id
                    )))
                }
            }
        }
        Err(e) => return Err(e.into()),
    };

    // NULL-owned starter rows are read-only templates for non-admins:
    // require_owner_or_admin answers 403 here, before any production
    // decision, so the starter workflow stays clone-not-edit.
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;

    // (b) A non-admin may not mutate a row that is *already*
    // production-tagged, regardless of what the request's environment field
    // says (including null/absent). This is what stops the un-tag-then-apply
    // bypass of the apply-time production guard. Only reached after the
    // ownership gate above passes.
    if is_production_environment(topo.environment.as_deref()) && !role.meets(Role::Admin) {
        return Err(BffError::ProductionRequiresAdmin {
            environment: topo.environment.as_deref().unwrap_or("").trim().to_string(),
        });
    }

    // Skip the UPDATE entirely when the caller sent no field changes — this
    // keeps version_number stable for clients that re-submit a PATCH-style
    // form without modifying anything. The row was already fetched (and
    // authorized) above, so the readback path needs no second get().
    let no_field_changes = req.display_name.is_none()
        && req.description.is_none()
        && req.environment.is_none()
        && req.design_graph_json.is_none()
        && req.latest_yaml.is_none()
        && latest_version_id.is_none();
    if no_field_changes {
        return Ok(Json(UpdateArchitectureResponse {
            architecture: ArchitectureSummary::from(topo),
        }));
    }

    let result = state
        .topology_repo
        .update(
            TopologyUpdateInput {
                id: id.clone(),
                expected_version: req.expected_version,
                display_name: req.display_name,
                description: req.description,
                environment: req.environment,
                // Status transitions are owned by validate/plan/apply phases —
                // Phase 0 CRUD never moves a topology out of Draft.
                status: None,
                design_graph_json: req.design_graph_json,
                latest_yaml: req.latest_yaml,
                latest_version_id,
                last_validation_status: None,
                last_fleet_check_status: None,
            },
            scope.as_deref(),
        )
        .await;
    match result {
        Ok(topo) => Ok(Json(UpdateArchitectureResponse {
            architecture: ArchitectureSummary::from(topo),
        })),
        // Surface a stable, human-readable conflict message — the
        // From<StoreError> impl on BffError already does this, but we
        // explicitly build the message here to match the wording the
        // dispatch contract calls out.
        Err(StoreError::StaleVersion {
            current, expected, ..
        }) => Err(BffError::Conflict(format!(
            "stale version: client sent {expected}, current is {current}"
        ))),
        Err(other) => Err(other.into()),
    }
}

/// Soft-delete a topology. Operator+ only. Optimistic-concurrency: the caller
/// supplies the version they read; a concurrent edit that bumped the row
/// returns 409 Conflict. Already-archived rows return 404 so callers know
/// whether they were the one that archived it.
pub async fn archive_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<ArchiveArchitectureRequest>,
) -> Result<Json<ArchiveArchitectureResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    tracing::info!(
        architecture_id = %id,
        expected_version = req.expected_version,
        "archive_architecture"
    );
    // Ownership check (403 for foreign rows, 404 for missing) before the
    // write; the scoped store call below re-enforces the same predicate.
    // NULL-owned starter rows pass the READ gate in
    // get_topology_authorized but are denied here for non-admins: they are
    // read-only templates (Security H6).
    let topo = get_topology_authorized(&state, &claims, &id).await?;
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    let scope = caller_visible_scope(&claims)?;
    let result = state
        .topology_repo
        .archive(&id, req.expected_version, scope.as_deref())
        .await;
    match result {
        Ok(topo) => Ok(Json(ArchiveArchitectureResponse {
            architecture: ArchitectureSummary::from(topo),
        })),
        Err(StoreError::StaleVersion {
            current, expected, ..
        }) => Err(BffError::Conflict(format!(
            "stale version: client sent {expected}, current is {current}"
        ))),
        Err(other) => Err(other.into()),
    }
}

// ---------------------------------------------------------------------------
// Stub handlers (Phase 0)
// ---------------------------------------------------------------------------
//
// Each stub validates the role gate that the real handler will use so the
// behavior is identical from the routing perspective once the real
// implementation lands. Handlers accept and ignore `Json<Value>` so the
// stubs are tolerant of any body shape — clients can probe the surface
// without crafting endpoint-specific payloads.

// ---------------------------------------------------------------------------
// Phase 1 DTOs — validate, generate-yaml, import-yaml
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ValidateArchitectureRequest {
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ValidateArchitectureYamlRequest {
    pub yaml: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ValidateArchitectureResponse {
    #[serde(flatten)]
    pub result: ValidationResult,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GenerateYamlRequest {
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GenerateYamlResponse {
    pub yaml: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ImportYamlRequest {
    pub id: String,
    pub yaml: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ImportYamlResponse {
    pub result: ValidationResult,
}

// ---------------------------------------------------------------------------
// Phase 1 handlers
// ---------------------------------------------------------------------------

/// Validate the persisted topology's `latest_yaml`. Persists the outcome by
/// setting `last_validation_status` on the topology row (Passed when no
/// errors, Failed otherwise).
///
/// A topology with no `latest_yaml` is reported as a single SCHEMA_INVALID
/// finding (no body to validate) rather than a 4xx — the caller might be a
/// dashboard that just wants to display "no YAML yet" alongside other
/// validation state.
pub async fn validate_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<ValidateArchitectureRequest>,
) -> Result<Json<ValidateArchitectureResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    tracing::info!(architecture_id = %id, "validate_architecture");

    // Ownership check before reading the row or persisting any status.
    // NULL-owned starter rows are read-only templates for non-admins (403
    // via require_owner_or_admin), and a production-tagged row is
    // admin-only — validating it would persist `last_validation_status`
    // onto a row the caller may not write.
    let topo = get_topology_authorized(&state, &claims, &id).await?;
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    enforce_production_guard(topo.environment.as_deref(), role)?;
    let yaml = topo.latest_yaml.as_deref().unwrap_or("");
    let result = validate_yaml_str(yaml);

    // Best-effort persistence of the outcome. If the row was bumped between
    // our get() and the set_validation_status call, surface the conflict so
    // the caller can re-read; the validation result itself was correct, but
    // the persisted status would be against a stale row.
    let new_status = if result.summary.errors == 0 {
        ValidationStatus::Passed
    } else {
        ValidationStatus::Failed
    };
    let scope = caller_visible_scope(&claims)?;
    match state
        .topology_repo
        .set_validation_status(&id, topo.version_number, new_status, scope.as_deref())
        .await
    {
        Ok(_) => {}
        Err(StoreError::StaleVersion {
            current, expected, ..
        }) => {
            return Err(BffError::Conflict(format!(
                "topology was modified concurrently while persisting validation status: client sent {expected}, current is {current}"
            )));
        }
        Err(other) => return Err(other.into()),
    }

    Ok(Json(ValidateArchitectureResponse { result }))
}

/// Validate an ad-hoc YAML body without touching persistent state. Used by
/// the editor's "validate before save" path.
pub async fn validate_architecture_yaml(
    BearerToken(claims): BearerToken,
    State(_state): State<AppState>,
    Json(req): Json<ValidateArchitectureYamlRequest>,
) -> Result<Json<ValidateArchitectureResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    if req.yaml.trim().is_empty() {
        return Err(BffError::BadRequest("yaml must not be blank".into()));
    }
    let result = validate_yaml_str(&req.yaml);
    Ok(Json(ValidateArchitectureResponse { result }))
}

/// Phase 1 generate-yaml. Returns the topology's `latest_yaml` verbatim if
/// present; otherwise responds with a 422 carrying `code: GRAPH_EMPTY`.
///
/// The graph→YAML mapper is a Phase 2 deliverable owned by the canvas (the
/// canvas knows the node/edge schema; the validator does not). Surfacing a
/// stable code now lets the UI behave deterministically while the mapper
/// is built.
pub async fn generate_architecture_yaml(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<GenerateYamlRequest>,
) -> Result<Json<GenerateYamlResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    tracing::info!(architecture_id = %id, "generate_architecture_yaml");
    let topo = get_topology_authorized(&state, &claims, &id).await?;
    if let Some(yaml) = topo.latest_yaml.filter(|s| !s.trim().is_empty()) {
        return Ok(Json(GenerateYamlResponse { yaml }));
    }
    // Phase 2 will translate design_graph_json → YAML. Until then, an
    // empty graph means we have nothing to emit.
    Err(BffError::GraphEmpty)
}

/// Replace a topology's `latest_yaml` with caller-supplied YAML. Validates
/// the YAML, persists `latest_yaml` and `last_validation_status` together
/// in one optimistic-concurrency-checked update. Validation failure does
/// NOT block the import — the YAML is stored and the row is marked
/// `last_validation_status = failed` so the operator can iterate.
pub async fn import_yaml_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<ImportYamlRequest>,
) -> Result<Json<ImportYamlResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    if req.yaml.trim().is_empty() {
        return Err(BffError::BadRequest("yaml must not be blank".into()));
    }
    tracing::info!(architecture_id = %id, "import_yaml_architecture");

    let topo = get_topology_authorized(&state, &claims, &id).await?;
    // NULL-owned starter rows are read-only templates for non-admins (403),
    // and a production-tagged row is admin-only — importing YAML would
    // persist `latest_yaml` onto a row the caller may not write.
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    enforce_production_guard(topo.environment.as_deref(), role)?;
    let result = validate_yaml_str(&req.yaml);
    let new_status = if result.summary.errors == 0 {
        ValidationStatus::Passed
    } else {
        ValidationStatus::Failed
    };

    let scope = caller_visible_scope(&claims)?;
    let update_result = state
        .topology_repo
        .update(
            TopologyUpdateInput {
                id: id.clone(),
                expected_version: topo.version_number,
                display_name: None,
                description: None,
                environment: None,
                status: None,
                design_graph_json: None,
                latest_yaml: Some(req.yaml),
                latest_version_id: None,
                last_validation_status: Some(new_status),
                last_fleet_check_status: None,
            },
            scope.as_deref(),
        )
        .await;
    match update_result {
        Ok(_) => Ok(Json(ImportYamlResponse { result })),
        Err(StoreError::StaleVersion {
            current, expected, ..
        }) => Err(BffError::Conflict(format!(
            "stale version: client sent {expected}, current is {current}"
        ))),
        Err(other) => Err(other.into()),
    }
}

// ---------------------------------------------------------------------------
// Phase 4 DTOs — plan, destroy-plan, discard-plan
// ---------------------------------------------------------------------------

/// Body for `POST /v1/architectures/plan` and `POST /v1/architectures/destroy-plan`.
///
/// `refresh_inventory` defaults to `true` so callers get a fresh fleet
/// snapshot per plan call. Setting it to `false` reuses the most recent
/// persisted snapshot for the architecture and returns 400 if none exists.
///
/// `allow_warnings` is a forward-compatibility hook used by the apply path
/// in Phase 5; it is accepted here so the wire shape is stable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanArchitectureRequest {
    pub id: String,
    #[serde(default)]
    pub allow_warnings: Option<bool>,
    #[serde(default)]
    pub refresh_inventory: Option<bool>,
}

/// Body for `POST /v1/architectures/discard-plan`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DiscardPlanRequest {
    pub plan_id: String,
}

/// Response shape for `plan` and `destroy-plan`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanResponse {
    pub plan_id: String,
    pub architecture_id: String,
    /// Numeric topology version the plan was generated against. Lets the UI
    /// reject a stale apply attempt without an extra round-trip.
    pub architecture_version: i64,
    /// Stable id of the `architecture_versions` row referenced by the plan
    /// FK. Phase 5's apply path joins on this id; surfacing it here keeps
    /// the wire shape complete so clients do not have to fish it back out
    /// of the snapshot row.
    pub architecture_version_id: String,
    pub status: PlanStatus,
    pub mode: PlanMode,
    pub summary: chv_architecture_reconcile::PlanSummary,
    pub changes: Vec<PlanChange>,
    pub warnings: Vec<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Response for `discard-plan`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DiscardPlanResponse {
    pub status: String,
}

// ---------------------------------------------------------------------------
// Phase 4 handlers — plan, destroy-plan, discard-plan
// ---------------------------------------------------------------------------

/// Generate an apply-mode plan for a topology. Captures (or reuses) a fleet
/// snapshot, runs validation, computes the diff, persists the plan row, and
/// returns the plan body.
///
/// Operator+ only. The 15-minute TTL is computed against the injected
/// clock so tests can drive expiry deterministically.
pub async fn plan_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<PlanArchitectureRequest>,
) -> Result<Json<PlanResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    let refresh = req.refresh_inventory.unwrap_or(true);
    // TODO(Phase 5): consult allow_warnings to bypass non-blocking warnings
    // during apply. Phase 4 plan generation always returns warnings inline;
    // the apply handler is the one that gates on the flag.
    let _ = req.allow_warnings;
    let resp = generate_plan_inner(&state, &claims, id, PlanMode::Apply, refresh).await?;
    Ok(Json(resp))
}

/// Generate a destroy-mode plan for a topology. Same shape as `plan` but
/// every desired resource becomes a `Delete` and `requires_confirmation` is
/// always true on the resulting changes.
pub async fn destroy_plan_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<PlanArchitectureRequest>,
) -> Result<Json<PlanResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    let refresh = req.refresh_inventory.unwrap_or(true);
    let resp = generate_plan_inner(&state, &claims, id, PlanMode::Destroy, refresh).await?;
    Ok(Json(resp))
}

/// Mark a previously-generated plan as `Discarded`. Idempotent — discarding
/// an already-discarded plan returns the same response. Returns 404 when
/// the plan does not exist.
///
/// ## Discardable states
///
/// `Draft`, `FailedValidation`, `RequiresConfirmation`, `ReadyToApply` are
/// discardable; the row transitions to `Discarded` and `discarded_by` is
/// stamped with the caller's subject. `Discarded` itself is idempotent
/// (returns 200 without re-stamping). `Applying`, `Applied`, `Failed`,
/// `Expired` are terminal and refuse with 409 / `code: PLAN_NOT_DISCARDABLE`
/// — Phase 5's apply path moves a plan into those states and a discard
/// after that point would be misleading.
pub async fn discard_plan_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<DiscardPlanRequest>,
) -> Result<Json<DiscardPlanResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let plan_id = ArchitecturePlanId::new(req.plan_id.clone())
        .map_err(|e| BffError::BadRequest(format!("invalid plan id: {e}")))?;
    // Ownership check (403 when the plan belongs to another user's
    // architecture, 404 when it does not exist) before any read or write.
    let plan = get_plan_authorized(&state, &claims, &plan_id).await?;
    let plan_repo = PlanRepository::new(state.pool.clone());
    tracing::info!(
        target: "architecture.plan",
        architecture_id = %plan.architecture_id,
        plan_id = %plan_id,
        mode = ?plan.mode,
        status = ?plan.status,
        actor = %claims.sub,
        "discard_plan_architecture"
    );
    if plan.status == PlanStatus::Discarded {
        return Ok(Json(DiscardPlanResponse {
            status: "discarded".to_string(),
        }));
    }
    match plan.status {
        // `Applying` is transient by design: apply_plan rolls the row to
        // `Failed` (H4 fix) before returning any error, so a plan
        // observed as `Applying` is a real in-flight apply and the
        // operator must wait for it to terminate.
        //
        // `Applied` is the success terminal state — discarding it would
        // erase audit history.
        //
        // `Expired` is auto-set by the TTL sweeper; the row is no
        // longer actionable but still kept for audit.
        //
        // `Failed` is *discardable*: the H4 contract is that an enqueue
        // failure rolls the plan back to `Failed` so the operator has a
        // clean abandon path. Allowing discard from `Failed` lets the
        // UI clean up the row without waiting for the 15-minute TTL.
        PlanStatus::Applying | PlanStatus::Applied | PlanStatus::Expired => {
            return Err(BffError::PlanNotDiscardable {
                plan_id: plan_id.to_string(),
                current_status: plan.status,
            });
        }
        _ => {}
    }
    plan_repo
        .update_status(PlanStatusUpdateInput {
            id: plan_id,
            status: PlanStatus::Discarded,
            confirmed_by: None,
            mark_confirmed: false,
            mark_discarded: true,
            discarded_by: Some(claims.sub.clone()),
        })
        .await?;
    Ok(Json(DiscardPlanResponse {
        status: "discarded".to_string(),
    }))
}

/// Shared orchestration for `plan` and `destroy-plan`.
///
/// Steps:
/// 1. Load the topology and validate it has saved YAML.
/// 2. Capture a fresh fleet snapshot (or reload the latest persisted one
///    when `refresh_inventory=false`).
/// 3. Run static + fleet validation. If any blocking finding fires, persist
///    a `failed_validation` plan with empty changes and the finding messages
///    as warnings, then return.
/// 4. Otherwise compute the ordered diff, build the [`Plan`], and persist
///    it with the matching `requires_confirmation`/`ready_to_apply` status.
async fn generate_plan_inner(
    state: &AppState,
    claims: &Claims,
    id: ArchitectureId,
    mode: PlanMode,
    refresh_inventory: bool,
) -> Result<PlanResponse, BffError> {
    tracing::info!(
        target: "architecture.plan",
        architecture_id = %id,
        mode = ?mode,
        refresh_inventory,
        "plan_architecture"
    );

    let caller = claims.sub.as_str();
    let caller_can_apply = caller_can_apply(claims);
    // Ownership check (403 for foreign rows, 404 for missing) before any
    // read, snapshot capture, or plan/version persistence.
    let topo = get_topology_authorized(state, claims, &id).await?;
    let yaml = topo
        .latest_yaml
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if yaml.is_empty() {
        return Err(BffError::BadRequest(
            "plan requires saved topology yaml".into(),
        ));
    }

    // Capture or reload the inventory snapshot.
    let snapshot_repo =
        chv_controlplane_store::InventorySnapshotRepository::new(state.pool.clone());
    let (snapshot, snapshot_id) = if refresh_inventory {
        let provider = FleetInventoryProvider {
            nodes: state.node_repo.clone(),
            networks: state.network_repo.clone(),
            images: state.image_repo.clone(),
            deploy_allowed_for_caller: caller_can_apply,
        };
        let snapshot = chv_architecture_reconcile::capture(&provider, "bff/plan")
            .await
            .map_err(|e| BffError::Internal(format!("capture inventory: {e}")))?;
        let snap_id = InventorySnapshotId::new(chv_common::gen_resource_id())
            .map_err(|e| BffError::Internal(format!("mint inventory snapshot id: {e}")))?;
        let snapshot_json = serde_json::to_string(&snapshot)?;
        let summary_json = Some(
            serde_json::json!({
                "totals": {
                    "hosts": snapshot.nodes.len(),
                    "networks": snapshot.networks.len(),
                    "datastores": snapshot.datastores.len(),
                    "images": snapshot.images.len(),
                    "backup_targets": snapshot.backup_targets.len(),
                },
                "backup_targets_complete": snapshot.backup_targets_complete,
                "captured_by": "bff/plan",
            })
            .to_string(),
        );
        let saved = snapshot_repo
            .create(InventorySnapshotCreateInput {
                id: snap_id.clone(),
                source: snapshot.source.clone(),
                snapshot_json,
                summary_json,
                captured_by: Some(caller.to_string()),
            })
            .await?;
        (snapshot, saved.id)
    } else {
        // Phase 4 keeps the no-refresh path simple: there is no
        // `latest_for_architecture` API yet, so callers explicitly opting out
        // of refresh hit a deterministic 400. Phase 5 will widen this.
        return Err(BffError::BadRequest(
            "refresh_inventory=false is not yet supported; omit or set true".into(),
        ));
    };

    // Parse the model and run pure-data fleet checks. Static validation is
    // a separate user-initiated action (`POST /v1/architectures/validate`)
    // that gates whether plan is even allowed; running it again here would
    // duplicate findings already surfaced via `last_validation_status`.
    // Phase 5 may layer it back in once the contract calls for it.
    let model = parse_arch_yaml(yaml)
        .map_err(|e| BffError::BadRequest(format!("latest_yaml parse failed: {e}")))?;
    let fleet_findings: Vec<Finding> = check_fleet(&model, &snapshot);

    let blocking: Vec<&Finding> = fleet_findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .collect();
    let warning_messages: Vec<String> = fleet_findings
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .map(|f| f.message.clone())
        .collect();

    // Resolve / mint the architecture_version_id used by the plan FK. If
    // the topology already carries one, reuse it; otherwise create a fresh
    // version row from the current YAML so the plan FK resolves cleanly.
    let version_repo = VersionRepository::new(state.pool.clone());
    let version_id = match &topo.latest_version_id {
        Some(v) => v.clone(),
        None => {
            let new_id = ArchitectureVersionId::new(chv_common::gen_resource_id())
                .map_err(|e| BffError::Internal(format!("mint version id: {e}")))?;
            version_repo
                .create(VersionCreateInput {
                    id: new_id.clone(),
                    architecture_id: id.clone(),
                    version_number: topo.version_number,
                    yaml_content: yaml.to_string(),
                    design_graph_json: topo.design_graph_json.clone(),
                    normalized_model_json: None,
                    change_summary: Some("auto-created by bff/plan".to_string()),
                    created_by: Some(caller.to_string()),
                })
                .await?;
            new_id
        }
    };

    let plan_repo = PlanRepository::new(state.pool.clone());
    let plan_id = ArchitecturePlanId::new(chv_common::gen_resource_id())
        .map_err(|e| BffError::Internal(format!("mint plan id: {e}")))?;

    // Capture the version id string up front; the FK move into PlanCreateInput
    // consumes `version_id`, but the PlanResponse echoes it back to the
    // client so the UI can pin a subsequent apply call to this exact version.
    let version_id_string = version_id.as_str().to_string();
    let architecture_version = topo.version_number;

    let now = state.clock.now();
    let expires_at = now + chrono::Duration::minutes(15);

    if !blocking.is_empty() {
        // Failed-validation plan: persist with empty changes, warnings carry
        // the blocking finding messages so the UI can render them.
        let blocking_messages: Vec<String> = blocking.iter().map(|f| f.message.clone()).collect();
        let summary = chv_architecture_reconcile::PlanSummary::from_changes(
            &[],
            blocking_messages.len() as u32,
        );
        let plan_struct = chv_architecture_reconcile::Plan {
            mode,
            changes: Vec::new(),
            summary: summary.clone(),
            warnings: blocking_messages.clone(),
        };
        let plan_json = serde_json::to_string(&plan_struct)?;
        let summary_json = serde_json::to_string(&summary)?;
        let persisted = plan_repo
            .create(PlanCreateInput {
                id: plan_id.clone(),
                architecture_id: id.clone(),
                architecture_version_id: version_id,
                inventory_snapshot_id: Some(snapshot_id),
                mode,
                status: PlanStatus::FailedValidation,
                plan_json: Some(plan_json),
                summary_json: Some(summary_json),
                created_by: Some(caller.to_string()),
                expires_at,
            })
            .await?;
        tracing::info!(
            target: "architecture.plan",
            architecture_id = %id,
            plan_id = %persisted.id,
            mode = ?mode,
            status = ?PlanStatus::FailedValidation,
            "plan generated"
        );
        return Ok(PlanResponse {
            plan_id: persisted.id.into_inner(),
            architecture_id: id.into_inner(),
            architecture_version,
            architecture_version_id: version_id_string,
            status: PlanStatus::FailedValidation,
            mode,
            summary,
            changes: Vec::new(),
            warnings: blocking_messages,
            expires_at: persisted.expires_at,
            created_at: persisted.created_at,
        });
    }

    // Success path: build a real plan.
    let plan_struct =
        chv_architecture_reconcile::build_plan(&model, &snapshot, mode, warning_messages);
    let status = if plan_struct.changes.iter().any(|c| c.requires_confirmation) {
        PlanStatus::RequiresConfirmation
    } else {
        PlanStatus::ReadyToApply
    };
    let plan_json = serde_json::to_string(&plan_struct)?;
    let summary_json = serde_json::to_string(&plan_struct.summary)?;
    let persisted = plan_repo
        .create(PlanCreateInput {
            id: plan_id.clone(),
            architecture_id: id.clone(),
            architecture_version_id: version_id,
            inventory_snapshot_id: Some(snapshot_id),
            mode,
            status,
            plan_json: Some(plan_json),
            summary_json: Some(summary_json),
            created_by: Some(caller.to_string()),
            expires_at,
        })
        .await?;

    tracing::info!(
        target: "architecture.plan",
        architecture_id = %id,
        plan_id = %persisted.id,
        mode = ?mode,
        status = ?status,
        "plan generated"
    );

    Ok(PlanResponse {
        plan_id: persisted.id.into_inner(),
        architecture_id: id.into_inner(),
        architecture_version,
        architecture_version_id: version_id_string,
        status,
        mode,
        summary: plan_struct.summary,
        changes: plan_struct.changes,
        warnings: plan_struct.warnings,
        expires_at: persisted.expires_at,
        created_at: persisted.created_at,
    })
}

// ---------------------------------------------------------------------------
// Phase 3 DTOs — check-fleet
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckFleetRequest {
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckFleetResponse {
    pub status: ValidationStatusKind,
    pub inventory_snapshot_id: String,
    pub checked_at: String,
    pub findings: Vec<Finding>,
}

/// Mirrors `chv_architecture_validate::ValidationStatusKind` so the BFF
/// surface owns the wire enum without re-exporting it. Keeping the
/// duplication local keeps the contract stable if the validator tag
/// changes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatusKind {
    Valid,
    Warning,
    Invalid,
}

/// Phase 3 fleet check. Captures a fresh inventory snapshot, runs
/// layer-2 checks against the topology's `latest_yaml`, persists the
/// snapshot, updates `last_fleet_check_status`, and returns the
/// findings.
///
/// Role-gated to Operator+ via the router. The `caller_can_deploy`
/// signal passed to the inventory provider is `true` until the
/// `architecture:apply` permission ships in Phase 4 — without it we
/// would emit `PERMISSION_DENIED_DEPLOY` on every call.
pub async fn check_fleet_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<CheckFleetRequest>,
) -> Result<Json<CheckFleetResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    let id = parse_id(&req.id)?;
    tracing::info!(architecture_id = %id, "check_fleet_architecture");

    let topo = get_topology_authorized(&state, &claims, &id).await?;
    // NULL-owned starter rows are read-only templates for non-admins (403),
    // and a production-tagged row is admin-only — the fleet check persists
    // `last_fleet_check_status` onto a row the caller may not write.
    require_owner_or_admin(&claims, topo.owner_user_id.as_deref())?;
    let role = Role::parse(&claims.role).ok_or_else(|| {
        BffError::Internal("operator middleware passed but role string is unparseable".into())
    })?;
    enforce_production_guard(topo.environment.as_deref(), role)?;
    let yaml = topo
        .latest_yaml
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if yaml.is_empty() {
        return Err(BffError::BadRequest(
            "topology has no latest_yaml; import or generate YAML first".into(),
        ));
    }

    // Capture the fleet snapshot. `deploy_allowed_for_caller` is plumbed
    // from the caller's role so the drift `PermissionChanged` heuristic in
    // `chv-architecture-reconcile` reflects the live caller, not a hardcoded
    // `true` (closes C3 — operators were getting false reassurance).
    let provider = FleetInventoryProvider {
        nodes: state.node_repo.clone(),
        networks: state.network_repo.clone(),
        images: state.image_repo.clone(),
        deploy_allowed_for_caller: caller_can_apply(&claims),
    };
    let snapshot = chv_architecture_reconcile::capture(&provider, "bff/check-fleet")
        .await
        .map_err(|e| BffError::Internal(format!("capture inventory: {e}")))?;

    // Persist the snapshot for plan/drift downstream.
    let snapshot_id = InventorySnapshotId::new(chv_common::gen_resource_id())
        .map_err(|e| BffError::Internal(format!("mint inventory snapshot id: {e}")))?;
    let snapshot_json = serde_json::to_string(&snapshot)?;
    let summary_json = Some(
        serde_json::json!({
            "totals": {
                "hosts": snapshot.nodes.len(),
                "networks": snapshot.networks.len(),
                "datastores": snapshot.datastores.len(),
                "images": snapshot.images.len(),
                "backup_targets": snapshot.backup_targets.len(),
            },
            "backup_targets_complete": snapshot.backup_targets_complete,
            "captured_by": "bff/check-fleet",
        })
        .to_string(),
    );
    let persisted = state.topology_repo.pool();
    // Use the existing snapshot repository so persistence stays
    // single-source-of-truth.
    let snapshot_repo = chv_controlplane_store::InventorySnapshotRepository::new(persisted.clone());
    let saved = snapshot_repo
        .create(InventorySnapshotCreateInput {
            id: snapshot_id.clone(),
            source: snapshot.source.clone(),
            snapshot_json,
            summary_json,
            captured_by: Some(claims.sub.clone()),
        })
        .await?;

    // Parse the model and run pure-data fleet checks.
    let model = parse_arch_yaml(yaml)
        .map_err(|e| BffError::BadRequest(format!("latest_yaml parse failed: {e}")))?;
    let findings: Vec<Finding> = check_fleet(&model, &snapshot);

    // Compute status from finding severities. Errors → invalid;
    // warnings → warning; clean → valid.
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for f in &findings {
        match f.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => {}
        }
    }
    let status = if errors > 0 {
        ValidationStatusKind::Invalid
    } else if warnings > 0 {
        ValidationStatusKind::Warning
    } else {
        ValidationStatusKind::Valid
    };

    // Persist last_fleet_check_status. Failed when any error finding,
    // Passed otherwise (warnings still count as a successful check).
    let new_status = if errors > 0 {
        FleetCheckStatus::Failed
    } else {
        FleetCheckStatus::Passed
    };
    let scope = caller_visible_scope(&claims)?;
    let update_result = state
        .topology_repo
        .update(
            TopologyUpdateInput {
                id: id.clone(),
                expected_version: topo.version_number,
                display_name: None,
                description: None,
                environment: None,
                status: None,
                design_graph_json: None,
                latest_yaml: None,
                latest_version_id: None,
                last_validation_status: None,
                last_fleet_check_status: Some(new_status),
            },
            scope.as_deref(),
        )
        .await;
    match update_result {
        Ok(_) => {}
        Err(StoreError::StaleVersion {
            current, expected, ..
        }) => {
            return Err(BffError::Conflict(format!(
                "topology was modified concurrently while persisting fleet check status: client sent {expected}, current is {current}"
            )));
        }
        Err(other) => return Err(other.into()),
    }

    Ok(Json(CheckFleetResponse {
        status,
        inventory_snapshot_id: saved.id.into_inner(),
        checked_at: snapshot.captured_at.to_rfc3339(),
        findings,
    }))
}

// ---------------------------------------------------------------------------
// Phase 5 DTOs — apply / destroy
// ---------------------------------------------------------------------------

/// Body for `POST /v1/architectures/apply` and
/// `POST /v1/architectures/destroy`.
///
/// `confirmation.typed_name` is required for destructive plans (any
/// `Delete`/`Replace` change, or `destroy` mode); the apply path rejects
/// missing/mismatched names with `code: "MISSING_CONFIRMATION"`.
///
/// `acknowledged_warnings` is a hard gate when the plan carries warnings —
/// the apply path rejects with `code: "WARNINGS_NOT_ACKNOWLEDGED"` when it
/// is left at its `false` default and the plan has any warnings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApplyArchitectureRequest {
    pub id: String,
    pub plan_id: String,
    #[serde(default)]
    pub confirmation: ConfirmationDto,
    #[serde(default)]
    pub acknowledged_warnings: bool,
}

/// Wire-shape for the typed-name confirmation payload. Mirrors the
/// reconcile crate's [`ConfirmationToken`] but stays a separate type so the
/// JSON contract is owned by the BFF.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConfirmationDto {
    pub typed_name: Option<String>,
}

impl From<ConfirmationDto> for ConfirmationToken {
    fn from(d: ConfirmationDto) -> Self {
        Self {
            typed_name: d.typed_name,
        }
    }
}

/// Response shape for `apply` and `destroy`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApplyResponse {
    pub run_id: String,
    pub task_id: Option<String>,
    pub status: String,
    pub started_at: Option<String>,
    pub architecture_id: String,
    pub architecture_version_id: String,
    pub plan_id: String,
}

// ---------------------------------------------------------------------------
// Phase 5 production-environment guard
// ---------------------------------------------------------------------------

/// Normalize and decide whether a string is the production environment.
///
/// We trim leading/trailing whitespace (covers UI-side copy/paste with a
/// stray newline) and ASCII-lowercase the result so casing variants
/// ("Production", "PROD", " prod\n") all map to the same answer.
///
/// Note: ASCII-only on purpose. A Cyrillic-look-alike like "prоduction"
/// (with Cyrillic 'о', U+043E) deliberately does NOT match — only the
/// Latin spelling is treated as production. Documented intent: the guard
/// is a courtesy for typo-tolerance, not a defence against deliberate
/// homograph spoofing (which would be defeated at the input-validation
/// layer instead).
fn is_production_environment(environment: Option<&str>) -> bool {
    match environment {
        Some(env) => {
            let normalized = env.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "production" | "prod")
        }
        None => false,
    }
}

/// Reject apply/destroy attempts against a production-tagged architecture
/// when the caller is not an admin. The codebase has only Viewer/Operator/
/// Admin roles today; spec §architecture-designer/contracts hints at a
/// future fine-grained `architecture:apply:production` permission, but this
/// guard is the conservative bridge until that lands.
///
/// The decision is made against the *persisted* topology environment (the
/// `architecture` row loaded by the caller's scoped read) — never against
/// any field of the apply/destroy request, which carries none. A `None`
/// persisted environment, or any non-`production`/`prod` value, passes
/// through. Admins always pass through. Operators (and viewers) hit a
/// dedicated 403 with `code: "PRODUCTION_REQUIRES_ADMIN"` so the UI can
/// route to "ask an admin to apply this".
///
/// Security F7 (environment-null bypass): the companion check in
/// `update_architecture` blocks non-admins from *un-tagging* a persisted
/// production row (including via `environment: null`/absent), so the
/// apply-time guard here cannot be evaded by first relabelling the row.
pub(crate) fn enforce_production_guard(
    environment: Option<&str>,
    role: Role,
) -> Result<(), BffError> {
    if is_production_environment(environment) && !role.meets(Role::Admin) {
        return Err(BffError::ProductionRequiresAdmin {
            environment: environment.unwrap_or("").trim().to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 5 handlers — apply / destroy
// ---------------------------------------------------------------------------

/// `POST /v1/architectures/apply` — turn a `ready_to_apply` plan into a
/// queued [`chv_controlplane_types::architecture::ArchitectureApplyRun`]
/// and idempotent per-change Operations.
///
/// Operator role is required at the routing layer; the
/// production-environment guard escalates to Admin for
/// `environment ∈ {"production", "prod"}`. Non-prod environments are
/// applyable by any operator.
pub async fn apply_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<ApplyArchitectureRequest>,
) -> Result<Json<ApplyResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    apply_inner(&state, &claims, req, /*destroy_mode=*/ false).await
}

/// `POST /v1/architectures/destroy` — same shape as `apply_architecture`,
/// but rejects with 400 when the plan is not in `Destroy` mode. The
/// reconcile crate's typed-name confirmation guard fires identically for
/// both paths because every destroy plan is destructive.
pub async fn destroy_architecture(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<ApplyArchitectureRequest>,
) -> Result<Json<ApplyResponse>, BffError> {
    require_operator_or_admin(&claims)?;
    apply_inner(&state, &claims, req, /*destroy_mode=*/ true).await
}

/// Shared implementation for `apply` and `destroy`. The only behavioural
/// difference between the two endpoints is the destroy-mode pre-condition
/// check; everything else (production guard, plan parse, reconcile call,
/// metrics, tracing) is identical.
async fn apply_inner(
    state: &AppState,
    claims: &crate::auth::Claims,
    req: ApplyArchitectureRequest,
    destroy_mode: bool,
) -> Result<Json<ApplyResponse>, BffError> {
    record_apply_status(ApplyStatusLabel::Started);
    let timer = ApplyTimer::start();
    match apply_inner_core(state, claims, req, destroy_mode).await {
        Ok(resp) => {
            record_apply_status(ApplyStatusLabel::Enqueued);
            timer.observe();
            Ok(resp)
        }
        Err(err) => {
            record_apply_status(ApplyStatusLabel::Failed);
            timer.observe();
            Err(err)
        }
    }
}

async fn apply_inner_core(
    state: &AppState,
    claims: &crate::auth::Claims,
    req: ApplyArchitectureRequest,
    destroy_mode: bool,
) -> Result<Json<ApplyResponse>, BffError> {
    let architecture_id = parse_id(&req.id)?;
    let plan_id = ArchitecturePlanId::new(req.plan_id.clone())
        .map_err(|e| BffError::BadRequest(format!("invalid plan id: {e}")))?;
    let role = Role::parse(&claims.role).unwrap_or(Role::Viewer);

    // 1. Look up architecture with ownership check (403 for foreign rows,
    //    404 if missing).
    let architecture = get_topology_authorized(state, claims, &architecture_id).await?;

    // 2. Look up plan with ownership check — a plan belonging to another
    //    user's architecture answers 403, not its contents.
    let plan_repo = PlanRepository::new(state.pool.clone());
    let plan_record = get_plan_authorized(state, claims, &plan_id).await?;

    // 3. Verify the plan was generated against this architecture and the
    //    current `latest_version_id`. A version-drift mismatch is a 409 so
    //    the UI can re-run /plan rather than silently widening the apply
    //    window.
    //
    // Phase-4 plans may run against a topology that has no
    // `latest_version_id` yet (the plan handler mints a fresh version row
    // but does not back-fill `topology.latest_version_id`). In that case
    // there is no recorded "current version" to drift-check against, so
    // we trust the plan's version reference.
    if plan_record.architecture_id != architecture.id {
        return Err(BffError::Conflict(format!(
            "plan {plan_id} belongs to architecture {} not {}",
            plan_record.architecture_id, architecture.id
        )));
    }
    if let Some(latest_version_id) = architecture.latest_version_id.as_ref() {
        if plan_record.architecture_version_id != *latest_version_id {
            return Err(BffError::PlanNotApplicable {
                plan_id: plan_id.to_string(),
                current_status: plan_record.status.as_str().to_string(),
                reason: Some("version_drift".to_string()),
            });
        }
    } else {
        // `latest_version_id` is unset (Phase-4 plan handler does not
        // back-fill the topology row). Compare numeric `version_number` if
        // we can. The plan row records `architecture_version_number` since
        // Phase 4; without that, log a warn and trust the plan.
        tracing::warn!(
            target: "architecture.apply",
            architecture_id = %architecture.id,
            plan_id = %plan_id,
            "skipping version-drift check: topology.latest_version_id is None — see Phase-7 hardening"
        );
    }

    // 4. Production-environment guard. Operator hits a clean
    //    PRODUCTION_REQUIRES_ADMIN; admin passes through.
    enforce_production_guard(architecture.environment.as_deref(), role)?;

    // 5. Deserialize the persisted plan body. Phase 4 always writes a non-
    //    empty `plan_json`; treat its absence as an internal error rather
    //    than a 4xx — the row is corrupt if it lacks the body.
    let plan_json = plan_record
        .plan_json
        .as_deref()
        .ok_or_else(|| BffError::Internal(format!("plan {plan_id} has no persisted plan_json")))?;
    let plan: chv_architecture_reconcile::Plan = serde_json::from_str(plan_json).map_err(|e| {
        BffError::Internal(format!(
            "failed to deserialize plan_json for {plan_id}: {e}"
        ))
    })?;

    // 6. Destroy-mode contract: the destroy endpoint must only accept plans
    //    generated with `mode = destroy`. Apply mode plans hit a 400 here so
    //    the UI knows to call /destroy-plan first.
    if destroy_mode && plan.mode != PlanMode::Destroy {
        return Err(BffError::BadRequest(format!(
            "plan {plan_id} has mode {:?}; destroy endpoint requires Destroy-mode plan",
            plan.mode
        )));
    }
    if !destroy_mode && plan.mode == PlanMode::Destroy {
        return Err(BffError::BadRequest(format!(
            "plan {plan_id} has mode Destroy; use the /destroy endpoint instead",
        )));
    }

    let environment_for_log = architecture.environment.clone();
    tracing::info!(
        target: "architecture.apply",
        architecture_id = %architecture.id,
        version_id = %plan_record.architecture_version_id,
        plan_id = %plan_id,
        environment = environment_for_log.as_deref().unwrap_or(""),
        destroy_mode,
        "apply_plan invoked"
    );

    // 7. Build the reconcile-side context and call `apply_plan`.
    let ctx = ApplyContext {
        architecture_id: architecture.id.clone(),
        architecture_version_id: plan_record.architecture_version_id.clone(),
        topology_name: architecture.name.clone(),
        environment: architecture.environment.clone(),
        plan_id: plan_id.clone(),
        requested_by: Some(claims.sub.clone()),
        confirmation: req.confirmation.into(),
        acknowledged_warnings: req.acknowledged_warnings,
        topology_version: architecture.version_number,
    };

    let outcome: ApplyOutcome = apply_plan(
        &plan,
        &plan_record,
        &state.operation_repo,
        state.apply_runs.as_ref(),
        &plan_repo,
        &state.topology_repo,
        &ctx,
        state.clock.as_ref(),
    )
    .await
    .map_err(|err: ApplyError| {
        // Log failures with the error type so dashboards can split out
        // 4xx pre-condition violations from 5xx store failures without
        // matching on bodies.
        tracing::warn!(
            target: "architecture.apply",
            architecture_id = %architecture.id,
            plan_id = %plan_id,
            environment = environment_for_log.as_deref().unwrap_or(""),
            error = %err,
            "apply_plan failed"
        );
        BffError::from(err)
    })?;

    tracing::info!(
        target: "architecture.apply",
        architecture_id = %architecture.id,
        version_id = %plan_record.architecture_version_id,
        plan_id = %plan_id,
        run_id = %outcome.run.id,
        environment = environment_for_log.as_deref().unwrap_or(""),
        queued = outcome.queued_operations.len(),
        skipped = outcome.skipped_operations.len(),
        "apply_plan succeeded"
    );

    Ok(Json(ApplyResponse {
        run_id: outcome.run.id.as_str().to_string(),
        task_id: outcome.run.task_id.clone(),
        status: outcome.run.status.as_str().to_string(),
        started_at: outcome.run.started_at.map(|d| d.to_rfc3339()),
        architecture_id: architecture.id.into_inner(),
        architecture_version_id: plan_record.architecture_version_id.into_inner(),
        plan_id: plan_id.into_inner(),
    }))
}

// ---------------------------------------------------------------------------
// Drift detection (Phase 6)
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/architectures/drift`.
///
/// `force_refresh` (default `false`) skips the 5-minute cache and always
/// re-captures the live fleet snapshot + recomputes drift.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DriftRequest {
    pub id: String,
    #[serde(default)]
    pub force_refresh: bool,
}

/// Response body for `POST /v1/architectures/drift`.
///
/// `drift_report_id` is populated whenever a row was returned (cache hit
/// or fresh persist). `cache_hit` is `true` iff the response was served
/// from the most-recent persisted report without recomputing. When
/// `status == CheckFailed`, `error_message` carries the underlying
/// failure (snapshot capture or YAML parse) — `findings` is then empty.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DriftResponse {
    pub drift_report_id: Option<String>,
    pub status: DriftStatus,
    pub findings: Vec<DriftFinding>,
    pub summary: DriftSummary,
    pub baseline_version_id: String,
    pub computed_at: String,
    pub cache_hit: bool,
    pub error_message: Option<String>,
}

/// Cache TTL for drift reports. Re-running the snapshot capture + drift
/// compute against a fleet of cluster scale is cheap (low ms), but the
/// downstream UI hits this endpoint on every dashboard mount, so a small
/// TTL stops a wave of identical requests after a successful compute.
///
/// Exposed `pub` so integration tests at the crate boundary can reference
/// the same constant the handler uses (no magic numbers in the test
/// fixtures).
pub const DRIFT_CACHE_TTL_SECS: i64 = 300;

/// Phase 6: real handler for `POST /v1/architectures/drift`.
///
/// Mounted under viewer middleware (drift is read-only). Algorithm:
///
/// 1. Look up the architecture (404 if missing).
/// 2. Resolve `baseline_version_id` from `latest_version_id`, falling
///    back to the topology's `version_number` if unset (Phase-5 carryover
///    pattern).
/// 3. If `!force_refresh`, fetch the most-recent persisted drift report
///    for this architecture. If it's within [`DRIFT_CACHE_TTL_SECS`],
///    return it as `cache_hit = true`.
/// 4. Otherwise, capture a live fleet snapshot and run [`compute_drift`].
///    If snapshot capture or YAML parse fails, persist a `check_failed`
///    drift report and return 200 with that status. Only return 502
///    `DRIFT_CHECK_FAILED` if even persistence fails.
/// 5. On a successful compute, persist the report and return it with
///    `cache_hit = false`.
pub async fn get_architecture_drift(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<DriftRequest>,
) -> Result<Json<DriftResponse>, BffError> {
    let architecture_id = parse_id(&req.id)?;

    tracing::info!(
        target: "architecture.drift",
        architecture_id = %architecture_id,
        force_refresh = req.force_refresh,
        "architecture.drift.invoked"
    );

    // 1. Look up the architecture with ownership check (403 for foreign
    //    rows, 404 if missing).
    let topo = get_topology_authorized(&state, &claims, &architecture_id).await?;

    // 2. Resolve baseline_version_id. The drift_reports row carries a FK
    //    onto `architecture_versions(id)` so we must persist a real version
    //    row when the topology has none. Phase-4 plan handler back-fills
    //    `latest_version_id` whenever it mints a fresh version row, so this
    //    branch only fires on architectures that were created but never
    //    planned. Reuse an existing version row if one already matches the
    //    topology's `version_number` to avoid the UNIQUE(architecture_id,
    //    version_number) collision on subsequent drift calls.
    let version_repo = VersionRepository::new(state.pool.clone());
    let scope = caller_visible_scope(&claims)?;
    let baseline_version_id = match topo.latest_version_id.as_ref() {
        Some(v) => v.clone(),
        None => {
            // Look for an existing version row with this architecture's
            // current version_number first.
            let existing = version_repo
                .list_for_architecture(&architecture_id, scope.as_deref())
                .await?;
            if let Some(matching) = existing
                .into_iter()
                .find(|v| v.version_number == topo.version_number)
            {
                matching.id
            } else {
                let new_id = ArchitectureVersionId::new(chv_common::gen_resource_id())
                    .map_err(|e| BffError::Internal(format!("mint baseline version id: {e}")))?;
                let yaml = topo.latest_yaml.clone().unwrap_or_default();
                version_repo
                    .create(VersionCreateInput {
                        id: new_id.clone(),
                        architecture_id: architecture_id.clone(),
                        version_number: topo.version_number,
                        yaml_content: yaml,
                        design_graph_json: topo.design_graph_json.clone(),
                        normalized_model_json: None,
                        change_summary: Some("auto-created by bff/drift".to_string()),
                        created_by: None,
                    })
                    .await?;
                new_id
            }
        }
    };

    // 3. Cache lookup: most-recent drift_report row, served if it falls
    //    within the TTL window. Uses the indexed point-query
    //    `most_recent_for_architecture` so the hot path is a `LIMIT 1`
    //    seek on `architecture_drift_reports_architecture_id_created_at_idx`,
    //    not a full-list scan.
    if !req.force_refresh {
        let latest = state
            .drift_reports
            .most_recent_for_architecture(&architecture_id, scope.as_deref())
            .await?;
        if let Some(latest) = latest {
            let age_secs = state
                .clock
                .now()
                .signed_duration_since(latest.created_at)
                .num_seconds();
            // A negative `age_secs` means the persisted `created_at` is
            // ahead of the configured clock — typically a `ManualClock`
            // running behind `strftime('now')`, but also a defensible
            // signal in production: the row is at least as fresh as
            // `now`, so treat it as a cache hit. The forward bound stays
            // strict against the TTL.
            if age_secs < DRIFT_CACHE_TTL_SECS {
                let summary = latest
                    .summary_json
                    .as_deref()
                    .map(serde_json::from_str::<DriftSummary>)
                    .transpose()
                    .map_err(|e| {
                        BffError::Internal(format!(
                            "drift_report {} has unparsable summary_json: {e}",
                            latest.id
                        ))
                    })?
                    .unwrap_or_default();
                let findings = latest
                    .findings_json
                    .as_deref()
                    .map(serde_json::from_str::<Vec<DriftFinding>>)
                    .transpose()
                    .map_err(|e| {
                        BffError::Internal(format!(
                            "drift_report {} has unparsable findings_json: {e}",
                            latest.id
                        ))
                    })?
                    .unwrap_or_default();
                record_drift_status(DriftStatusLabel::CacheHit);
                tracing::info!(
                    target: "architecture.drift",
                    architecture_id = %architecture_id,
                    report_id = %latest.id,
                    findings_count = findings.len(),
                    age_secs,
                    "architecture.drift.cache_hit"
                );
                return Ok(Json(DriftResponse {
                    drift_report_id: Some(latest.id.into_inner()),
                    status: latest.status,
                    findings,
                    summary,
                    baseline_version_id: latest.baseline_version_id.into_inner(),
                    computed_at: latest.created_at.to_rfc3339(),
                    cache_hit: true,
                    error_message: None,
                }));
            }
        }
    }

    // 4. Fresh compute. Two failure modes flow through the
    //    `check_failed` persist path: snapshot capture errors and YAML
    //    parse errors. Anything else (e.g. drift_reports.create failing)
    //    is a 502 DRIFT_CHECK_FAILED.
    let provider = FleetInventoryProvider {
        nodes: state.node_repo.clone(),
        networks: state.network_repo.clone(),
        images: state.image_repo.clone(),
        deploy_allowed_for_caller: caller_can_apply(&claims),
    };

    let yaml = topo
        .latest_yaml
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();

    let compute_outcome: Result<DriftReport, String> = if yaml.is_empty() {
        Err("architecture has no latest_yaml; nothing to drift-check against".to_string())
    } else {
        match chv_architecture_reconcile::capture(&provider, "bff/drift").await {
            Err(e) => Err(format!("snapshot capture failed: {e}")),
            Ok(snapshot) => match parse_arch_yaml(yaml) {
                Err(e) => Err(format!("baseline yaml parse failed: {e}")),
                Ok(model) => Ok(compute_drift(&model, &snapshot)),
            },
        }
    };

    let drift_report_id = ArchitectureDriftReportId::new(chv_common::gen_resource_id())
        .map_err(|e| BffError::Internal(format!("mint drift_report id: {e}")))?;

    match compute_outcome {
        Ok(report) => {
            let summary_json = serde_json::to_string(&report.summary)?;
            let findings_json = serde_json::to_string(&report.findings)?;
            let persisted = state
                .drift_reports
                .create(DriftReportCreateInput {
                    id: drift_report_id,
                    architecture_id: architecture_id.clone(),
                    baseline_version_id: baseline_version_id.clone(),
                    inventory_snapshot_id: None,
                    status: report.status,
                    summary_json: Some(summary_json),
                    findings_json: Some(findings_json),
                })
                .await
                .map_err(|e| BffError::DriftCheckFailed {
                    architecture_id: architecture_id.to_string(),
                    message: format!("persist drift report: {e}"),
                })?;

            let label = match report.status {
                DriftStatus::NoDrift => DriftStatusLabel::NoDrift,
                DriftStatus::Drifted => DriftStatusLabel::Drifted,
                DriftStatus::Unknown => DriftStatusLabel::Unknown,
                DriftStatus::CheckFailed => DriftStatusLabel::CheckFailed,
            };
            record_drift_status(label);

            // Transition the topology row's lifecycle `status` column so
            // the dashboard badge matches the freshly-persisted drift
            // result. We only move the column for `NoDrift` (back to
            // `applied`) and `Drifted` — `Unknown` and `CheckFailed` mean
            // we genuinely do not know whether the deployed fleet matches
            // the baseline, so leaving the existing badge alone is more
            // honest than overwriting it with a guess. Best-effort: a
            // failure here is logged but does not fail the drift response
            // because the drift_report is the durable signal.
            if let Some(target) = drift_status_to_topology_status(report.status) {
                if let Err(err) = chv_architecture_reconcile::apply::set_topology_terminal_status(
                    &state.topology_repo,
                    &architecture_id,
                    target,
                )
                .await
                {
                    tracing::warn!(
                        target: "architecture.drift",
                        architecture_id = %architecture_id,
                        to_status = target.as_str(),
                        error = %err,
                        "failed to transition topology lifecycle status from drift result"
                    );
                }
            }

            tracing::info!(
                target: "architecture.drift",
                architecture_id = %architecture_id,
                report_id = %persisted.id,
                findings_count = report.findings.len(),
                status = report.status.as_str(),
                "architecture.drift.computed"
            );

            Ok(Json(DriftResponse {
                drift_report_id: Some(persisted.id.into_inner()),
                status: report.status,
                findings: report.findings,
                summary: report.summary,
                baseline_version_id: persisted.baseline_version_id.into_inner(),
                computed_at: persisted.created_at.to_rfc3339(),
                cache_hit: false,
                error_message: None,
            }))
        }
        Err(message) => {
            // Persist a `check_failed` row with no findings so the UI can
            // render the failure inline. Empty summary + empty findings
            // keeps the wire shape uniform with the success path.
            let empty_summary = DriftSummary::default();
            let summary_json = serde_json::to_string(&empty_summary)?;
            let findings_json = serde_json::to_string::<Vec<DriftFinding>>(&Vec::new())?;
            let persisted = state
                .drift_reports
                .create(DriftReportCreateInput {
                    id: drift_report_id,
                    architecture_id: architecture_id.clone(),
                    baseline_version_id: baseline_version_id.clone(),
                    inventory_snapshot_id: None,
                    status: DriftStatus::CheckFailed,
                    summary_json: Some(summary_json),
                    findings_json: Some(findings_json),
                })
                .await
                .map_err(|e| BffError::DriftCheckFailed {
                    architecture_id: architecture_id.to_string(),
                    message: format!("persist check_failed report: {e}"),
                })?;

            record_drift_status(DriftStatusLabel::CheckFailed);
            tracing::warn!(
                target: "architecture.drift",
                architecture_id = %architecture_id,
                report_id = %persisted.id,
                error = %message,
                "architecture.drift.failed"
            );

            Ok(Json(DriftResponse {
                drift_report_id: Some(persisted.id.into_inner()),
                status: DriftStatus::CheckFailed,
                findings: Vec::new(),
                summary: empty_summary,
                baseline_version_id: persisted.baseline_version_id.into_inner(),
                computed_at: persisted.created_at.to_rfc3339(),
                cache_hit: false,
                error_message: Some(message),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Apply-run history (Phase 5 carryover, wired in Phase 6)
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/architectures/runs/list`.
///
/// The wire field is `id` (not `architecture_id`) for parity with every
/// other architecture-scoped DTO in this module (`GetArchitectureRequest`,
/// `DriftRequest`, `PlanArchitectureRequest`, `ApplyArchitectureRequest`).
/// Reviewers flagged the original `architecture_id` name as a stray
/// inconsistency in the otherwise-uniform `{ id }` convention.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ListApplyRunsRequest {
    pub id: String,
}

/// Response body for `POST /v1/architectures/runs/list`. Runs are returned
/// in `created_at DESC` order (most-recent first).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ListApplyRunsResponse {
    pub runs: Vec<ApplyRunDto>,
}

/// Wire shape for an apply-run row. RFC3339 strings for timestamps so the
/// JSON wire format is timezone-explicit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApplyRunDto {
    pub id: String,
    pub architecture_id: String,
    pub architecture_version_id: String,
    pub plan_id: Option<String>,
    pub task_id: Option<String>,
    pub status: RunStatus,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub requested_by: Option<String>,
    pub result_json: Option<String>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Phase 5 carryover B1: real handler for `POST /v1/architectures/runs/list`.
///
/// Mounted under operator middleware (run history can leak operational
/// detail). Algorithm:
///
/// 1. Parse the architecture_id (400 if invalid).
/// 2. Verify the architecture exists (404 if missing).
/// 3. Call [`ApplyRunRepository::list_for_architecture`] (DESC).
/// 4. Map each row to [`ApplyRunDto`] and return.
pub async fn list_architecture_runs(
    BearerToken(claims): BearerToken,
    State(state): State<AppState>,
    Json(req): Json<ListApplyRunsRequest>,
) -> Result<Json<ListApplyRunsResponse>, BffError> {
    let architecture_id = parse_id(&req.id)?;

    // Ownership check: 403 when the architecture belongs to another user,
    // 404 when it does not exist. Without this the endpoint would leak run
    // history (task ids, error messages, requested_by) of foreign rows.
    get_topology_authorized(&state, &claims, &architecture_id).await?;

    let scope = caller_visible_scope(&claims)?;
    let runs = state
        .apply_runs
        .list_for_architecture(&architecture_id, scope.as_deref())
        .await?;
    let runs: Vec<ApplyRunDto> = runs
        .into_iter()
        .map(|r| ApplyRunDto {
            id: r.id.into_inner(),
            architecture_id: r.architecture_id.into_inner(),
            architecture_version_id: r.architecture_version_id.into_inner(),
            plan_id: r.plan_id.map(|p| p.into_inner()),
            task_id: r.task_id,
            status: r.status,
            started_at: r.started_at.map(|d| d.to_rfc3339()),
            finished_at: r.finished_at.map(|d| d.to_rfc3339()),
            requested_by: r.requested_by,
            result_json: r.result_json,
            error_message: r.error_message,
            created_at: r.created_at.to_rfc3339(),
            updated_at: r.updated_at.to_rfc3339(),
        })
        .collect();

    tracing::info!(
        target: "architecture.runs",
        architecture_id = %architecture_id,
        run_count = runs.len(),
        "architecture.runs.listed"
    );

    Ok(Json(ListApplyRunsResponse { runs }))
}

pub async fn list_architecture_versions(
    BearerToken(_claims): BearerToken,
    State(_state): State<AppState>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, BffError> {
    Err(BffError::NotImplemented("phase 0".into()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn parse_id(id: &str) -> Result<ArchitectureId, BffError> {
    ArchitectureId::new(id)
        .map_err(|e| BffError::BadRequest(format!("invalid architecture id: {e}")))
}

/// Map a [`DriftStatus`] (per-report outcome) to the topology lifecycle
/// `status` column the dashboard badge reads.
///
/// - `Drifted`  → `Drifted`    (the deployed fleet diverged from the baseline)
/// - `NoDrift`  → `Applied`    (re-confirms the topology is in sync)
/// - `Unknown`  → `None`       (do not overwrite — the previous status is more honest than a guess)
/// - `CheckFailed` → `None`    (snapshot capture failed; same reasoning)
///
/// Returning `None` is the signal to the caller to leave the lifecycle
/// column untouched rather than write a value that is misleading.
fn drift_status_to_topology_status(status: DriftStatus) -> Option<ArchitectureStatus> {
    match status {
        DriftStatus::Drifted => Some(ArchitectureStatus::Drifted),
        DriftStatus::NoDrift => Some(ArchitectureStatus::Applied),
        DriftStatus::Unknown | DriftStatus::CheckFailed => None,
    }
}

#[cfg(test)]
mod prod_guard_tests {
    use super::*;

    #[test]
    fn none_environment_passes_for_any_role() {
        enforce_production_guard(None, Role::Viewer).expect("None must pass for viewer");
        enforce_production_guard(None, Role::Operator).expect("None must pass for operator");
        enforce_production_guard(None, Role::Admin).expect("None must pass for admin");
    }

    #[test]
    fn staging_passes_for_operator() {
        enforce_production_guard(Some("staging"), Role::Operator)
            .expect("staging is not gated; operator must pass");
    }

    #[test]
    fn production_blocks_operator() {
        let err = enforce_production_guard(Some("production"), Role::Operator)
            .expect_err("production must block operator");
        match err {
            BffError::ProductionRequiresAdmin { environment } => {
                assert_eq!(environment, "production");
            }
            other => panic!("expected ProductionRequiresAdmin, got {other:?}"),
        }
    }

    #[test]
    fn prod_alias_blocks_operator() {
        let err = enforce_production_guard(Some("prod"), Role::Operator)
            .expect_err("'prod' alias must block operator");
        assert!(matches!(err, BffError::ProductionRequiresAdmin { .. }));
    }

    #[test]
    fn production_passes_for_admin() {
        enforce_production_guard(Some("production"), Role::Admin)
            .expect("admin must clear the production guard");
    }

    #[test]
    fn production_blocks_viewer() {
        // Viewer should never reach here in practice (admin middleware
        // gates the route) but defence-in-depth: the guard itself rejects.
        let err = enforce_production_guard(Some("production"), Role::Viewer)
            .expect_err("viewer must be blocked");
        assert!(matches!(err, BffError::ProductionRequiresAdmin { .. }));
    }
}
