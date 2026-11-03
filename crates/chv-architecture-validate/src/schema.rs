//! JSON-Schema validation against the embedded
//! `chvarchitecture-v1alpha1.schema.json`.
//!
//! The schema is embedded at compile time via `include_str!` so the
//! validator never needs filesystem access at runtime, and is compiled once
//! per process via `OnceLock`.
//!
//! Each individual JSON-Schema error produces ONE [`Finding`]; we never
//! aggregate. Aggregation hides distinct violations from the operator.

use chv_controlplane_types::architecture::{Finding, Severity};
use jsonschema::Validator;
use std::borrow::Cow;
use std::sync::OnceLock;

use crate::codes::SCHEMA_INVALID;

const SCHEMA_JSON: &str =
    include_str!("../../../docs/schemas/chvarchitecture-v1alpha1.schema.json");

fn compiled_schema() -> &'static Validator {
    static SCHEMA: OnceLock<Validator> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let value: serde_json::Value = serde_json::from_str(SCHEMA_JSON)
            .expect("embedded chvarchitecture-v1alpha1.schema.json must be valid JSON");
        jsonschema::validator_for(&value)
            .expect("embedded chvarchitecture-v1alpha1.schema.json must compile as a JSON Schema")
    })
}

/// Run JSON-Schema validation against the YAML string.
///
/// Returns one `Finding` per schema violation, with `severity = error` and
/// `blocking = true`. An empty result means the schema validation passed.
///
/// A YAML syntax error is also reported as a single SCHEMA_INVALID finding
/// (rather than panicking) so callers always receive a flat findings list.
pub fn validate_against_schema(yaml_str: &str) -> Vec<Finding> {
    // Parse YAML directly into the JSON value model the schema validator
    // consumes. serde-saphyr is a safe-Rust YAML parser; serde_json::Value
    // is the intermediate representation shared with the typed model.
    let json_value: serde_json::Value = match crate::parse::parse_value(yaml_str) {
        Ok(v) => v,
        Err(err) => {
            return vec![Finding {
                severity: Severity::Error,
                code: Cow::Borrowed(SCHEMA_INVALID),
                message: format!("yaml syntax error: {err}"),
                path: None,
                resource_ref: None,
                blocking: true,
                suggestion: Some("fix the YAML syntax".to_string()),
            }];
        }
    };

    let schema = compiled_schema();
    schema
        .iter_errors(&json_value)
        .map(|err| Finding {
            severity: Severity::Error,
            code: Cow::Borrowed(SCHEMA_INVALID),
            message: err.to_string(),
            path: Some(err.instance_path().to_string()),
            resource_ref: None,
            blocking: true,
            suggestion: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_minimal_passes_schema() {
        let yaml = r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
"#;
        let f = validate_against_schema(yaml);
        assert!(f.is_empty(), "minimal doc should pass schema; got {f:#?}");
    }

    #[test]
    fn missing_metadata_emits_finding() {
        let yaml = r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
"#;
        let f = validate_against_schema(yaml);
        assert!(!f.is_empty());
        assert_eq!(f[0].code.as_ref(), SCHEMA_INVALID);
    }

    #[test]
    fn wrong_api_version_emits_finding() {
        let yaml = r#"
apiVersion: chv.kubedo.io/v999
kind: CHVArchitecture
metadata:
  name: t1
"#;
        let f = validate_against_schema(yaml);
        assert!(!f.is_empty());
        assert!(f.iter().all(|x| x.code.as_ref() == SCHEMA_INVALID));
    }

    #[test]
    fn unknown_top_level_field_violates_additional_properties() {
        let yaml = r#"
apiVersion: chv.kubedo.io/v1alpha1
kind: CHVArchitecture
metadata:
  name: t1
unexpected_top_level: 1
"#;
        let f = validate_against_schema(yaml);
        assert!(!f.is_empty(), "additionalProperties:false must fire");
    }

    #[test]
    fn syntax_error_becomes_one_finding() {
        let yaml = "this: is\n  bad: indent: yaml";
        let f = validate_against_schema(yaml);
        assert_eq!(f.len(), 1);
        assert!(f[0].message.contains("yaml syntax error"));
    }

    #[test]
    fn schema_compiles_once() {
        // Sanity: the OnceLock cache should produce the same validator
        // across calls. We can't compare validators directly, but we can
        // verify both calls succeed without panicking.
        let _ = compiled_schema();
        let _ = compiled_schema();
    }
}
