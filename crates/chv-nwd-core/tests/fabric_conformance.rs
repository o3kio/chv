//! CHV-side gate for the shared fabric provider contract.
//!
//! The `fabric-conformance` crate ships one executable suite that runs
//! against the reference in-memory fake kernel (`fabric_linux::RecordingRunner`).
//! Running it here as an integration test keeps the CHV build pinned to a
//! `fabric` provider revision that satisfies the fabric provider contract
//! (`contracts/fabric-provider-v2.md` in the o3kio/fabric repository), so a
//! future provider bump that breaks the contract fails CHV CI instead of
//! surfacing on hosts.
//!
//! Per ADR-021 and the implementation plan (Phase 1), this is the
//! unprivileged conformance gate; the privileged multi-host gate is a
//! separate harness in the fabric repository.

use fabric_conformance::run_suite;

#[test]
fn fabric_provider_conformance_suite_passes() {
    let report = run_suite();
    for case in &report.results {
        assert!(
            case.passed,
            "fabric conformance case '{}' failed: {}",
            case.name,
            case.detail.as_deref().unwrap_or("no detail")
        );
    }
    assert!(
        report.passed(),
        "fabric conformance suite must pass in full"
    );
}
