mod alert_rules;
mod alerts;
mod architectures;
mod backups;
mod bootstrap_tokens;
mod certificate_revocations;
mod credential_crypto;

/// Test-support access to the credential cipher so repository tests can
/// inject a deterministic key (or no key) without mutating process-global
/// environment variables.
#[cfg(any(test, feature = "test-util"))]
pub use credential_crypto::{CredentialEncryption, EncryptError};
mod db;
mod desired_state;
mod events;
mod hypervisor_settings;
mod images;
mod migrations;
mod monitoring_agents;
mod network_exposures;
mod networks;
mod nodes;
mod notification_outbox;
mod observed_state;
mod operations;
mod vtep;

pub use alert_rules::{
    AlertRule, AlertRuleRepository, AlertRuleRow, AlertRuleSpec, CheckStatusMatch, DimensionMatch,
    GroupOp, MissingDataPolicy, RuleCreateInput, RuleUpdateInput, ThresholdOperator,
    MAX_FOR_SECONDS, MAX_GROUP_CONDITIONS, MAX_ID_BYTES, MAX_NAME_BYTES, MAX_RATE_WINDOW_SECONDS,
    MIN_RATE_WINDOW_SECONDS,
};
pub use alerts::{
    dedup_key, AlertCreateInput, AlertRepository, IncidentListFilter, IncidentOpenInput,
    IncidentRow, IncidentTransitionRow, ALERT_SOURCE_MONITORING, ALERT_TYPE_MONITORING_RULE,
    INCIDENT_STATUS_FIRING, INCIDENT_STATUS_PENDING, INCIDENT_STATUS_RESOLVED,
};
pub use architectures::{
    is_active_run_conflict, ApplyRunCreateInput, ApplyRunRepository, ApplyRunUpdateInput,
    DriftReportCreateInput, DriftReportRepository, InventorySnapshotCreateInput,
    InventorySnapshotRepository, NetboxProjectionConfigRepository,
    NetboxProjectionConfigUpsertInput, NetboxProjectionRunCreateInput,
    NetboxProjectionRunRepository, PlanCreateInput, PlanRepository, PlanStatusUpdateInput,
    TopologyCreateInput, TopologyListFilter, TopologyRepository, TopologyUpdateInput,
    VersionCreateInput, VersionRepository, ACTIVE_RUN_CONFLICT_MARKER, MAX_ATTEMPTS,
};
pub use backups::{
    BackupJobCreateInput, BackupJobRow, BackupJobStatusUpdateInput, BackupJobUpdateInput,
    BackupRepository, BackupRestoreCreateInput, BackupRestoreRow, BackupScheduleCreateInput,
    BackupScheduleRow, BackupScheduleUpdateInput,
};
pub use bootstrap_tokens::{BootstrapTokenRepository, BootstrapTokenValidation};
pub use certificate_revocations::{
    normalize_chv_certificate_serial, CertificateRevocationRepository, NodeCertificateRevocation,
    RevokeCertificateInput, RevokeCertificateOutcome, CHV_CERTIFICATE_SERIAL_HEX_LEN,
};
pub use db::{
    connect_pool, migrations_path, migrator, run_migrations, ControlPlaneStoreConfig, StoreError,
    StorePool,
};
pub use desired_state::{
    CloneTargetMaterialization, CloneTargetSpec, DesiredStateRepository, NetworkDesiredStateInput,
    NetworkStatusPatchInput, VmDesiredStateInput, VmPowerStatePatchInput, VmResourcesPatchInput,
    VolumeAttachmentPatchInput, VolumeDesiredStateInput, VolumeResizePatchInput,
    VolumeSnapshotPatchInput, VolumeSummaryRow,
};
pub use events::{EventAppendInput, EventRepository};
pub use hypervisor_settings::{HypervisorSettingsRepository, HypervisorSettingsRow};
pub use images::{ImageRepository, ImageRow};
pub use migrations::{
    is_terminal_phase, InterruptedMigrationOutcome, InterruptedMigrationUpdate, MigrationRecord,
    MigrationRepository,
};
pub use monitoring_agents::{
    AgentAuthError, AgentCredential, AgentOsMetadata, ClaimConsumeError, ConsumedClaim,
    IssuedClaim, MonitoringAgentRepository, MonitoringAgentRow, RecordBatchOutcome,
};
pub use network_exposures::NetworkExposureInput;
pub use networks::{NetworkRepository, NetworkRow};
pub use nodes::{
    NodeBootstrapResultInput, NodeDrainIntentInput, NodeInventoryInput, NodeRepository,
    NodeSchedulingPatchInput, NodeStatePatchInput, NodeUpsertInput, NodeVersionInput,
    AUTHORITY_MODE_CORE_MANAGED, AUTHORITY_MODE_CORE_NATIVE, AUTHORITY_MODE_LEGACY,
};
pub use notification_outbox::{
    enqueue_on_tx, NotificationEventInput, NotificationOutboxRepository, OutboxEventRow,
    CHANNEL_SLACK, CHANNEL_WEBHOOK, EVENT_TYPE_ACKNOWLEDGED, EVENT_TYPE_DELIVERY_FAILED,
    EVENT_TYPE_FIRING, EVENT_TYPE_RESOLVED, EVENT_TYPE_TEST,
};
pub use observed_state::{
    NetworkObservedStateInput, NodeObservedStateInput, ObservedStateRepository, VmMetricsInput,
    VmObservedStateInput, VolumeObservedStateInput,
};
pub use operations::{OperationCreateInput, OperationRepository, OperationStatusUpdateInput};
pub use vtep::{FabricPeerRecord, VtepEntry, VtepRepository};

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

#[cfg(test)]
mod tests;
