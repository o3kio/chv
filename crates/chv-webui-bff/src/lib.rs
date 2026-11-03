pub mod audit;
pub mod auth;
pub mod cache;
pub mod correlation_middleware;
pub mod csrf_middleware;
pub mod error;
pub mod handlers;
pub mod metrics_apply;
pub mod metrics_drift;
pub mod metrics_middleware;
pub mod mutations;
pub mod router;

pub use auth::BearerToken;
pub use cache::BffCache;
pub use error::BffError;
pub use mutations::MutationService;
pub use router::{bff_router, bff_router_unaudited, AppState, NotificationChannels};
// Re-exported so AppState consumers (tests, control-plane bootstrap)
// can construct the monitoring health handle without depending on the
// store crate directly.
pub use chv_monitoring_store::MonitoringHealth;
