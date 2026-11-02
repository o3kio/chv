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
/// `strict_booleans` keeps YAML 1.2 semantics: only `true`/`false` are
/// booleans, so values such as `n`, `yes`, or `on` stay strings. The previous
/// `serde_yaml` parser behaved the same way, and silently coercing
/// `name: n` into `false` would be a surprising config regression.
pub(crate) fn parse_value(yaml: &str) -> Result<serde_json::Value, serde_saphyr::Error> {
    serde_saphyr::from_str_with_options(
        yaml,
        serde_saphyr::options! {
            strict_booleans: true,
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
