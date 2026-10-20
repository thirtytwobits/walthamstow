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
    let mut defs = serde_json::Map::new();
    for section in ["$defs", "definitions"] {
        for (name, definition) in schema
            .get(section)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            defs.insert(format!("#/{section}/{name}"), definition.clone());
        }
    }
    walk_object_schema(schema, "", &defs, &mut emit, &mut Vec::new());
}

/// Walks `node`'s properties. `active` names the references expanded on the way here; one
/// of them is not expanded again, so a schema that refers to itself is walked once.
fn walk_object_schema<F: FnMut(WalkerField)>(
    node: &Value,
    path: &str,
    defs: &serde_json::Map<String, Value>,
    emit: &mut F,
    active: &mut Vec<String>,
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
        let field_type = type_label(property, &resolved, defs, &mut Vec::new());
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
            if let Some(items) = array.get("items") {
                let item_schema = resolve_ref(items, defs);
                if matches_object(&item_schema) {
                    let array_path = format!("{field_path}[]");
                    within(active, [ref_name(property), ref_name(items)], |active| {
                        walk_object_schema(&item_schema, &array_path, defs, emit, active)
                    });
                }
            }
        } else if matches_object(&resolved) {
            within(active, [ref_name(property)], |active| {
                walk_object_schema(&resolved, &field_path, defs, emit, active)
            });
        }
    }
}

/// Runs `walk` with `references` added to `active`, unless one of them is already there.
fn within<'a>(
    active: &mut Vec<String>,
    references: impl IntoIterator<Item = Option<&'a str>>,
    walk: impl FnOnce(&mut Vec<String>),
) {
    let references = references.into_iter().flatten().collect::<Vec<_>>();
    if references
        .iter()
        .any(|reference| active.iter().any(|held| held == reference))
    {
        return;
    }
    let depth = active.len();
    active.extend(references.iter().map(|reference| reference.to_string()));
    walk(active);
    active.truncate(depth);
}

/// The reference `node` makes, when it makes one: the key its definition is held under.
fn ref_name(node: &Value) -> Option<&str> {
    node.get("$ref")?.as_str()
}

/// The definition `node` refers to, through any chain of references, or `node` when it
/// refers to none. A chain that returns to a definition it passed stops there.
fn resolve_ref(node: &Value, defs: &serde_json::Map<String, Value>) -> Value {
    let mut resolved = node.clone();
    let mut passed = Vec::new();
    while let Some(name) = ref_name(&resolved).map(str::to_string) {
        let Some(definition) = defs.get(&name) else {
            break;
        };
        if passed.contains(&name) {
            break;
        }
        passed.push(name);
        resolved = definition.clone();
    }
    resolved
}

/// `node`'s type, naming an array's item type. `active` names the item references being
/// labelled; one met again is labelled as a bare array.
fn type_label(
    node: &Value,
    resolved: &Value,
    defs: &serde_json::Map<String, Value>,
    active: &mut Vec<String>,
) -> String {
    let typed = if node.get("type").is_some() {
        node
    } else {
        resolved
    };
    match typed.get("type").and_then(Value::as_str) {
        Some("array") => {
            let Some(items) = typed.get("items") else {
                return "array".to_string();
            };
            let item = resolve_ref(items, defs);
            let mut label = "array".to_string();
            within(active, [ref_name(items)], |active| {
                label = format!("array of {}", type_label(&item, &item, defs, active));
            });
            label
        }
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

    #[test]
    fn a_schema_that_refers_to_itself_is_walked_once() {
        let schema = json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "child": { "$ref": "#/$defs/Node" },
                        "children": { "type": "array", "items": { "$ref": "#/$defs/Node" } }
                    }
                },
                "Nested": { "type": "array", "items": { "$ref": "#/$defs/Nested" } }
            },
            "type": "object",
            "properties": {
                "root": { "$ref": "#/$defs/Node" },
                "nested": { "$ref": "#/$defs/Nested" }
            }
        });
        let fields = fields(&schema);
        let paths = fields
            .iter()
            .map(|field| field.path.as_str())
            .collect::<Vec<_>>();
        for walked in ["root", "root.name", "root.child", "root.children", "nested"] {
            assert!(paths.contains(&walked), "{walked}: {paths:?}");
        }
        assert!(
            paths.iter().all(|path| !path.starts_with("root.child.")),
            "{paths:?}"
        );
    }

    #[test]
    fn a_reference_to_an_alias_reaches_the_definition_behind_it() {
        let schema = json!({
            "$defs": {
                "Alias": { "$ref": "#/$defs/Concrete" },
                "Concrete": { "type": "object", "properties": { "inner": { "type": "string" } } },
                "Loop": { "$ref": "#/$defs/Back" },
                "Back": { "$ref": "#/$defs/Loop" }
            },
            "type": "object",
            "properties": {
                "aliased": { "$ref": "#/$defs/Alias" },
                "looped": { "$ref": "#/$defs/Loop" }
            }
        });
        let fields = fields(&schema);
        let paths = fields
            .iter()
            .map(|field| field.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"aliased.inner"), "{paths:?}");
        assert!(paths.contains(&"looped"), "{paths:?}");
    }

    #[test]
    fn a_reference_into_legacy_definitions_is_walked() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": {
                "Address": {
                    "type": "object",
                    "properties": {
                        "street": { "type": "string", "description": "Where it is." }
                    }
                }
            },
            "type": "object",
            "properties": { "home": { "$ref": "#/definitions/Address" } }
        });
        let fields = fields(&schema);
        let street = fields.iter().find(|field| field.path == "home.street");
        assert!(street.is_some_and(|field| !field.description.is_empty()));
    }
}
