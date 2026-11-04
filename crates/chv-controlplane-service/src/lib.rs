pub mod api;
mod backup_shipper;
mod backup_worker;
mod bff_mutations;
pub mod circuit_breaker;
pub mod compat;
mod container;
pub mod convergence_metrics;
mod enrollment;
mod error;
mod fabric_planner;
mod inventory;
mod lifecycle;
pub mod migration;
mod migration_reaper;
mod netbox_projection_worker;
// Entirely behind the feature: default-feature builds never see the
// module (ADR-024's compile-time half of the plain-HTTP double gate).
#[cfg(feature = "netbox-demo")]
pub mod netbox_demo;
mod node_client;
mod node_client_pool;
mod orchestrator;
pub mod overlay;
pub mod peer_identity;
mod reconcile;
mod revocation;
mod server;
mod telemetry;

pub mod monitoring_agent;
pub mod monitoring_ingest;
pub mod monitoring_validate;

pub mod alert_evaluator;
pub mod monitoring_exporter;
pub mod notification_dispatcher;

pub use monitoring_ingest::{
    run_monitoring_maintenance, MonitoringIngestImplementation, MonitoringIngestService,
};

pub use backup_shipper::{shipper_from_destination, BackupShipper, NullShipper};
pub use backup_worker::BackupWorker;
pub use bff_mutations::ControlPlaneMutationService;
pub use container::{ControlPlaneComponents, ControlPlaneRuntime, ControlPlaneService};
pub use convergence_metrics::{ConvergenceMetrics, SharedConvergenceMetrics};
pub use enrollment::{
    CaBackedCertificateIssuer, CertificateIssuer, EnrollmentService,
    EnrollmentServiceImplementation, IssuedCertificate,
};
pub use error::ControlPlaneServiceError;
pub use fabric_planner::{CompiledFabricPlan, FabricPlanner};
pub use inventory::{InventoryService, InventoryServiceImplementation};
pub use lifecycle::{LifecycleService, LifecycleServiceImplementation};
pub use migration::{MigrationRecoveryReport, MigrationRegistry, RecoveredMigration};
pub use migration_reaper::MigrationReaper;
pub use netbox_projection_worker::NetboxProjectionWorker;
pub use node_client::NodeClient;
pub use node_client_pool::NodeClientPool;
pub use orchestrator::Orchestrator;
pub use overlay::OverlayManager;
pub use peer_identity::{
    extract_peer_certificate_serial_from_extensions, extract_peer_node_id_from_extensions,
    parse_node_id_from_der, parse_serial_from_der, validate_security_mode, verify_peer_matches,
    InsecurePeer, PeerCertificateSerial, PeerIdentityError, PeerIdentityInterceptor, PeerNodeId,
};
pub use reconcile::{ReconcileService, ReconcileServiceImplementation};
pub use revocation::{
    load_startup_snapshot, refresh_interval_from_env, run_refresh_loop, RevocationSnapshot,
    DEFAULT_REFRESH_INTERVAL, REFRESH_INTERVAL_ENV,
};
pub use server::{
    EnrollmentServer, InventoryServer, LifecycleServer, MonitoringServer, ReconcileServer,
    TelemetryServer,
};
pub use telemetry::{TelemetryService, TelemetryServiceImplementation};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod server_audit_tests;

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod monitoring_agent_tests;

#[cfg(test)]
mod monitoring_ingest_tests;

#[cfg(test)]
mod netbox_projection_worker_tests;

#[cfg(test)]
mod netbox_projection_sim_tests;
