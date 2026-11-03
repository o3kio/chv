use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

macro_rules! string_id_newtype {
    ($name:ident, $field_name:literal) => {
        string_id_newtype!($name, $field_name, max_bytes = usize::MAX);
    };
    ($name:ident, $field_name:literal, max_bytes = $max_bytes:expr) => {
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(IdentifierError::empty($field_name));
                }
                if trimmed.as_bytes().len() > $max_bytes {
                    return Err(IdentifierError::too_long($field_name, $max_bytes));
                }
                Ok(Self(trimmed.to_string()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdentifierError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentifierError {
    field: &'static str,
    max_bytes: Option<usize>,
}

impl IdentifierError {
    pub fn empty(field: &'static str) -> Self {
        Self {
            field,
            max_bytes: None,
        }
    }

    pub fn too_long(field: &'static str, max_bytes: usize) -> Self {
        Self {
            field,
            max_bytes: Some(max_bytes),
        }
    }
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.max_bytes {
            Some(max) => write!(f, "{} must not exceed {} bytes", self.field, max),
            None => write!(f, "{} cannot be empty", self.field),
        }
    }
}

impl Error for IdentifierError {}

string_id_newtype!(ActorId, "actor_id");
string_id_newtype!(NodeId, "node_id");
string_id_newtype!(OperationId, "operation_id");
// Resource ids feed into fixed-size structures such as the BPF vm_key in
// chv-nwd-core, so ids are capped at the key width (32 bytes). That is
// exactly a 128-bit hex id (#664): a longer id would have to be truncated
// there and could collide with another resource's id.
string_id_newtype!(ResourceId, "resource_id", max_bytes = 32);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Generation(u64);

impl Generation {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseGenerationError;

impl fmt::Display for ParseGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("generation must be a decimal string")
    }
}

impl Error for ParseGenerationError {}

impl From<u64> for Generation {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl FromStr for Generation {
    type Err = ParseGenerationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
            return Err(ParseGenerationError);
        }
        if value.len() > 1 && value.starts_with('0') {
            return Err(ParseGenerationError);
        }

        value
            .parse::<u64>()
            .map(Self)
            .map_err(|_| ParseGenerationError)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ResourceKind {
    Node,
    Vm,
    Volume,
    Network,
}

impl ResourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Vm => "vm",
            Self::Volume => "volume",
            Self::Network => "network",
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseResourceKindError;

impl fmt::Display for ParseResourceKindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown resource kind")
    }
}

impl Error for ParseResourceKindError {}

impl FromStr for ResourceKind {
    type Err = ParseResourceKindError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "node" | "Node" => Ok(Self::Node),
            "vm" | "Vm" => Ok(Self::Vm),
            "volume" | "Volume" => Ok(Self::Volume),
            "network" | "Network" => Ok(Self::Network),
            _ => Err(ParseResourceKindError),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ResourceRef {
    pub kind: ResourceKind,
    pub id: ResourceId,
}

impl ResourceRef {
    pub fn new(kind: ResourceKind, id: ResourceId) -> Self {
        Self { kind, id }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum NodeState {
    Discovered,
    Bootstrapping,
    HostReady,
    StorageReady,
    NetworkReady,
    TenantReady,
    Degraded,
    Draining,
    Maintenance,
    Failed,
    Unreachable,
}

impl NodeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "Discovered",
            Self::Bootstrapping => "Bootstrapping",
            Self::HostReady => "HostReady",
            Self::StorageReady => "StorageReady",
            Self::NetworkReady => "NetworkReady",
            Self::TenantReady => "TenantReady",
            Self::Degraded => "Degraded",
            Self::Draining => "Draining",
            Self::Maintenance => "Maintenance",
            Self::Failed => "Failed",
            Self::Unreachable => "Unreachable",
        }
    }

    pub const fn is_schedulable(self) -> bool {
        matches!(self, Self::TenantReady)
    }
}

impl fmt::Display for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseNodeStateError;

impl fmt::Display for ParseNodeStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown node state")
    }
}

impl Error for ParseNodeStateError {}

impl FromStr for NodeState {
    type Err = ParseNodeStateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "Discovered" => Ok(Self::Discovered),
            "Bootstrapping" => Ok(Self::Bootstrapping),
            "HostReady" => Ok(Self::HostReady),
            "StorageReady" => Ok(Self::StorageReady),
            "NetworkReady" => Ok(Self::NetworkReady),
            "TenantReady" => Ok(Self::TenantReady),
            "Degraded" => Ok(Self::Degraded),
            "Draining" => Ok(Self::Draining),
            "Maintenance" => Ok(Self::Maintenance),
            "Failed" => Ok(Self::Failed),
            "Unreachable" => Ok(Self::Unreachable),
            _ => Err(ParseNodeStateError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum OperationStatus {
    Pending,
    Accepted,
    Running,
    RetryPending,
    Succeeded,
    Failed,
    Rejected,
    Cancelled,
    Stale,
    Conflict,
    AwaitingOperatorInput,
}

impl OperationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Accepted => "Accepted",
            Self::Running => "Running",
            Self::RetryPending => "RetryPending",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Rejected => "Rejected",
            Self::Cancelled => "Cancelled",
            Self::Stale => "Stale",
            Self::Conflict => "Conflict",
            Self::AwaitingOperatorInput => "AwaitingOperatorInput",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed
                | Self::Rejected
                | Self::Cancelled
                | Self::Stale
                | Self::Conflict
        )
    }

    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Pending
                | Self::Accepted
                | Self::Running
                | Self::RetryPending
                | Self::AwaitingOperatorInput
        )
    }
}

impl fmt::Display for OperationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseOperationStatusError;

impl fmt::Display for ParseOperationStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown operation status")
    }
}

impl Error for ParseOperationStatusError {}

impl FromStr for OperationStatus {
    type Err = ParseOperationStatusError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "Pending" => Ok(Self::Pending),
            "Accepted" => Ok(Self::Accepted),
            "Running" => Ok(Self::Running),
            "RetryPending" => Ok(Self::RetryPending),
            "Succeeded" => Ok(Self::Succeeded),
            "Failed" => Ok(Self::Failed),
            "Rejected" => Ok(Self::Rejected),
            "Cancelled" => Ok(Self::Cancelled),
            "Stale" => Ok(Self::Stale),
            "Conflict" => Ok(Self::Conflict),
            "AwaitingOperatorInput" => Ok(Self::AwaitingOperatorInput),
            _ => Err(ParseOperationStatusError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum EventSeverity {
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl EventSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "Debug",
            Self::Info => "Info",
            Self::Warning => "Warning",
            Self::Error => "Error",
            Self::Critical => "Critical",
        }
    }
}

impl fmt::Display for EventSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseEventSeverityError;

impl fmt::Display for ParseEventSeverityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown event severity")
    }
}

impl Error for ParseEventSeverityError {}

impl FromStr for EventSeverity {
    type Err = ParseEventSeverityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "Debug" | "debug" => Ok(Self::Debug),
            "Info" | "info" => Ok(Self::Info),
            "Warning" | "warning" => Ok(Self::Warning),
            "Error" | "error" => Ok(Self::Error),
            "Critical" | "critical" => Ok(Self::Critical),
            _ => Err(ParseEventSeverityError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum EventType {
    Audit,
    Enrollment,
    DesiredStateApplied,
    DesiredStateAcknowledged,
    DesiredStateRejected,
    ObservedStateReported,
    StateTransition,
    OperationStarted,
    OperationCompleted,
    OperationFailed,
    Maintenance,
    Health,
}

impl EventType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Audit => "Audit",
            Self::Enrollment => "Enrollment",
            Self::DesiredStateApplied => "DesiredStateApplied",
            Self::DesiredStateAcknowledged => "DesiredStateAcknowledged",
            Self::DesiredStateRejected => "DesiredStateRejected",
            Self::ObservedStateReported => "ObservedStateReported",
            Self::StateTransition => "StateTransition",
            Self::OperationStarted => "OperationStarted",
            Self::OperationCompleted => "OperationCompleted",
            Self::OperationFailed => "OperationFailed",
            Self::Maintenance => "Maintenance",
            Self::Health => "Health",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseEventTypeError;

impl fmt::Display for ParseEventTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown event type")
    }
}

impl Error for ParseEventTypeError {}

impl FromStr for EventType {
    type Err = ParseEventTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "Audit" => Ok(Self::Audit),
            "Enrollment" => Ok(Self::Enrollment),
            "DesiredStateApplied" => Ok(Self::DesiredStateApplied),
            "DesiredStateAcknowledged" => Ok(Self::DesiredStateAcknowledged),
            "DesiredStateRejected" => Ok(Self::DesiredStateRejected),
            "ObservedStateReported" => Ok(Self::ObservedStateReported),
            "StateTransition" | "NodeStateTransition" => Ok(Self::StateTransition),
            "OperationStarted" => Ok(Self::OperationStarted),
            "OperationCompleted" => Ok(Self::OperationCompleted),
            "OperationFailed" => Ok(Self::OperationFailed),
            "Maintenance" => Ok(Self::Maintenance),
            "Health" => Ok(Self::Health),
            _ => Err(ParseEventTypeError),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredState<T> {
    pub version: Generation,
    pub value: T,
    pub recorded_at: SystemTime,
    pub recorded_by: Option<ActorId>,
}

impl<T> DesiredState<T> {
    pub fn new(
        version: Generation,
        value: T,
        recorded_at: SystemTime,
        recorded_by: Option<ActorId>,
    ) -> Self {
        Self {
            version,
            value,
            recorded_at,
            recorded_by,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedState<T> {
    pub version: Generation,
    pub value: T,
    pub observed_at: SystemTime,
    pub observed_by: Option<ActorId>,
}

impl<T> ObservedState<T> {
    pub fn new(
        version: Generation,
        value: T,
        observed_at: SystemTime,
        observed_by: Option<ActorId>,
    ) -> Self {
        Self {
            version,
            value,
            observed_at,
            observed_by,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventRecord {
    pub operation_id: Option<OperationId>,
    pub resource: Option<ResourceRef>,
    pub severity: EventSeverity,
    pub event_type: EventType,
    pub message: String,
    pub occurred_at: SystemTime,
    pub actor: Option<ActorId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationRecord {
    pub operation_id: OperationId,
    pub resource: Option<ResourceRef>,
    pub status: OperationStatus,
    pub requested_by: Option<ActorId>,
    pub summary: Option<String>,
    pub requested_at: SystemTime,
    pub updated_at: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_rejects_non_numeric_input() {
        assert!(Generation::from_str("").is_err());
        assert!(Generation::from_str("abc").is_err());
        assert!(Generation::from_str("12x").is_err());
    }

    #[test]
    fn resource_kind_round_trips() {
        let kind = ResourceKind::from_str("vm").unwrap();
        assert_eq!(kind, ResourceKind::Vm);
        assert_eq!(kind.to_string(), "vm");
    }

    #[test]
    fn resource_id_caps_length_at_the_bpf_key_width() {
        // Exactly 32 bytes (a 128-bit hex id) fits the BPF vm_key.
        assert!(ResourceId::new("vm-12345678901234567890123456789").is_ok());
        // 33 bytes must be rejected, not silently truncated.
        assert!(ResourceId::new("vm-123456789012345678901234567890").is_err());
        // Trimming still applies before the length check.
        assert!(ResourceId::new("  vm-12345678901234567890123456789  ").is_ok());
        assert!(ResourceId::new("  vm-123456789012345678901234567890 ").is_err());
    }

    #[test]
    fn node_state_schedulability_tracks_the_tenant_ready_state() {
        assert!(!NodeState::Bootstrapping.is_schedulable());
        assert!(NodeState::TenantReady.is_schedulable());
    }

    #[test]
    fn operation_status_identifies_terminal_states() {
        assert!(!OperationStatus::Accepted.is_terminal());
        assert!(OperationStatus::Succeeded.is_terminal());
        assert!(OperationStatus::Cancelled.is_terminal());
        assert!(OperationStatus::Conflict.is_terminal());
        assert!(!OperationStatus::AwaitingOperatorInput.is_terminal());
        assert!(OperationStatus::AwaitingOperatorInput.is_active());
    }
}
