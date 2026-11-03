//! YAML parse and emit for `CHVArchitecture`.
//!
//! Parsing splits two failure modes:
//!
//! - `WrongKind` — the document parsed as YAML, but its `apiVersion` or
//!   `kind` is not the expected `chv.kubedo.io/v1alpha1 / CHVArchitecture`.
//!   Surfacing this distinctly lets the BFF give a clearer error than a
//!   generic "missing field" message.
//! - `YamlSyntax` — anything else: malformed YAML, type mismatches, etc.

use crate::model::CHVArchitecture;

/// Identity strings the validator recognises.
pub const EXPECTED_API_VERSION: &str = "chv.kubedo.io/v1alpha1";
pub const EXPECTED_KIND: &str = "CHVArchitecture";

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("yaml syntax error: {0}")]
    YamlSyntax(#[from] serde_saphyr::Error),
    #[error("yaml model error: {0}")]
    Model(#[from] serde_json::Error),
    #[error("yaml emit error: {0}")]
    YamlEmit(#[from] serde_saphyr::SerializeError),
    #[error(
        "wrong document kind: expected apiVersion={EXPECTED_API_VERSION} kind={EXPECTED_KIND}, found {found}"
    )]
    WrongKind { found: String },
}

/// Parse YAML into the JSON value model shared with the typed model and the
/// JSON-Schema validator.
///
/// The option set keeps the parts of the old `serde_yaml` 0.9 behavior this
/// contract depends on — boolean typing, merge-key handling, non-finite
/// floats, and error shape — rather than reproducing serde_yaml byte for
/// byte:
///
/// - `strict_booleans` keeps YAML 1.2 semantics: only `true`/`false` are
///   booleans, so values such as `n`, `yes`, or `on` stay strings. Silently
///   coercing `name: n` into `false` would be a surprising config regression.
/// - `merge_keys: AsOrdinary` keeps `<<` as a literal mapping key. serde_yaml
///   did not implement YAML 1.1 merge expansion, so accepting merged documents
///   would silently change what the schema validator and `Project.extras` see.
/// - `non_finite_float_policy: PassThrough` reproduces serde_yaml's path:
///   `.inf`/`.nan` reach `serde_json::Value` as `null` instead of rejecting the
///   whole document.
/// - the budget disables the alias/anchor ratio heuristic and raises the node
///   cap for fleet-scale documents, while keeping the default depth bound (a
///   deeper limit risks stack exhaustion in debug builds). The absolute
///   `max_aliases`/`max_events` caps still bound alias bombs.
/// - `with_snippet: false` keeps messages single-line, since findings are
///   rendered inline in the UI.
///
/// Numeric resolution is **not** fully identical. `010` resolves to `10.0`
/// (not the string `"010"`), `1_000` resolves to `1000`, and integers above
/// `u64::MAX` become floats. Those differences are documented in the YAML
/// contract and pinned by `numeric_scalar_resolution_is_pinned` so a future
/// upgrade cannot change them silently.
pub(crate) fn parse_value(yaml: &str) -> Result<serde_json::Value, serde_saphyr::Error> {
    serde_saphyr::from_str_with_options(
        yaml,
        serde_saphyr::options! {
            strict_booleans: true,
            merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
            non_finite_float_policy: serde_saphyr::NonFiniteFloatPolicy::PassThrough,
            with_snippet: false,
            budget: serde_saphyr::budget! {
                enforce_alias_anchor_ratio: false,
                max_nodes: 1_000_000,
            },
        },
    )
}

/// Parse an arbitrary YAML string into a strongly-typed `CHVArchitecture`.
///
/// Performs a cheap identity probe before binding to `CHVArchitecture` so
/// `WrongKind` errors take precedence over deeper schema mismatches; this is
/// what the BFF needs in order to give the user the most actionable error.
pub fn parse_yaml(yaml: &str) -> Result<CHVArchitecture, ParseError> {
    let raw: serde_json::Value = parse_value(yaml)?;
    let api_version = raw
        .get("apiVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("<missing>");
    let kind = raw
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("<missing>");
    if api_version != EXPECTED_API_VERSION || kind != EXPECTED_KIND {
        return Err(ParseError::WrongKind {
            found: format!("apiVersion={api_version} kind={kind}"),
        });
    }
    let model: CHVArchitecture = serde_json::from_value(raw)?;
    Ok(model)
}

/// Emit the canonical YAML form of a `CHVArchitecture`. Round-trips through
/// `parse_yaml` for a model that was itself built by `parse_yaml`. Note: a
/// hand-built model that uses `*::Unknown` enum fallbacks WILL NOT round-trip
/// (the typed model has no way to recover the original string); production
/// code should not construct models manually for emission.
pub fn to_yaml(model: &CHVArchitecture) -> Result<String, ParseError> {
    Ok(serde_saphyr::to_string(model)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> &'static str {
        r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
"#
    }

    #[test]
    fn parses_minimal_document() {
        let m = parse_yaml(minimal()).expect("parse");
        assert_eq!(m.api_version, EXPECTED_API_VERSION);
        assert_eq!(m.kind, EXPECTED_KIND);
        assert_eq!(m.metadata.name, "t1");
    }

    #[test]
    fn rejects_wrong_kind() {
        let err = parse_yaml(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: NotCHVArch
metadata:
  name: x
"#,
        )
        .expect_err("wrong kind");
        assert!(matches!(err, ParseError::WrongKind { .. }));
    }

    #[test]
    fn rejects_wrong_api_version() {
        let err = parse_yaml(
            r#"
apiVersion: chv.kubedo.io/v2
kind: CHVArchitecture
metadata:
  name: x
"#,
        )
        .expect_err("wrong api version");
        assert!(matches!(err, ParseError::WrongKind { .. }));
    }

    #[test]
    fn round_trip_minimal() {
        let m = parse_yaml(minimal()).expect("parse");
        let y = to_yaml(&m).expect("emit");
        let m2 = parse_yaml(&y).expect("re-parse");
        assert_eq!(m, m2);
    }

    /// YAML 1.2 scalar semantics — the behavior `serde_yaml` had, which this
    /// crate's configs were written against. Only `true`/`false` are booleans;
    /// YAML 1.1 spellings such as `n`, `yes`, and `on` must stay strings.
    /// Coercing `name: n` to `false` would silently corrupt a topology.
    #[test]
    fn yaml_12_scalars_stay_strings() {
        let raw = parse_value(
            "n_val: n\nyes_val: yes\non_val: on\noff_val: off\ntrue_val: true\nfalse_val: false\ntrue_mixed: True\ntrue_weird: tRuE\nfalse_upper: FALSE\n",
        )
        .expect("parse scalars");

        for (key, expected) in [
            ("n_val", "n"),
            ("yes_val", "yes"),
            ("on_val", "on"),
            ("off_val", "off"),
        ] {
            assert_eq!(
                raw.get(key).and_then(|v| v.as_str()),
                Some(expected),
                "{key} must remain the string {expected:?}"
            );
        }
        for key in ["true_val", "true_mixed", "true_weird"] {
            assert_eq!(raw.get(key), Some(&serde_json::Value::Bool(true)), "{key}");
        }
        for key in ["false_val", "false_upper"] {
            assert_eq!(raw.get(key), Some(&serde_json::Value::Bool(false)), "{key}");
        }
    }

    /// The same guarantee through the typed model: YAML 1.1 spellings survive
    /// in string fields, while real booleans still deserialize as booleans.
    #[test]
    fn yaml_12_scalars_survive_typed_parse() {
        let m = parse_yaml(
            r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: n
networks:
  - name: on
    type: bridge
    dhcp:
      enabled: true
"#,
        )
        .expect("parse");

        assert_eq!(m.metadata.name, "n");
        assert_eq!(m.networks[0].name, "on");
        let dhcp = m.networks[0].dhcp.as_ref().expect("dhcp present");
        assert!(dhcp.enabled, "`true` must still deserialize as bool");
    }

    /// serde_yaml kept `<<` as a literal key (it did not implement YAML 1.1
    /// merge expansion). Expanding it would change what the schema validator
    /// and `Project.extras` see, so the parser must keep the old shape.
    #[test]
    fn merge_keys_stay_literal() {
        let raw =
            parse_value("base: &b\n  x: 1\nderived:\n  <<: *b\n  y: 2\n").expect("parse merge");
        assert_eq!(raw["derived"]["<<"]["x"], serde_json::json!(1));
        assert_eq!(raw["derived"]["y"], serde_json::json!(2));
    }

    /// Non-finite floats used to reach the JSON layer as `null`, not as a
    /// parse failure. Rejecting the whole document would refuse saves the BFF
    /// previously accepted.
    #[test]
    fn non_finite_floats_become_null() {
        let raw = parse_value("pos: .inf\nneg: -.inf\nnan: .nan\n").expect("parse non-finite");
        for key in ["pos", "neg", "nan"] {
            assert!(raw[key].is_null(), "{key} should be null");
        }
    }

    /// A fleet document may legitimately merge one `defaults: &d` anchor into
    /// hundreds of items. serde-saphyr's default alias/anchor ratio rejects
    /// that at 100 aliases; the configured budget must not.
    #[test]
    fn many_aliases_of_one_anchor_parse() {
        let mut yaml = String::from("defaults: &d\n  cpu: 2\nitems:\n");
        for i in 0..250 {
            yaml.push_str(&format!("  - id: {i}\n    <<: *d\n"));
        }
        let raw = parse_value(&yaml).expect("250 aliases must parse");
        assert_eq!(raw["items"].as_array().expect("items").len(), 250);
    }

    /// Duplicate keys stay a hard error (both parsers agreed on this).
    #[test]
    fn duplicate_keys_still_error() {
        let err = parse_value("a: 1\na: 2\n").expect_err("duplicate key");
        assert!(format!("{err}").contains("duplicate"), "{err}");
    }

    /// Pin the numeric resolution differences that remain after configuring
    /// serde-saphyr, so a future upgrade cannot change them silently.
    #[test]
    fn numeric_scalar_resolution_is_pinned() {
        let raw =
            parse_value("leading_zero: 010\nunderscored: 1_000\nhuge: 18446744073709551616\n")
                .expect("parse numbers");
        assert!(
            raw["leading_zero"].is_f64(),
            "010 resolves to a float: {:?}",
            raw["leading_zero"]
        );
        assert_eq!(raw["underscored"], serde_json::json!(1000));
        assert!(raw["huge"].is_f64(), "over-u64 integers resolve to f64");
    }

    /// Findings are rendered inline in the UI; parser errors must stay on one
    /// line (serde-saphyr's rustc-style snippet is off).
    #[test]
    fn parser_errors_are_single_line() {
        let err = parse_value("a: [1, 2\n").expect_err("syntax error");
        assert!(!format!("{err}").contains('\n'), "{err}");
    }

    #[test]
    fn syntax_error_is_yaml_syntax() {
        let err = parse_yaml(
            "apiVersion: chv.kubedo.io/v1alpha1\nkind: CHVArchitecture\n  bad: indent: thing",
        )
        .expect_err("syntax err");
        assert!(matches!(err, ParseError::YamlSyntax(_)));
    }

    /// Property test: round-trip preserves equality for every randomly
    /// generated model whose surface stays in the *known* enum variants
    /// (no `Unknown` fallbacks — those by design lose the original string).
    /// We use a small but representative generator: random metadata name,
    /// 0..3 networks each with 0..2 instances. Enough to exercise vector
    /// concatenation, BTreeMap rendering, optional fields, and enums.
    #[test]
    fn round_trip_property() {
        use proptest::prelude::*;

        proptest!(ProptestConfig::with_cases(40), |(
            md_name in "[a-z][a-z0-9_-]{0,15}",
            netcount in 0usize..=3,
        )| {
            let mut yaml = format!(
                "apiVersion: chv.kubedo.io/v1alpha1\nkind: CHVArchitecture\nmetadata:\n  name: {md_name}\n"
            );
            if netcount > 0 {
                yaml.push_str("networks:\n");
                for i in 0..netcount {
                    yaml.push_str(&format!(
                        "  - name: net-{i}\n    type: bridge\n    cidr: 10.{i}.0.0/24\n"
                    ));
                }
            }
            let m = parse_yaml(&yaml).expect("parse");
            let y = to_yaml(&m).expect("emit");
            let m2 = parse_yaml(&y).expect("re-parse");
            prop_assert_eq!(m, m2);
        });
    }
}
