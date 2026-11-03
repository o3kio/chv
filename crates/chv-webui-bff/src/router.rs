use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    http::{header, HeaderValue, Method},
    middleware,
    routing::{delete, get, patch, post},
    Router,
};
use chv_common::Clock;
use chv_controlplane_store::{
    AlertRepository, AlertRuleRepository, ApplyRunRepository, BackupRepository,
    DesiredStateRepository, DriftReportRepository, EventRepository, ImageRepository,
    NetboxProjectionConfigRepository, NetboxProjectionRunRepository, NetworkRepository,
    NodeRepository, NotificationOutboxRepository, ObservedStateRepository, OperationRepository,
    StorePool, TopologyRepository,
};
use tower_http::cors::CorsLayer;

use crate::cache::BffCache;
use crate::mutations::MutationService;

/// Which notification destinations the control plane configured
/// (ADR-027, #602 PR-6). Booleans only: the BFF never sees the
/// webhook URLs or the signing secret.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NotificationChannels {
    /// `[monitoring.notifications].webhook_url` is set.
    pub webhook: bool,
    /// `[monitoring.notifications].slack_webhook_url` is set.
    pub slack: bool,
}

impl NotificationChannels {
    /// Whether any destination exists at all.
    pub fn any(&self) -> bool {
        self.webhook || self.slack
    }

    /// The channel a delivery test should ride: the signed webhook
    /// when configured, else Slack. `None` when nothing can deliver.
    pub fn test_channel(&self) -> Option<&'static str> {
        if self.webhook {
            Some(chv_controlplane_store::CHANNEL_WEBHOOK)
        } else if self.slack {
            Some(chv_controlplane_store::CHANNEL_SLACK)
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: StorePool,
    pub node_repo: NodeRepository,
    pub operation_repo: OperationRepository,
    pub event_repo: EventRepository,
    pub alert_repo: AlertRepository,
    /// Typed alert rules + incident lifecycle (ADR-027, #602 PR-6).
    /// Wraps the `alert_rules` table; the evaluator worker executes
    /// them. Stored as `Arc` so `AppState` stays cheap to clone.
    pub alert_rules: std::sync::Arc<AlertRuleRepository>,
    /// Durable notification outbox (ADR-027, #602 PR-6): the delivery
    /// audit view and the authorized delivery test.
    pub notification_outbox: std::sync::Arc<NotificationOutboxRepository>,
    /// The `[monitoring.alerting]` rule ceiling, enforced loudly at
    /// rule creation (the config validation clamps the range).
    pub alerting_max_rules: i64,
    /// Which notification destinations `[monitoring.notifications]`
    /// configured: the delivery test rides a deliverable channel and
    /// refuses honestly when there is none. Booleans only — no
    /// secret or URL ever crosses into the BFF.
    pub notification_channels: NotificationChannels,
    pub desired_state_repo: DesiredStateRepository,
    pub observed_state_repo: ObservedStateRepository,
    pub backup_repo: BackupRepository,
    /// Architecture Designer topology repository (Phase 0).
    pub topology_repo: TopologyRepository,
    /// Read-only network rows for fleet checks (Phase 3).
    pub network_repo: NetworkRepository,
    /// Read-only image rows for fleet checks (Phase 3).
    pub image_repo: ImageRepository,
    /// Architecture Designer apply-run repository (Phase 5). Wraps the
    /// `architecture_apply_runs` table; used by the apply / destroy
    /// handlers to enqueue runs and propagate status. Stored as `Arc` so
    /// `AppState` stays cheap to clone.
    pub apply_runs: std::sync::Arc<ApplyRunRepository>,
    /// Architecture Designer drift-report repository (Phase 6). Wraps the
    /// `architecture_drift_reports` table; used by the drift handler to
    /// cache the most recent report (5-minute TTL) and persist fresh
    /// computations. Stored as `Arc` so `AppState` stays cheap to clone.
    pub drift_reports: std::sync::Arc<DriftReportRepository>,
    /// NetBox projection config repository (#239, PR 5). Wraps the
    /// `netbox_projection_config` table; used by the netbox handlers for
    /// config get/upsert/delete and the dry-run token decrypt. Stored as
    /// `Arc` so `AppState` stays cheap to clone.
    pub netbox_config: std::sync::Arc<NetboxProjectionConfigRepository>,
    /// NetBox projection run repository (#239, PR 5). Wraps the
    /// `netbox_projection_runs` table; used by the netbox handlers to
    /// enqueue export runs and read/retry run history (the PR-4 worker
    /// executes them). Stored as `Arc` so `AppState` stays cheap to
    /// clone.
    pub netbox_runs: std::sync::Arc<NetboxProjectionRunRepository>,
    pub mutations: Arc<dyn MutationService>,
    pub jwt_secret: String,
    pub agent_runtime_dir: PathBuf,
    pub cache: BffCache,
    /// Native monitoring history store (ADR-027, #602). `None` ⇒
    /// monitoring is disabled or degraded and every `/v1/monitoring/*`
    /// read answers `monitoring_unavailable` — which never means nodes
    /// or VMs are unhealthy.
    pub monitoring: Option<Arc<chv_monitoring_store::MonitoringStore>>,
    /// Shared monitoring health state (written by the ingest path and
    /// the maintenance worker, read by `/v1/monitoring/health`).
    pub monitoring_health: chv_monitoring_store::MonitoringHealth,
    /// Wall-clock abstraction. Production wires `Arc::new(SystemClock)`; tests
    /// inject `Arc::new(ManualClock::new(...))` to drive plan TTL/expiry
    /// timing deterministically. Wired in Phase 4 of Architecture Designer for
    /// `expires_at` computation on `POST /v1/architectures/plan`.
    pub clock: Arc<dyn Clock>,
}

/// The BFF's `/v1` surface WITHOUT the audit, metrics, and correlation
/// wrappers.
///
/// doc-hidden on purpose: the only legitimate caller is the control
/// plane's `admin_router_with_guest_agents`, which mounts this inside
/// the full production router and then applies correlation + metrics +
/// audit around **everything** — its own extra privileged surfaces
/// (`/admin/*`, `/api/v1/*`, `/internal/*`, the guest-agent browser
/// routes) and the router fallback — so no privileged mutation can
/// bypass the trail and nothing is audited twice (#658 review H1/H2).
/// Every other caller (tests, embedders) must use [`bff_router`],
/// which adds the same wrappers in the same order.
#[doc(hidden)]
pub fn bff_router_unaudited(state: AppState) -> Router<AppState> {
    // Unauthenticated
    let login = Router::new()
        .route("/v1/auth/login", post(crate::handlers::auth::login))
        .route("/v1/auth/logout", post(crate::handlers::auth::logout));

    // Authenticated change-password endpoint. Lives in its own sub-router so
    // it is reachable even when the caller's JWT carries
    // `must_change_password=true` (the role middleware in `viewer`/`operator`/
    // `admin` would otherwise short-circuit with 403 PASSWORD_CHANGE_REQUIRED).
    // The handler itself extracts `BearerToken`, so authentication still
    // happens — there is no auth bypass.
    let change_password = Router::new().route(
        "/v1/auth/change-password",
        post(crate::handlers::auth::change_password),
    );

    // Viewer — read-only and personal endpoints
    let viewer = Router::new()
        .route(
            "/v1/overview",
            post(crate::handlers::overview::get_overview),
        )
        // Operator health surface (chvctl health ...) — read-only aggregates.
        .route("/v1/health", get(crate::handlers::health::health))
        .route(
            "/v1/cluster/health",
            get(crate::handlers::health::cluster_health),
        )
        .route(
            "/v1/nodes/:node_id/health",
            get(crate::handlers::health::node_health),
        )
        .route("/v1/metrics", post(crate::handlers::metrics::get_metrics))
        // Native monitoring read API (query/alerts contract v1, #602).
        // Viewer-role gated: the middleware checks the role before any
        // handler reads a target or presents metric existence.
        .route(
            "/v1/monitoring/catalog",
            get(crate::handlers::monitoring::catalog),
        )
        .route(
            "/v1/monitoring/overview",
            post(crate::handlers::monitoring::overview),
        )
        .route(
            "/v1/monitoring/current",
            post(crate::handlers::monitoring::current),
        )
        .route(
            "/v1/monitoring/checks",
            post(crate::handlers::monitoring::checks),
        )
        .route(
            "/v1/monitoring/history",
            post(crate::handlers::monitoring::history),
        )
        .route(
            "/v1/monitoring/health",
            get(crate::handlers::monitoring::health),
        )
        // Native alerting reads (query/alerts contract v1, #602 PR-6):
        // incidents, rules and the delivery audit. Viewer-gated —
        // pending incidents are visible before they notify.
        .route(
            "/v1/monitoring/alerts",
            post(crate::handlers::alerts::list_incidents),
        )
        .route(
            "/v1/monitoring/alerts/detail",
            post(crate::handlers::alerts::incident_detail),
        )
        .route(
            "/v1/monitoring/alert-rules",
            post(crate::handlers::alerts::list_rules),
        )
        .route(
            "/v1/monitoring/notifications/deliveries",
            post(crate::handlers::alerts::list_deliveries),
        )
        .route("/v1/nodes", post(crate::handlers::nodes::list_nodes))
        .route("/v1/nodes/get", post(crate::handlers::nodes::get_node))
        .route("/v1/vms", post(crate::handlers::vms::list_vms))
        .route("/v1/vms/get", post(crate::handlers::vms::get_vm))
        .route(
            "/v1/vms/console",
            post(crate::handlers::vms::get_vm_console),
        )
        .route(
            "/v1/vms/:vm_id/console-url",
            get(crate::handlers::vms::get_vm_console_url),
        )
        .route(
            "/v1/exports/:export_id/download",
            get(crate::handlers::exports::download_export),
        )
        .route(
            "/v1/vms/events",
            post(crate::handlers::events::list_events_for_vm),
        )
        .route("/v1/tasks", post(crate::handlers::tasks::list_tasks))
        .route("/v1/tasks/get", post(crate::handlers::tasks::get_task))
        .route(
            "/v1/tasks/stream",
            axum::routing::get(crate::handlers::tasks::stream_tasks),
        )
        // Migration read surface (chvctl migrate status/list) — plain
        // SELECTs over the migrations table (#372 DP4b); the start and
        // cancel entry points stay on their existing routes (the
        // vm-mutate migrate action and the CP admin router).
        .route(
            "/v1/migrations",
            get(crate::handlers::migrations::list_migrations),
        )
        .route(
            "/v1/migrations/:migration_id",
            get(crate::handlers::migrations::get_migration),
        )
        .route(
            "/v1/clusters",
            post(crate::handlers::clusters::list_clusters),
        )
        .route(
            "/v1/networks",
            post(crate::handlers::networks::list_networks),
        )
        .route(
            "/v1/networks/get",
            post(crate::handlers::networks::get_network),
        )
        .route("/v1/events", post(crate::handlers::events::list_events))
        .route("/v1/images", post(crate::handlers::images::list_images))
        .route(
            "/v1/maintenance",
            post(crate::handlers::maintenance::get_maintenance),
        )
        .route(
            "/v1/settings",
            post(crate::handlers::settings::get_settings),
        )
        .route(
            "/v1/settings/hypervisor",
            get(crate::handlers::hypervisor_settings::get_settings)
                .post(crate::handlers::hypervisor_settings::get_settings),
        )
        .route(
            "/v1/settings/hypervisor/profiles",
            get(crate::handlers::hypervisor_settings::list_profiles)
                .post(crate::handlers::hypervisor_settings::list_profiles),
        )
        .route("/v1/volumes", post(crate::handlers::volumes::list_volumes))
        .route(
            "/v1/volumes/get",
            post(crate::handlers::volumes::get_volume),
        )
        .route(
            "/v1/vm-templates",
            get(crate::handlers::templates::list_vm_templates),
        )
        .route(
            "/v1/vm-templates/:id",
            get(crate::handlers::templates::preview_vm_template),
        )
        .route(
            "/v1/cloud-init-templates",
            get(crate::handlers::templates::list_cloud_init_templates),
        )
        .route(
            "/v1/vms/snapshots",
            post(crate::handlers::snapshots::list_vm_snapshots),
        )
        .route("/v1/quotas", post(crate::handlers::quotas::list_quotas))
        .route("/v1/quotas/me", post(crate::handlers::quotas::get_my_quota))
        .route(
            "/v1/quotas/:user_id",
            get(crate::handlers::quotas::get_quota),
        )
        .route(
            "/v1/quotas/:user_id/usage",
            post(crate::handlers::quotas::get_user_usage),
        )
        .route("/v1/usage", post(crate::handlers::quotas::get_my_usage))
        .route(
            "/v1/quotas/check",
            post(crate::handlers::quotas::check_quota),
        )
        .route("/v1/tokens", post(crate::handlers::tokens::list_tokens))
        // Backup/restore is HARD-FENCED (#654). CHV ships no tenant-data
        // backup execution engine, so every route answers 501 with the stable
        // `NOT_IMPLEMENTED` code instead of promising an engine that cannot
        // run. Auth/CSRF middleware still gate the route before the handler.
        .route(
            "/v1/backup-jobs",
            post(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backup-history",
            post(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/jobs",
            get(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/jobs/:job_id",
            get(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/schedules",
            get(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/schedules/:schedule_id",
            get(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/restores",
            get(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/restores/:restore_id",
            get(crate::handlers::backups::backup_fence),
        )
        // Architecture Designer (Phase 0) — viewer-accessible reads + stubs.
        .route(
            "/v1/architectures/list",
            post(crate::handlers::architectures::list_architectures),
        )
        .route(
            "/v1/architectures/get",
            post(crate::handlers::architectures::get_architecture),
        )
        .route(
            "/v1/architectures/validate",
            post(crate::handlers::architectures::validate_architecture),
        )
        .route(
            "/v1/architectures/versions/list",
            post(crate::handlers::architectures::list_architecture_versions),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::viewer_middleware,
        ));

    // Operator — VM lifecycle, network/storage mutations
    let operator = Router::new()
        .route(
            "/v1/nodes/mutate",
            post(crate::handlers::nodes::mutate_node),
        )
        // Native alerting mutations (query/alerts contract v1, #602
        // PR-6): rule CRUD under revision preconditions, incident
        // acknowledge and time-bound silence. Operator-gated.
        .route(
            "/v1/monitoring/alert-rules/create",
            post(crate::handlers::alerts::create_rule),
        )
        .route(
            "/v1/monitoring/alert-rules/update",
            post(crate::handlers::alerts::update_rule),
        )
        .route(
            "/v1/monitoring/alert-rules/delete",
            post(crate::handlers::alerts::delete_rule),
        )
        .route(
            "/v1/monitoring/alerts/acknowledge",
            post(crate::handlers::alerts::acknowledge),
        )
        .route(
            "/v1/monitoring/alerts/silence",
            post(crate::handlers::alerts::silence),
        )
        .route("/v1/vms/create", post(crate::handlers::vms::create_vm))
        .route("/v1/vms/delete", post(crate::handlers::vms::delete_vm))
        .route("/v1/vms/resize", post(crate::handlers::vms::resize_vm))
        .route("/v1/vms/mutate", post(crate::handlers::vms::mutate_vm))
        .route(
            "/v1/vms/:vm_id/export",
            post(crate::handlers::exports::export_vm),
        )
        .route("/v1/vms/import", post(crate::handlers::imports::import_vm))
        .route(
            "/v1/networks/create",
            post(crate::handlers::networks::create_network),
        )
        .route(
            "/v1/networks/delete",
            post(crate::handlers::networks::delete_network),
        )
        .route(
            "/v1/networks/update",
            post(crate::handlers::networks::update_network),
        )
        .route(
            "/v1/networks/mutate",
            post(crate::handlers::networks::mutate_network),
        )
        .route(
            "/v1/images/import",
            post(crate::handlers::images::import_image),
        )
        .route(
            "/v1/images/delete",
            post(crate::handlers::images::delete_image),
        )
        // #513 PR 2: the standalone volume-create route — the first
        // production producer of the PR 1 CreateVolume carrier.
        .route(
            "/v1/volumes/create",
            post(crate::handlers::volumes::create_volume),
        )
        .route(
            "/v1/volumes/mutate",
            post(crate::handlers::volumes::mutate_volume),
        )
        .route(
            "/v1/volumes/snapshot",
            post(crate::handlers::volumes::snapshot_volume),
        )
        .route(
            "/v1/volumes/restore-snapshot",
            post(crate::handlers::volumes::restore_volume_snapshot),
        )
        .route(
            "/v1/volumes/delete-snapshot",
            post(crate::handlers::volumes::delete_volume_snapshot),
        )
        // #522 PR 2: the volume-delete route — the first producer of
        // the PR 1 DeleteVolume dispatch carrier (#534).
        .route(
            "/v1/volumes/delete",
            post(crate::handlers::volumes::delete_volume),
        )
        .route(
            "/v1/volumes/clone",
            post(crate::handlers::volumes::clone_volume),
        )
        .route(
            "/v1/vm-templates",
            post(crate::handlers::templates::create_vm_template),
        )
        .route(
            "/v1/vm-templates/:id",
            delete(crate::handlers::templates::delete_vm_template),
        )
        .route(
            "/v1/vm-templates/:id/clone",
            post(crate::handlers::templates::clone_vm_template),
        )
        .route(
            "/v1/cloud-init-templates",
            post(crate::handlers::templates::create_cloud_init_template),
        )
        .route(
            "/v1/cloud-init-templates/:id",
            delete(crate::handlers::templates::delete_cloud_init_template),
        )
        .route(
            "/v1/cloud-init-templates/:id/render",
            post(crate::handlers::templates::render_cloud_init_template),
        )
        .route(
            "/v1/vms/snapshots/create",
            post(crate::handlers::snapshots::create_snapshot),
        )
        .route(
            "/v1/vms/snapshots/delete",
            post(crate::handlers::snapshots::delete_snapshot),
        )
        .route(
            "/v1/vms/snapshots/restore",
            post(crate::handlers::snapshots::restore_snapshot),
        )
        .route(
            "/v1/tokens/create",
            post(crate::handlers::tokens::create_token),
        )
        .route(
            "/v1/tokens/revoke",
            post(crate::handlers::tokens::revoke_token),
        )
        // Backup/restore is HARD-FENCED (#654) — see the viewer block above.
        .route(
            "/v1/backups/jobs",
            post(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/jobs/:job_id",
            patch(crate::handlers::backups::backup_fence)
                .delete(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/jobs/:job_id/execute",
            post(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/schedules",
            post(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/schedules/:schedule_id",
            patch(crate::handlers::backups::backup_fence)
                .delete(crate::handlers::backups::backup_fence),
        )
        .route(
            "/v1/backups/restores",
            post(crate::handlers::backups::backup_fence),
        )
        // Architecture Designer (Phase 0) — operator CRUD writes + planning
        // stubs. Apply/destroy live in the admin layer below.
        .route(
            "/v1/architectures/create",
            post(crate::handlers::architectures::create_architecture),
        )
        .route(
            "/v1/architectures/update",
            post(crate::handlers::architectures::update_architecture),
        )
        .route(
            "/v1/architectures/archive",
            post(crate::handlers::architectures::archive_architecture),
        )
        .route(
            "/v1/architectures/check-fleet",
            post(crate::handlers::architectures::check_fleet_architecture),
        )
        .route(
            "/v1/architectures/generate-yaml",
            post(crate::handlers::architectures::generate_architecture_yaml),
        )
        .route(
            "/v1/architectures/validate-yaml",
            post(crate::handlers::architectures::validate_architecture_yaml),
        )
        .route(
            "/v1/architectures/import-yaml",
            post(crate::handlers::architectures::import_yaml_architecture),
        )
        .route(
            "/v1/architectures/plan",
            post(crate::handlers::architectures::plan_architecture),
        )
        .route(
            "/v1/architectures/destroy-plan",
            post(crate::handlers::architectures::destroy_plan_architecture),
        )
        .route(
            "/v1/architectures/discard-plan",
            post(crate::handlers::architectures::discard_plan_architecture),
        )
        // Architecture Designer (Phase 5) — apply / destroy. The
        // production-environment guard inside the handler (see
        // `enforce_production_guard`) escalates to Admin for `production`
        // / `prod`; non-prod is applyable by any operator. We deliberately
        // mount under operator middleware so the role check is fail-closed
        // at the routing layer too: admin-only environments still produce
        // 403 PRODUCTION_REQUIRES_ADMIN, all other operator apply attempts
        // proceed normally.
        .route(
            "/v1/architectures/apply",
            post(crate::handlers::architectures::apply_architecture),
        )
        .route(
            "/v1/architectures/destroy",
            post(crate::handlers::architectures::destroy_architecture),
        )
        // Architecture Designer (Phase 5/6) — apply-run history. Lifted from
        // the viewer block because run history can leak operational details
        // (task ids, error messages, requested_by actor) that should not be
        // visible to the read-only viewer role per Phase-5 reviewer F1.
        .route(
            "/v1/architectures/runs/list",
            post(crate::handlers::architectures::list_architecture_runs),
        )
        // Architecture Designer (Phase 6) — drift detection. Lifted from
        // the viewer block for the same reason as `/runs/list` (Phase-5 F1):
        // the drift response surfaces `<<live>>/...` resource enumeration
        // plus raw bridge / CIDR / datastore values, all of which constitute
        // fleet-detail leakage to the read-only viewer role. Operator gating
        // mirrors `/check-fleet`, which exposes the same shape of inventory
        // detail.
        .route(
            "/v1/architectures/drift",
            post(crate::handlers::architectures::get_architecture_drift),
        )
        // NetBox projection (#239, PR 5) — the eight POST-only endpoints
        // of docs/specs/architecture-designer/contracts/
        // netbox-api-contract.md. All operator-tier (the contract:
        // viewer has no access to any netbox endpoint). The two
        // data-level escalations — production export (and production
        // config writes / retries) to Admin, delete-retention to Admin —
        // are enforced inside the handlers against the persisted
        // topology row, mirroring how /apply mounts under operator
        // middleware while enforce_production_guard escalates.
        .route(
            "/v1/architectures/netbox/config/get",
            post(crate::handlers::netbox::netbox_config_get),
        )
        .route(
            "/v1/architectures/netbox/config/upsert",
            post(crate::handlers::netbox::netbox_config_upsert),
        )
        .route(
            "/v1/architectures/netbox/config/delete",
            post(crate::handlers::netbox::netbox_config_delete),
        )
        .route(
            "/v1/architectures/netbox/export/dry-run",
            post(crate::handlers::netbox::netbox_dry_run),
        )
        .route(
            "/v1/architectures/netbox/export",
            post(crate::handlers::netbox::netbox_export),
        )
        .route(
            "/v1/architectures/netbox/runs/list",
            post(crate::handlers::netbox::netbox_runs_list),
        )
        .route(
            "/v1/architectures/netbox/runs/get",
            post(crate::handlers::netbox::netbox_runs_get),
        )
        .route(
            "/v1/architectures/netbox/runs/retry",
            post(crate::handlers::netbox::netbox_runs_retry),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::operator_middleware,
        ));

    // Admin — user management, settings, node enrollment
    let admin = Router::new()
        // Authorized notification delivery test (query/alerts contract
        // v1, #602 PR-6). Admin-only: it exercises the operator's
        // configured destination.
        .route(
            "/v1/monitoring/notifications/test",
            post(crate::handlers::alerts::test_notification),
        )
        .route("/v1/users", post(crate::handlers::users::list_users))
        .route(
            "/v1/users/create",
            post(crate::handlers::users::create_user),
        )
        .route(
            "/v1/users/update",
            post(crate::handlers::users::update_user),
        )
        .route(
            "/v1/users/delete",
            post(crate::handlers::users::delete_user),
        )
        .route(
            "/v1/nodes/enroll",
            post(crate::handlers::nodes::enroll_node),
        )
        .route(
            "/v1/nodes/revoke-certificate",
            post(crate::handlers::nodes::revoke_node_certificate),
        )
        .route(
            "/v1/settings/hypervisor",
            patch(crate::handlers::hypervisor_settings::update_settings),
        )
        .route(
            "/v1/settings/hypervisor/update",
            post(crate::handlers::hypervisor_settings::update_settings),
        )
        .route(
            "/v1/settings/hypervisor/apply-profile",
            post(crate::handlers::hypervisor_settings::apply_profile),
        )
        .route(
            "/v1/settings/hypervisor/apply-profile/:id",
            post(crate::handlers::hypervisor_settings::apply_profile_by_path),
        )
        .route(
            "/v1/quotas/create",
            post(crate::handlers::quotas::create_quota),
        )
        .route(
            "/v1/quotas/:user_id",
            patch(crate::handlers::quotas::update_quota)
                .delete(crate::handlers::quotas::delete_quota),
        )
        // Durable privileged-mutation audit trail (#658). Admin only,
        // read-shaped POSTs (the audit middleware does not append rows
        // for these paths).
        .route(
            "/v1/audit-log",
            post(crate::handlers::audit::query_audit_log),
        )
        .route(
            "/v1/audit-log/export",
            post(crate::handlers::audit::export_audit_log),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::admin_middleware,
        ));

    login
        .merge(change_password)
        .merge(viewer)
        .merge(operator)
        .merge(admin)
        .layer(axum::middleware::from_fn(
            crate::csrf_middleware::csrf_protection,
        ))
}

/// The standalone BFF router: [`bff_router_unaudited`] plus the
/// correlation → metrics → audit wrappers (outermost first).
///
/// Metrics deliberately sits **outside** the audit layer so the status
/// it counts is the final one on the wire — including the 500
/// `AUDIT_LOG_WRITE_FAILED` replacement (#658 review M3). Correlation
/// runs outside audit so the id is already in request extensions when
/// the audit row is built.
pub fn bff_router(state: AppState) -> Router<AppState> {
    bff_router_unaudited(state.clone())
        // Wraps the role middlewares (and CSRF) so denied and failed
        // privileged mutations are recorded too.
        .layer(middleware::from_fn_with_state(
            state,
            crate::audit::record_privileged_mutation,
        ))
        .layer(axum::middleware::from_fn(
            crate::metrics_middleware::track_metrics,
        ))
        .layer(axum::middleware::from_fn(
            crate::correlation_middleware::extract_correlation_id,
        ))
}

/// Build a CORS layer from the `CHV_ALLOWED_ORIGIN` environment variable.
///
/// - If the variable is set, only that origin is allowed (no wildcard).
/// - If the variable is absent, CORS is disabled (no cross-origin requests
///   are permitted), which is the safe default for production deployments
///   where the BFF and frontend are served from the same origin.
///
/// Example: `CHV_ALLOWED_ORIGIN=https://console.example.com`
pub fn build_cors_layer() -> CorsLayer {
    match std::env::var("CHV_ALLOWED_ORIGIN") {
        Ok(origin) if !origin.is_empty() => {
            let header_value = match origin.parse::<HeaderValue>() {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(
                        origin = %origin,
                        error = %e,
                        "CHV_ALLOWED_ORIGIN is not a valid HTTP header value; denying all CORS"
                    );
                    return CorsLayer::new();
                }
            };
            CorsLayer::new()
                .allow_origin(header_value)
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    header::AUTHORIZATION,
                    header::CONTENT_TYPE,
                    header::ACCEPT,
                    header::HeaderName::from_static("x-correlation-id"),
                    header::HeaderName::from_static("x-csrf-token"),
                ])
                .allow_credentials(true)
        }
        _ => CorsLayer::new(),
    }
}
