//! Copyright (c) Scott A Dixon
//!
//! A walk of a document's JSON Schema into one guidance entry per field, which a product
//! maps into its own description types.

use serde_json::Value;

/// One field entry emitted by the walker. Mirrors the shape of every
/// resource's per-field protocol type — store as raw strings; let the
/// caller map this into its typed equivalent.
pub struct WalkerField {
    pub path: String,
    pub field_type: String,
    pub required: bool,
    pub description: String,
    pub allowed_values: Vec<String>,
}

/// Walks the canonical JSON Schema for a document and emits one
/// `WalkerField` per dot-pathed property (recursing into nested
/// objects and `$ref`s, expanding arrays of objects into
/// `<path>[].<field>` entries). Caller-provided `emit` collects.
pub fn walk_describe_schema<F: FnMut(WalkerField)>(schema: &Value, mut emit: F) {
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    walk_object_schema(schema, "", &defs, &mut emit);
}

fn walk_object_schema<F: FnMut(WalkerField)>(
    node: &Value,
    path: &str,
    defs: &serde_json::Map<String, Value>,
    emit: &mut F,
) {
    let required: std::collections::HashSet<&str> = node
        .get("required")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = node.get("properties").and_then(Value::as_object) else {
        return;
    };

    for (name, property) in properties {
        let resolved = resolve_ref(property, defs);
        let field_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}.{name}")
        };
        let field_type = type_label(property, &resolved, defs);
        let description = description_for(property, &resolved);
        let allowed_values = match enum_values(property) {
            direct if direct.is_empty() => enum_values(&resolved),
            direct => direct,
        };
        emit(WalkerField {
            path: field_path.clone(),
            field_type,
            required: required.contains(name.as_str()),
            description,
            allowed_values,
        });

        let raw_type = property
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| resolved.get("type").and_then(Value::as_str))
            .unwrap_or("");
        if raw_type == "array" {
            let array = if property.get("items").is_some() {
                property
            } else {
                &resolved
            };
            if let Some(item_schema) = array_item_schema(array, defs) {
                if matches_object(&item_schema) {
                    let array_path = format!("{field_path}[]");
                    walk_object_schema(&item_schema, &array_path, defs, emit);
                }
            }
        } else if matches_object(&resolved) {
            walk_object_schema(&resolved, &field_path, defs, emit);
        }
    }
}

fn resolve_ref(node: &Value, defs: &serde_json::Map<String, Value>) -> Value {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return node.clone();
    };
    let Some(name) = reference.strip_prefix("#/$defs/") else {
        return node.clone();
    };
    defs.get(name).cloned().unwrap_or_else(|| node.clone())
}

fn type_label(node: &Value, resolved: &Value, defs: &serde_json::Map<String, Value>) -> String {
    let typed = if node.get("type").is_some() {
        node
    } else {
        resolved
    };
    match typed.get("type").and_then(Value::as_str) {
        Some("array") => match array_item_schema(typed, defs) {
            Some(item) => format!("array of {}", type_label(&item, &item, defs)),
            None => "array".to_string(),
        },
        Some(declared) => declared.to_string(),
        None if node.get("$ref").is_some() => "object".to_string(),
        None => "any".to_string(),
    }
}

fn description_for(node: &Value, resolved: &Value) -> String {
    node.get("description")
        .and_then(Value::as_str)
        .or_else(|| resolved.get("description").and_then(Value::as_str))
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn enum_values(node: &Value) -> Vec<String> {
    node.get("enum")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn array_item_schema(node: &Value, defs: &serde_json::Map<String, Value>) -> Option<Value> {
    let items = node.get("items")?;
    Some(resolve_ref(items, defs))
}

fn matches_object(node: &Value) -> bool {
    matches!(node.get("type").and_then(Value::as_str), Some("object"))
        || node.get("properties").is_some()
}

/// Top-level summary text the schema declares (used to populate the
/// `summary` field on each describe result).
pub fn schema_summary(schema: &Value) -> String {
    schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(schema: &Value) -> Vec<WalkerField> {
        let mut fields = Vec::new();
        walk_describe_schema(schema, |field| fields.push(field));
        fields
    }

    #[test]
    fn an_enum_gives_its_allowed_values_directly_or_through_a_reference() {
        let allowed = ["draft", "final"];
        let schema = json!({
            "$defs": { "Status": { "type": "string", "enum": allowed } },
            "type": "object",
            "properties": {
                "direct": { "type": "string", "enum": allowed },
                "referenced": { "$ref": "#/$defs/Status" },
                "plain": { "type": "string" }
            }
        });
        for field in fields(&schema) {
            let expected: &[&str] = if field.path == "plain" { &[] } else { &allowed };
            assert_eq!(field.allowed_values, expected, "{}", field.path);
        }
    }

    #[test]
    fn an_array_gives_its_item_fields_directly_or_through_a_reference() {
        let tag = json!({ "type": "object", "properties": { "name": { "type": "string" } } });
        let schema = json!({
            "$defs": {
                "Tag": tag,
                "Tags": { "type": "array", "items": { "$ref": "#/$defs/Tag" } }
            },
            "type": "object",
            "properties": {
                "direct": { "type": "array", "items": { "$ref": "#/$defs/Tag" } },
                "referenced": { "$ref": "#/$defs/Tags" }
            }
        });
        let fields = fields(&schema);
        for path in ["direct", "referenced"] {
            let array = fields.iter().find(|field| field.path == path).unwrap();
            assert_eq!(array.field_type, "array of object", "{path}");
            let item = format!("{path}[].name");
            assert!(
                fields.iter().any(|field| field.path == item),
                "{path}: {:?}",
                fields.iter().map(|field| &field.path).collect::<Vec<_>>()
            );
        }
    }
}
