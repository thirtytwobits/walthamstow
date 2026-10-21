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

/// Removes the `$schema` keyword from every subschema, and from the schema itself unless it
/// names a dialect the validator knows: a known dialect selects the draft the schema is
/// validated under, and an unknown one is refused by the validator. Values that are not
/// schemas, such as a property's name or a `const`, are left as they are.
pub fn normalize_schema_document(schema: &mut Value) {
    let dialect = schema
        .get("$schema")
        .cloned()
        .filter(|_| jsonschema::Draft::default().detect(schema).is_ok());
    strip_schema_keywords(schema);
    if let (Some(dialect), Value::Object(map)) = (dialect, schema) {
        map.insert("$schema".to_string(), dialect);
    }
}

/// Removes the `$schema` keyword from `schema` and every subschema within it.
fn strip_schema_keywords(schema: &mut Value) {
    let Value::Object(map) = schema else {
        return;
    };
    map.remove("$schema");
    for (keyword, value) in map.iter_mut() {
        match keyword.as_str() {
            // One subschema, or in older drafts an array of them.
            "additionalProperties"
            | "additionalItems"
            | "contains"
            | "else"
            | "if"
            | "items"
            | "not"
            | "propertyNames"
            | "then"
            | "unevaluatedItems"
            | "unevaluatedProperties" => strip_subschemas(value),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => strip_subschemas(value),
            // Subschemas by name; in older drafts a dependency is a schema or a list of names.
            "$defs" | "definitions" | "dependentSchemas" | "patternProperties" | "properties"
            | "dependencies" => {
                if let Value::Object(named) = value {
                    named.values_mut().for_each(strip_schema_keywords);
                }
            }
            _ => {}
        }
    }
}

/// Strips `value`, a subschema or an array of subschemas.
fn strip_subschemas(value: &mut Value) {
    match value {
        Value::Array(subschemas) => subschemas.iter_mut().for_each(strip_schema_keywords),
        subschema => strip_schema_keywords(subschema),
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
    fn a_loaded_schema_keeps_its_known_dialect_and_no_schema_keyword_below_it() {
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
        assert!(jsonschema::Draft::default().detect(&schema).is_ok());
        assert!(schema.get("$schema").is_some());
        assert!(!holds_keyword(&schema["properties"]));
    }

    #[test]
    fn a_known_dialect_selects_the_draft_and_an_unknown_one_is_dropped() {
        let tuple =
            json!({ "type": "array", "items": [{ "type": "string" }, { "type": "integer" }] });
        let mut draft_7 = tuple.clone();
        draft_7["$schema"] = json!("http://json-schema.org/draft-07/schema#");
        normalize_schema_document(&mut draft_7);
        let validator =
            compile_schema_document(&draft_7, Path::new("draft-7.schema.yaml")).unwrap();
        assert!(validator.is_valid(&json!(["a", 1])));
        assert!(
            !validator.is_valid(&json!([1, "a"])),
            "items are judged as a tuple"
        );

        let mut unknown = json!({ "type": "object", "required": ["name"] });
        unknown["$schema"] = json!("http://stsci.edu/schemas/yaml-schema/draft-01");
        normalize_schema_document(&mut unknown);
        assert!(unknown.get("$schema").is_none());
        let validator =
            compile_schema_document(&unknown, Path::new("unknown.schema.yaml")).unwrap();
        assert!(!validator.is_valid(&json!({})));
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

    #[test]
    fn only_subschemas_lose_the_schema_keyword() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "$schema": { "type": "string" },
                "named": { "$schema": "https://json-schema.org/draft/2020-12/schema", "type": "string" },
                "fixed": { "const": { "$schema": "kept" }, "default": { "$schema": "kept" } }
            },
            "items": [{ "$schema": "x" }],
            "anyOf": [{ "$schema": "x", "type": "object" }]
        });
        normalize_schema_document(&mut schema);
        assert!(
            schema.get("$schema").is_some(),
            "the root's known dialect is kept"
        );
        assert!(
            schema["properties"].get("$schema").is_some(),
            "a property's name is kept"
        );
        assert!(schema["properties"]["named"].get("$schema").is_none());
        assert_eq!(schema["properties"]["fixed"]["const"]["$schema"], "kept");
        assert_eq!(schema["properties"]["fixed"]["default"]["$schema"], "kept");
        assert!(schema["items"][0].get("$schema").is_none());
        assert!(schema["anyOf"][0].get("$schema").is_none());
    }
}
