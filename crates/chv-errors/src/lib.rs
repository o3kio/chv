#[derive(Debug, thiserror::Error)]
pub enum ChvError {
    #[error("not found: {resource} {id}")]
    NotFound { resource: String, id: String },

    #[error("already exists: {resource} {id}")]
    AlreadyExists { resource: String, id: String },

    #[error("invalid argument: {field} — {reason}")]
    InvalidArgument { field: String, reason: String },

    #[error("bad request: {reason}")]
    BadRequest { reason: String },

    #[error("unauthorized: {reason}")]
    Unauthorized { reason: String },

    #[error("access denied: {resource} — {reason}")]
    AccessDenied { resource: String, reason: String },

    #[error("quota exceeded: {resource} — limit {limit}, used {used}, requested {requested}")]
    QuotaExceeded {
        resource: String,
        limit: i64,
        used: i64,
        requested: i64,
    },

    #[error("backend unavailable: {backend} — {reason}")]
    BackendUnavailable { backend: String, reason: String },

    /// A downstream service refused the request because a capacity/admission
    /// bound is currently full. Distinct from `QuotaExceeded` (a structured
    /// per-resource quota) so callers can retry a transient admission
    /// rejection without guessing from the message text.
    #[error("resource exhausted: {reason}")]
    ResourceExhausted { reason: String },

    #[error("network unavailable: {resource} — {reason}")]
    NetworkUnavailable { resource: String, reason: String },

    #[error("unimplemented: {reason}")]
    Unimplemented { reason: String },

    #[error("conflict: {resource} {id}")]
    Conflict { resource: String, id: String },

    #[error("stale generation: {resource} {id} — expected >= {expected}, got {got}")]
    StaleGeneration {
        resource: String,
        id: String,
        expected: String,
        got: String,
    },

    #[error("control plane unavailable: {reason}")]
    ControlPlaneUnavailable { reason: String },

    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A bounded operation did not finish in time. Distinct from `Internal` so
    /// callers can tell "we stopped waiting but the remote side may still be
    /// running" from a definitive failure (e.g. a Cloud Hypervisor migration
    /// call whose transfer keeps running after the agent's own wait expires).
    #[error("timed out after {secs}s: {operation}")]
    Timeout { operation: String, secs: u64 },

    /// The request was fully delivered, but no definitive response was read
    /// (the connection failed or closed mid-response). The remote side may
    /// still be acting on it, so callers must not treat this as a definitive
    /// failure — in particular they must not unpause a VM that a possibly-live
    /// migration transfer is still using.
    #[error("outcome unknown for {operation}: {reason}")]
    OutcomeUnknown { operation: String, reason: String },

    #[error("internal error: {reason}")]
    Internal { reason: String },
}

pub struct ErrorCode;

#[allow(non_upper_case_globals)]
impl ErrorCode {
    pub const OK: &str = "OK";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const ALREADY_EXISTS: &str = "ALREADY_EXISTS";
    pub const INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    pub const UNAUTHORIZED: &str = "UNAUTHORIZED";
    pub const ACCESS_DENIED: &str = "ACCESS_DENIED";
    pub const QUOTA_EXCEEDED: &str = "QUOTA_EXCEEDED";
    pub const BACKEND_UNAVAILABLE: &str = "BACKEND_UNAVAILABLE";
    pub const RESOURCE_EXHAUSTED: &str = "RESOURCE_EXHAUSTED";
    pub const NETWORK_UNAVAILABLE: &str = "NETWORK_UNAVAILABLE";
    pub const UNIMPLEMENTED: &str = "UNIMPLEMENTED";
    pub const CONFLICT: &str = "CONFLICT";
    pub const STALE_GENERATION: &str = "STALE_GENERATION";
    pub const CONTROL_PLANE_UNAVAILABLE: &str = "CONTROL_PLANE_UNAVAILABLE";
    pub const IO: &str = "IO_ERROR";
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const OUTCOME_UNKNOWN: &str = "OUTCOME_UNKNOWN";
    pub const INTERNAL: &str = "INTERNAL_ERROR";
}

impl ChvError {
    pub fn error_code(&self) -> &'static str {
        match self {
            ChvError::NotFound { .. } => ErrorCode::NOT_FOUND,
            ChvError::AlreadyExists { .. } => ErrorCode::ALREADY_EXISTS,
            ChvError::InvalidArgument { .. } => ErrorCode::INVALID_ARGUMENT,
            ChvError::BadRequest { .. } => ErrorCode::BAD_REQUEST,
            ChvError::Unauthorized { .. } => ErrorCode::UNAUTHORIZED,
            ChvError::AccessDenied { .. } => ErrorCode::ACCESS_DENIED,
            ChvError::QuotaExceeded { .. } => ErrorCode::QUOTA_EXCEEDED,
            ChvError::BackendUnavailable { .. } => ErrorCode::BACKEND_UNAVAILABLE,
            ChvError::ResourceExhausted { .. } => ErrorCode::RESOURCE_EXHAUSTED,
            ChvError::NetworkUnavailable { .. } => ErrorCode::NETWORK_UNAVAILABLE,
            ChvError::Unimplemented { .. } => ErrorCode::UNIMPLEMENTED,
            ChvError::Conflict { .. } => ErrorCode::CONFLICT,
            ChvError::StaleGeneration { .. } => ErrorCode::STALE_GENERATION,
            ChvError::ControlPlaneUnavailable { .. } => ErrorCode::CONTROL_PLANE_UNAVAILABLE,
            ChvError::Io { .. } => ErrorCode::IO,
            ChvError::Timeout { .. } => ErrorCode::TIMEOUT,
            ChvError::OutcomeUnknown { .. } => ErrorCode::OUTCOME_UNKNOWN,
            ChvError::Internal { .. } => ErrorCode::INTERNAL,
        }
    }

    pub fn status(&self) -> &'static str {
        "error"
    }

    pub fn to_result_fields(&self) -> (&'static str, &'static str, String) {
        (self.status(), self.error_code(), self.to_string())
    }

    pub fn ok_result_fields() -> (&'static str, &'static str, String) {
        (ErrorCode::OK, ErrorCode::OK, String::new())
    }
}

impl From<ChvError> for tonic::Status {
    fn from(err: ChvError) -> tonic::Status {
        match &err {
            ChvError::NotFound { resource, id } => {
                tonic::Status::not_found(format!("{resource} {id}"))
            }
            ChvError::AlreadyExists { resource, id } => {
                tonic::Status::already_exists(format!("{resource} {id}"))
            }
            ChvError::InvalidArgument { field, reason } => {
                tonic::Status::invalid_argument(format!("{field}: {reason}"))
            }
            ChvError::BadRequest { reason } => tonic::Status::invalid_argument(reason.clone()),
            ChvError::AccessDenied { resource, reason } => {
                tonic::Status::permission_denied(format!("{resource}: {reason}"))
            }
            ChvError::Unauthorized { .. } => tonic::Status::unauthenticated("unauthorized"),
            ChvError::QuotaExceeded { resource, .. } => {
                tonic::Status::resource_exhausted(format!("{resource} quota exceeded"))
            }
            ChvError::Conflict { resource, id } => {
                tonic::Status::already_exists(format!("{resource} {id}"))
            }
            ChvError::StaleGeneration {
                resource,
                id,
                expected,
                got,
            } => tonic::Status::failed_precondition(format!(
                "stale generation on {resource} {id}: expected >= {expected}, got {got}"
            )),
            ChvError::BackendUnavailable { backend, .. } => {
                tonic::Status::unavailable(format!("{backend} unavailable"))
            }
            ChvError::ResourceExhausted { reason } => {
                tonic::Status::resource_exhausted(reason.clone())
            }
            ChvError::ControlPlaneUnavailable { .. } => {
                tonic::Status::unavailable("control plane unavailable")
            }
            ChvError::NetworkUnavailable { resource, .. } => {
                tonic::Status::unavailable(format!("{resource} unavailable"))
            }
            ChvError::Unimplemented { reason } => tonic::Status::unimplemented(reason.clone()),
            ChvError::Timeout { operation, secs } => {
                tonic::Status::deadline_exceeded(format!("{operation} timed out after {secs}s"))
            }
            ChvError::OutcomeUnknown { .. } => tonic::Status::internal("operation outcome unknown"),
            ChvError::Io { .. } | ChvError::Internal { .. } => {
                tonic::Status::internal("internal error")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #378 §7 fast-fail piece: the `Unimplemented` variant must keep
    /// the error's identity through every structured rendering — the
    /// structured error code (consumed by e.g. nwd's `ResultMeta`
    /// rendering via `to_result_fields`), the Display text (which rides
    /// the operations row's `error_message`), and the tonic round-trip
    /// (`From<ChvError> for tonic::Status` maps it back to
    /// `Code::Unimplemented`, never the generic internal flattening).
    #[test]
    fn unimplemented_variant_preserves_identity_in_structured_renderings() {
        let err = ChvError::Unimplemented {
            reason: "snapshot_volume failed: status: Unimplemented, message: \
                     \"snapshot_volume is unsupported in core-managed mode\""
                .to_string(),
        };

        assert_eq!(err.error_code(), ErrorCode::UNIMPLEMENTED);
        assert_eq!(ErrorCode::UNIMPLEMENTED, "UNIMPLEMENTED");

        let (status, code, message) = err.to_result_fields();
        assert_eq!(status, "error");
        assert_eq!(code, ErrorCode::UNIMPLEMENTED);
        assert!(
            message.contains("snapshot_volume is unsupported in core-managed mode"),
            "the refusal text must ride the Display rendering: {message}"
        );
        assert!(
            message.starts_with("unimplemented: "),
            "the Display names the class: {message}"
        );

        let tonic_status = tonic::Status::from(err);
        assert_eq!(tonic_status.code(), tonic::Code::Unimplemented);
    }
}
