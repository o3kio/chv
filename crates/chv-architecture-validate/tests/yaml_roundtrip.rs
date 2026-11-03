//! Phase-7 D4 — YAML round-trip test for `CHVArchitecture` fixtures.
//!
//! Asserts that every fixture survives a full
//! `parse_yaml → to_yaml → parse_yaml` cycle without losing structural
//! information. We compare the **two parsed models** (model_a vs model_b),
//! not raw bytes: the YAML emitter does not guarantee byte-identical output
//! (key ordering, scalar quoting), and the durable contract is "no
//! data loss across the round-trip", which structural equality captures.
//!
//! Fixtures live under `tests/fixtures/` and are progressively richer.
//! Each one ADDS a coverage axis on top of the previous:
//!
//! * `minimal.yaml` — smallest non-trivial topology. Covers: servers,
//!   networks, instances. (3 axes)
//! * `multi_tier.yaml` — adds: tiered server placement (web/app/db),
//!   datastores, images, templates, multiple NIC attachments per
//!   instance. (5 new axes; 8 cumulative)
//! * `production_full.yaml` — adds: `environment: production`,
//!   backup_targets, backup_policies, ssh_keys, instance_users, roles,
//!   users, projects. (8 new axes; 16 cumulative — every resource kind
//!   the model defines).
//!
//! Negative-path fixtures (unknown fields, duplicate names, missing
//! required keys) are NOT covered here — the spec scopes D4 to "every
//! fixture in `tests/fixtures/`" and those are happy-path. Parse-error
//! coverage lives in `chv-architecture-validate`'s own unit tests
//! (`src/parse.rs`'s embedded `#[cfg(test)] mod tests`).
//!
//! `CHVArchitecture` derives `PartialEq` (verified in `model.rs`), so we
//! can use plain `assert_eq!` for the round-trip equality check. No JSON
//! fall-back is required.

use chv_architecture_validate::parse::{parse_yaml, to_yaml};

fn assert_round_trip(label: &str, yaml: &str) {
    let model_a =
        parse_yaml(yaml).unwrap_or_else(|e| panic!("fixture {label} failed to parse: {e}"));
    let regen =
        to_yaml(&model_a).unwrap_or_else(|e| panic!("fixture {label} failed to re-emit: {e}"));
    let model_b = parse_yaml(&regen).unwrap_or_else(|e| {
        panic!("fixture {label} round-trip re-parse failed: {e}\nemitted:\n{regen}")
    });
    assert_eq!(
        model_a, model_b,
        "fixture {label} lost structural data across round-trip"
    );
}

#[test]
fn minimal_round_trips() {
    let yaml = include_str!("fixtures/minimal.yaml");
    assert_round_trip("minimal.yaml", yaml);
}

#[test]
fn multi_tier_round_trips() {
    let yaml = include_str!("fixtures/multi_tier.yaml");
    assert_round_trip("multi_tier.yaml", yaml);
}

#[test]
fn production_full_round_trips() {
    let yaml = include_str!("fixtures/production_full.yaml");
    assert_round_trip("production_full.yaml", yaml);
}
