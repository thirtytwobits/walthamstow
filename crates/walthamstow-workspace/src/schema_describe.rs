//! Shared JSON-Schema walker used by every `*.describe` verb to flatten
//! a workspace document's schema into per-field guidance entries.
//!
//! Each resource registers a wrapping type with the same shape as
//! `WalkerField`. The conversion is `From`-driven so resource-specific
//! types stay clean while the walking logic is defined once here.

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
        let allowed_values = enum_values(property);
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
            if let Some(item_schema) = array_item_schema(property, defs) {
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
    if let Some(direct) = node.get("type").and_then(Value::as_str) {
        if direct == "array" {
            if let Some(item) = array_item_schema(node, defs) {
                return format!("array of {}", type_label(&item, &item, defs));
            }
            return "array".to_string();
        }
        return direct.to_string();
    }
    if let Some(resolved_type) = resolved.get("type").and_then(Value::as_str) {
        return resolved_type.to_string();
    }
    if node.get("$ref").is_some() {
        return "object".to_string();
    }
    "any".to_string()
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
