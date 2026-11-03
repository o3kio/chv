//! Transport-neutral v1 sample and check validation shared by the node
//! batch (gRPC, PR-2) and guest agent (HTTPS, G3) ingestion paths.
//!
//! Both transports MUST enforce the same contract — a metric or check
//! that is invalid on one wire is invalid on the other. The only
//! validation that differs is sender/target ownership, which is
//! transport specific (mTLS node identity vs enrolled agent credential
//! binding) and stays with the callers. Everything else — registry
//! membership, source allowlists, kind/unit agreement, unit-aware
//! value domains, value/quality consistency, integer counters,
//! timestamp bounds, dimension rules, epoch fencing, check-record
//! identifier/summary/status rules — lives here exactly once.

use chv_monitoring_core::model::{
    CheckRecord, CheckStatus, MetricKind, Sample, SampleBuilder, SampleQuality, SampleValue,
    Source, TargetKind, Unit,
};
use chv_monitoring_core::registry;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

/// Contract caps (ingestion v1 initial defaults).
pub const MAX_SAMPLES_PER_BATCH: usize = 512;
pub const MAX_SAMPLE_AGE_MS: i64 = 5 * 60 * 1000;
pub const MAX_FUTURE_SKEW_MS: i64 = 2 * 60 * 1000;
pub const MAX_BOOT_ID_BYTES: usize = 128;
/// Contract cap: "Max discovered checks/batch: 128" (ingestion v1).
pub const MAX_CHECKS_PER_BATCH: usize = 128;
/// Maximum check summary length in bytes (agent spec §"Check
/// records"; the manager rejects — never truncates).
pub const MAX_SUMMARY_BYTES: usize = 256;
/// Maximum check_id / service_key length in bytes (printable
/// identifier charset only).
pub const MAX_CHECK_ID_BYTES: usize = 128;

/// Wire outcome vocabulary (ingestion contract v1).
pub const OUTCOME_ACCEPTED: &str = "accepted";
pub const OUTCOME_DUPLICATE: &str = "duplicate";
pub const OUTCOME_REPLAY_CONFLICT: &str = "replay_conflict";
pub const OUTCOME_INVALID_BATCH: &str = "invalid_batch";
pub const OUTCOME_BATCH_TOO_LARGE: &str = "batch_too_large";
pub const OUTCOME_UNSUPPORTED_METRIC: &str = "unsupported_metric";
pub const OUTCOME_RATE_LIMITED: &str = "rate_limited";
pub const OUTCOME_INGESTION_UNAVAILABLE: &str = "ingestion_unavailable";
pub const OUTCOME_SERIES_CAP_EXCEEDED: &str = "series_cap_exceeded";
pub const OUTCOME_STALE_SEQUENCE: &str = "stale_sequence";

/// A sample in its raw wire form, before validation. Both transports
/// map their wire types into this shape; the value keeps the wire's
/// numeric distinction so integer counters stay integer.
#[derive(Debug, Clone)]
pub struct RawSample {
    pub target_kind: String,
    pub target_id: String,
    pub metric_id: String,
    pub source: String,
    pub kind: String,
    pub unit: String,
    pub observed_at_ms: i64,
    pub quality: String,
    pub value: Option<RawValue>,
    pub dimensions: BTreeMap<String, String>,
    pub boot_id: String,
    pub identity_epoch: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RawValue {
    Float(f64),
    Integer(u64),
}

/// A whole-batch rejection with its contract outcome.
#[derive(Debug)]
pub struct SampleRejection {
    pub outcome: &'static str,
    pub detail: String,
}

pub fn reject(outcome: &'static str, detail: impl std::fmt::Display) -> SampleRejection {
    SampleRejection {
        outcome,
        detail: detail.to_string(),
    }
}

/// Structural target-kind check shared by both transports: the kind
/// must parse, and VM targets must be valid `ResourceId`s (the 32-byte
/// resource id cap, matching the 128-bit hex id width). Callers add their
/// ownership rules on top.
pub fn validate_target(s: &RawSample) -> Result<TargetKind, SampleRejection> {
    let target_kind = TargetKind::from_str(&s.target_kind)
        .map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("target_kind: {e}")))?;
    if target_kind == TargetKind::Vm {
        chv_controlplane_types::domain::ResourceId::new(&s.target_id)
            .map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("vm target_id: {e}")))?;
    }
    Ok(target_kind)
}

/// Validate one raw sample against the v1 registry and model rules.
/// Whole-batch rejection semantics: any error here rejects the batch.
pub fn validate_sample(s: &RawSample, now_ms: i64) -> Result<Sample, SampleRejection> {
    let target_kind = validate_target(s)?;

    let metric = registry::lookup(&s.metric_id).ok_or_else(|| {
        reject(
            OUTCOME_UNSUPPORTED_METRIC,
            format!("unknown metric {}", s.metric_id),
        )
    })?;
    let source = Source::from_str(&s.source)
        .map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("source: {e}")))?;
    if !metric.allowed_sources.contains(&source) {
        return Err(reject(
            OUTCOME_UNSUPPORTED_METRIC,
            format!("metric {} does not allow source {}", s.metric_id, s.source),
        ));
    }
    let kind = MetricKind::from_str(&s.kind)
        .map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("kind: {e}")))?;
    if kind != metric.kind {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!(
                "metric {} is a {:?}, not {}",
                s.metric_id, metric.kind, s.kind
            ),
        ));
    }
    let unit =
        Unit::from_str(&s.unit).map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("unit: {e}")))?;
    if unit != metric.unit {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!(
                "metric {} is measured in {:?}, not {}",
                s.metric_id, metric.unit, s.unit
            ),
        ));
    }

    let quality = SampleQuality::parse(&s.quality).ok_or_else(|| {
        reject(
            OUTCOME_INVALID_BATCH,
            format!("unknown quality {}", s.quality),
        )
    })?;
    let value = match (s.value, quality) {
        (Some(RawValue::Float(v)), SampleQuality::Valid) => Some(SampleValue::Float(v)),
        (Some(RawValue::Integer(v)), SampleQuality::Valid) => Some(SampleValue::Integer(v)),
        (None, SampleQuality::Valid) => {
            return Err(reject(
                OUTCOME_INVALID_BATCH,
                format!("metric {} claims quality valid with no value", s.metric_id),
            ));
        }
        (Some(_), _) => {
            return Err(reject(
                OUTCOME_INVALID_BATCH,
                format!(
                    "metric {} carries a value with non-valid quality {}",
                    s.metric_id, s.quality
                ),
            ));
        }
        (None, _) => None,
    };
    // Counters are integer-valued on the wire and in the store
    // (exact decimal-string deltas): a float counter would be
    // accepted and durably stored, then never surfaced by the
    // integer-only query/rollup paths — reject it at the boundary
    // instead of committing data that can never be read back.
    if metric.kind == MetricKind::Counter {
        if let Some(SampleValue::Float(_)) = value {
            return Err(reject(
                OUTCOME_INVALID_BATCH,
                format!(
                    "counter metric {} must carry an integer value, not a float",
                    s.metric_id
                ),
            ));
        }
    }
    // Unit-aware value domains (metrics contract v1), registry-driven
    // by unit: boolean-unit state metrics carry exactly the integer
    // 0/1, never a float or another integer. `check.status` is the
    // explicit special case — its unit is Count, but the contract
    // pins its wire value to the typed state code: "a typed state,
    // not a float. The wire sample value is the integer state code:
    // 0=ok, 1=warning, 2=critical, 3=unknown; the manager rejects any
    // other encoding."
    if metric.kind == MetricKind::State {
        if let Some(v) = value {
            let in_domain = match metric.unit {
                Unit::Boolean => matches!(v, SampleValue::Integer(0 | 1)),
                _ if s.metric_id == "check.status" => matches!(v, SampleValue::Integer(0..=3)),
                _ => true,
            };
            if !in_domain {
                return Err(reject(
                    OUTCOME_INVALID_BATCH,
                    format!(
                        "state metric {} carries a value outside its unit domain: {v:?}",
                        s.metric_id
                    ),
                ));
            }
        }
    }

    // Timestamp bounds: live raw ingestion only.
    let age = now_ms - s.observed_at_ms;
    if age > MAX_SAMPLE_AGE_MS {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!(
                "metric {} observation is {} ms old (max {})",
                s.metric_id, age, MAX_SAMPLE_AGE_MS
            ),
        ));
    }
    if age < -MAX_FUTURE_SKEW_MS {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!(
                "metric {} observation is {} ms in the future (max skew {})",
                s.metric_id, -age, MAX_FUTURE_SKEW_MS
            ),
        ));
    }

    let builder = SampleBuilder::new(
        target_kind,
        &s.target_id,
        &s.metric_id,
        source,
        s.observed_at_ms as u64,
    )
    .map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("sample rejected: {e}")))?;
    let builder = match quality {
        SampleQuality::Valid => builder,
        other => builder.quality(other),
    };
    let builder = match value {
        Some(v) => builder.value(v),
        None => builder,
    };
    // Counter-epoch fence: build() rejects a valid counter sample
    // without one (a restarted counter source must never be
    // subtracted as a delta). Non-counter series may omit it.
    let builder = match (s.boot_id.as_str(), s.identity_epoch.as_str()) {
        (boot, epoch) if !boot.is_empty() && !epoch.is_empty() => builder.epoch(boot, epoch),
        _ => builder,
    };
    let mut builder = builder;
    for (k, v) in &s.dimensions {
        builder = builder
            .dimension(k, v)
            .map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("dimension: {e}")))?;
    }
    builder
        .build()
        .map_err(|e| reject(OUTCOME_INVALID_BATCH, format!("sample rejected: {e}")))
}

// ---------------------------------------------------------------------------
// Check records (agent spec §"Check records"; ingestion contract v1
// `checks` array)
// ---------------------------------------------------------------------------

/// A check record in its raw wire form, before validation. Both
/// transports map their wire types into this shape (mirrors
/// [`RawSample`]).
#[derive(Debug, Clone)]
pub struct RawCheck {
    pub schema_version: i32,
    pub check_id: String,
    pub service_key: Option<String>,
    pub status: String,
    pub summary: Option<String>,
    pub observed_at_ms: i64,
}

/// Validate one identifier field (`check_id`, `service_key`):
/// non-empty, at most [`MAX_CHECK_ID_BYTES`] bytes, and the printable
/// identifier charset `[A-Za-z0-9._:/-]` only (the contract's
/// dimension-value rule: values must be printable and rejected — not
/// truncated — when out of bounds).
fn validate_check_identifier(field: &str, value: &str) -> Result<(), SampleRejection> {
    if value.is_empty() {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!("{field} must not be empty"),
        ));
    }
    if value.len() > MAX_CHECK_ID_BYTES {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!("{field} exceeds {MAX_CHECK_ID_BYTES} bytes"),
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-' | b'@'))
    {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!("{field} must contain only [A-Za-z0-9._:/@-]"),
        ));
    }
    Ok(())
}

/// Validate one raw check record against the agent spec's check rules.
/// Whole-batch rejection semantics: any error here rejects the batch.
///
/// The manager is the untrusted boundary — the agent sanitizes before
/// sending, so an oversized or control-bearing summary is rejected
/// here, never silently stripped or truncated into something
/// misleading.
pub fn validate_check(c: &RawCheck, now_ms: i64) -> Result<CheckRecord, SampleRejection> {
    if c.schema_version != 1 {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            "check schema_version must be 1",
        ));
    }
    validate_check_identifier("check_id", &c.check_id)?;
    if let Some(key) = &c.service_key {
        validate_check_identifier("service_key", key)?;
    }
    let status = CheckStatus::parse(&c.status).ok_or_else(|| {
        reject(
            OUTCOME_INVALID_BATCH,
            format!(
                "unknown check status {} (must be ok|warning|critical|unknown)",
                c.status
            ),
        )
    })?;
    if let Some(summary) = &c.summary {
        if summary.len() > MAX_SUMMARY_BYTES {
            return Err(reject(
                OUTCOME_INVALID_BATCH,
                format!("check summary exceeds {MAX_SUMMARY_BYTES} bytes"),
            ));
        }
        if summary.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(reject(
                OUTCOME_INVALID_BATCH,
                "check summary must not contain control characters",
            ));
        }
    }
    // Same live-raw-ingestion timestamp window as samples: bounded
    // past age, bounded future skew.
    let age = now_ms - c.observed_at_ms;
    if age > MAX_SAMPLE_AGE_MS {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!(
                "check {} observation is {} ms old (max {})",
                c.check_id, age, MAX_SAMPLE_AGE_MS
            ),
        ));
    }
    if age < -MAX_FUTURE_SKEW_MS {
        return Err(reject(
            OUTCOME_INVALID_BATCH,
            format!(
                "check {} observation is {} ms in the future (max skew {})",
                c.check_id, -age, MAX_FUTURE_SKEW_MS
            ),
        ));
    }
    Ok(CheckRecord {
        check_id: c.check_id.clone(),
        service_key: c.service_key.clone(),
        status,
        summary: c.summary.clone(),
        observed_at_ms: c.observed_at_ms as u64,
    })
}

/// Validate a batch's check records: the per-batch cap
/// ([`MAX_CHECKS_PER_BATCH`]), duplicate `check_id` within one batch,
/// then each record through [`validate_check`]. Whole-batch rejection
/// semantics — v1 never ACKs an ambiguous subset.
pub fn validate_checks(
    checks: &[RawCheck],
    now_ms: i64,
) -> Result<Vec<CheckRecord>, SampleRejection> {
    if checks.len() > MAX_CHECKS_PER_BATCH {
        return Err(reject(
            OUTCOME_BATCH_TOO_LARGE,
            format!(
                "{} checks exceeds the {} per-batch cap",
                checks.len(),
                MAX_CHECKS_PER_BATCH
            ),
        ));
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::with_capacity(checks.len());
    for c in checks {
        if !seen.insert(c.check_id.clone()) {
            return Err(reject(
                OUTCOME_INVALID_BATCH,
                format!("duplicate check_id {} within one batch", c.check_id),
            ));
        }
        out.push(validate_check(c, now_ms)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(metric_id: &str, source: &str) -> RawSample {
        RawSample {
            target_kind: "vm".into(),
            target_id: "vm-abcdefghijkl".into(),
            metric_id: metric_id.into(),
            source: source.into(),
            kind: "gauge".into(),
            unit: "count".into(),
            observed_at_ms: 1_000,
            quality: "valid".into(),
            value: Some(RawValue::Integer(42)),
            dimensions: BTreeMap::new(),
            boot_id: "boot-1".into(),
            identity_epoch: "agent-credential-generation-1".into(),
        }
    }

    #[test]
    fn unknown_metric_is_unsupported() {
        let s = raw("made.up.metric", "guest_agent");
        let err = validate_sample(&s, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_UNSUPPORTED_METRIC);
    }

    #[test]
    fn source_outside_allowlist_is_unsupported() {
        // guest metric claimed with a node source
        let s = raw("vm.guest.load1", "node_os");
        let err = validate_sample(&s, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_UNSUPPORTED_METRIC);
    }

    #[test]
    fn valid_guest_sample_passes() {
        let s = raw("vm.guest.load1", "guest_agent");
        let sample = validate_sample(&s, 2_000).expect("valid");
        assert_eq!(sample.metric_id, "vm.guest.load1");
    }

    #[test]
    fn valid_quality_requires_value() {
        let mut s = raw("vm.guest.load1", "guest_agent");
        s.value = None;
        let err = validate_sample(&s, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("no value"));
    }

    #[test]
    fn stale_observation_rejected() {
        let mut s = raw("vm.guest.load1", "guest_agent");
        s.observed_at_ms = 0;
        let err = validate_sample(&s, 10 * 60 * 1000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("old"));
    }

    // -- unit-aware value domains --------------------------------------

    fn state_raw(metric_id: &str, unit: &str, value: RawValue) -> RawSample {
        let mut s = raw(metric_id, "guest_agent");
        s.kind = "state".into();
        s.unit = unit.into();
        s.value = Some(value);
        s
    }

    #[test]
    fn boolean_state_metrics_accept_only_integer_zero_or_one() {
        // Metrics contract v1: boolean-unit state metrics carry
        // exactly the integer 0/1 on the wire.
        for v in [0u64, 1] {
            let s = state_raw("vm.guest.service.up", "boolean", RawValue::Integer(v));
            let sample = validate_sample(&s, 2_000).expect("boolean 0/1 accepted");
            assert_eq!(sample.value, Some(SampleValue::Integer(v)));
        }
        // Integer 2 is outside the boolean domain.
        let s = state_raw("vm.guest.service.up", "boolean", RawValue::Integer(2));
        let err = validate_sample(&s, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("domain"), "{err:?}");
        // A float boolean is a wrong encoding, never coerced.
        let s = state_raw("vm.guest.service.up", "boolean", RawValue::Float(0.0));
        let err = validate_sample(&s, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("domain"), "{err:?}");
    }

    #[test]
    fn check_status_accepts_only_the_typed_state_codes() {
        // Metrics contract v1, `check.status`: the wire sample value is
        // the integer state code 0..=3 — never a float, never another
        // integer.
        for code in [0u64, 1, 2, 3] {
            let s = state_raw("check.status", "count", RawValue::Integer(code));
            let sample = validate_sample(&s, 2_000).expect("state code accepted");
            assert_eq!(sample.value, Some(SampleValue::Integer(code)));
        }
        let s = state_raw("check.status", "count", RawValue::Integer(4));
        let err = validate_sample(&s, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("domain"), "{err:?}");
        let s = state_raw("check.status", "count", RawValue::Float(1.0));
        let err = validate_sample(&s, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("domain"), "{err:?}");
    }

    // -- check records --------------------------------------------------

    fn raw_check() -> RawCheck {
        RawCheck {
            schema_version: 1,
            check_id: "service:nginx.service".into(),
            service_key: Some("nginx.service".into()),
            status: "ok".into(),
            summary: Some("active (running)".into()),
            observed_at_ms: 1_000,
        }
    }

    #[test]
    fn valid_check_record_passes() {
        let record = validate_check(&raw_check(), 2_000).expect("valid");
        assert_eq!(record.check_id, "service:nginx.service");
        assert_eq!(record.service_key.as_deref(), Some("nginx.service"));
        assert_eq!(record.status, CheckStatus::Ok);
        assert_eq!(record.summary.as_deref(), Some("active (running)"));
        assert_eq!(record.observed_at_ms, 1_000);
        // Every status string parses to its typed state.
        for (s, status) in [
            ("ok", CheckStatus::Ok),
            ("warning", CheckStatus::Warning),
            ("critical", CheckStatus::Critical),
            ("unknown", CheckStatus::Unknown),
        ] {
            let mut c = raw_check();
            c.status = s.into();
            assert_eq!(validate_check(&c, 2_000).unwrap().status, status);
        }
    }

    #[test]
    fn check_record_rejections_are_whole_batch() {
        // Wrong schema version.
        let mut c = raw_check();
        c.schema_version = 2;
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("schema_version"));

        // Empty, oversized and bad-charset check_id — rejected, never
        // truncated or repaired.
        for (check_id, needle) in [
            (String::new(), "empty"),
            ("x".repeat(129), "128 bytes"),
            ("bad id!".to_string(), "must contain only"),
        ] {
            let mut c = raw_check();
            c.check_id = check_id;
            let err = validate_check(&c, 2_000).unwrap_err();
            assert_eq!(err.outcome, OUTCOME_INVALID_BATCH, "{err:?}");
            assert!(err.detail.contains(needle), "{err:?}");
        }

        // service_key follows the same identifier rules (and may be
        // absent).
        let mut c = raw_check();
        c.service_key = Some("has space".into());
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("service_key"));
        let mut c = raw_check();
        c.service_key = None;
        assert!(validate_check(&c, 2_000).is_ok());

        // Unknown status string.
        let mut c = raw_check();
        c.status = "fine".into();
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("unknown check status"));

        // Summary: oversized and control-bearing are rejected, never
        // silently stripped (the manager is the untrusted boundary).
        let mut c = raw_check();
        c.summary = Some("x".repeat(257));
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("256 bytes"));
        let mut c = raw_check();
        c.summary = Some("bad\nsummary".into());
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("control"));
        let mut c = raw_check();
        c.summary = Some("bad\u{7f}summary".into());
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("control"));

        // Timestamps: same window as samples (5 min past / 2 min
        // future).
        let mut c = raw_check();
        c.observed_at_ms = 0;
        let err = validate_check(&c, 10 * 60 * 1000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("old"));
        let mut c = raw_check();
        c.observed_at_ms = 130_000;
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("future"));
    }

    #[test]
    fn check_identifiers_accept_systemd_instance_units() {
        // G4 real-VM lesson (the gate exists to catch exactly this):
        // bounded service discovery on any real systemd host finds
        // INSTANCE units — `user@1000.service` starts the moment a
        // user logs in, `systemd-fsck@dev-sda1.service` on every boot.
        // The check-id charset must carry their `@`, or every batch
        // containing one is rejected whole and the agent's store
        // connection silently starves.
        let mut c = raw_check();
        c.check_id = "service:user@1000.service".into();
        c.service_key = Some("user@1000.service".into());
        let record = validate_check(&c, 2_000).unwrap();
        assert_eq!(record.check_id, "service:user@1000.service");
        assert_eq!(record.service_key.as_deref(), Some("user@1000.service"));

        let mut c = raw_check();
        c.check_id = "service:systemd-fsck@dev-sda1.service".into();
        assert!(validate_check(&c, 2_000).is_ok());

        // The amendment widens ONLY the identifier charset: control
        // characters and spaces remain whole-batch rejections.
        let mut c = raw_check();
        c.check_id = "service:bad\nid".into();
        let err = validate_check(&c, 2_000).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("must contain only"));
    }

    #[test]
    fn check_batch_caps_and_duplicates_reject_whole_batch() {
        let now = 2_000i64;
        // 128 checks is the cap; 129 is batch_too_large.
        let cap: Vec<RawCheck> = (0..MAX_CHECKS_PER_BATCH as i64)
            .map(|i| {
                let mut c = raw_check();
                c.check_id = format!("http:local:{i}");
                c.observed_at_ms = now - 100 + i;
                c
            })
            .collect();
        assert_eq!(
            validate_checks(&cap, now).unwrap().len(),
            MAX_CHECKS_PER_BATCH
        );
        let mut over = cap.clone();
        over.push(raw_check());
        let err = validate_checks(&over, now).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_BATCH_TOO_LARGE);

        // A duplicate check_id within one batch is ambiguous — the
        // whole batch is rejected, never an ACKed subset.
        let mut dup = vec![raw_check(), raw_check()];
        dup[1].status = "critical".into();
        let err = validate_checks(&dup, now).unwrap_err();
        assert_eq!(err.outcome, OUTCOME_INVALID_BATCH);
        assert!(err.detail.contains("duplicate check_id"));
    }
}
