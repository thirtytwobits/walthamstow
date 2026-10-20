//! Copyright (c) Scott A Dixon
//!
//! Loading a document schema and validating documents against it.

use anyhow::{anyhow, Result};
use jsonschema::{validator_for, Validator};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Reads a YAML schema document, without the `$schema` keywords the validator does not take.
pub fn load_schema_document(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)?;
    let mut schema: Value = serde_yaml::from_str(&text)?;
    normalize_schema_document(&mut schema);
    Ok(schema)
}

/// Compiles `schema`, naming `path` in a failure.
pub fn compile_schema_document(schema: &Value, path: &Path) -> Result<Validator> {
    validator_for(schema).map_err(|error| anyhow!("{}: {error}", path.display()))
}

/// Removes the `$schema` keywords, which the validator does not take, at every depth.
pub fn normalize_schema_document(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            for entry in map.values_mut() {
                normalize_schema_document(entry);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_schema_document(item);
            }
        }
        _ => {}
    }
}

/// Refuses `value` with every schema failure, each prefixed with `file_label`.
pub fn validate_document_value(
    validator: &Validator,
    value: &Value,
    file_label: &str,
) -> Result<()> {
    let issues = collect_validation_messages(validator, value, file_label);
    if issues.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(issues.join(" ")))
    }
}

/// Every schema failure in `value`, each prefixed with `file_label`.
pub fn collect_validation_messages(
    validator: &Validator,
    value: &Value,
    file_label: &str,
) -> Vec<String> {
    validator
        .iter_errors(value)
        .map(|error| format!("{file_label}: {error}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema_file(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("document.schema.yaml");
        fs::write(&path, text).unwrap();
        (dir, path)
    }

    const SCHEMA: &str = r#"
$schema: https://json-schema.org/draft/2020-12/schema
type: object
required: [name]
properties:
  name:
    $schema: https://json-schema.org/draft/2020-12/schema
    type: string
"#;

    #[test]
    fn a_loaded_schema_holds_no_schema_keywords_at_any_depth() {
        let (_dir, path) = schema_file(SCHEMA);
        let schema = load_schema_document(&path).unwrap();
        fn holds_keyword(value: &Value) -> bool {
            match value {
                Value::Object(map) => {
                    map.contains_key("$schema") || map.values().any(holds_keyword)
                }
                Value::Array(items) => items.iter().any(holds_keyword),
                _ => false,
            }
        }
        assert!(!holds_keyword(&schema));
    }

    #[test]
    fn validation_names_the_file_in_each_failure_and_accepts_a_valid_document() {
        let (_dir, path) = schema_file(SCHEMA);
        let validator =
            compile_schema_document(&load_schema_document(&path).unwrap(), &path).unwrap();
        let label = "hero.yaml";
        validate_document_value(&validator, &json!({ "name": "Ariadne" }), label).unwrap();
        let messages = collect_validation_messages(&validator, &json!({ "name": 7 }), label);
        assert!(!messages.is_empty());
        assert!(messages.iter().all(|message| message.starts_with(label)));
        assert!(validate_document_value(&validator, &json!({}), label).is_err());
    }

    #[test]
    fn a_schema_that_does_not_compile_names_its_file() {
        let path = Path::new("broken.schema.yaml");
        let error = compile_schema_document(&json!({ "type": 7 }), path).unwrap_err();
        assert!(error.to_string().contains("broken.schema.yaml"));
    }
}
