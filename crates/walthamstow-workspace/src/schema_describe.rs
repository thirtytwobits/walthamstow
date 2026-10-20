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
    let (document, passed) = resolve_ref(schema, schema);
    within(&mut Vec::new(), &passed, |active| {
        walk_object_schema(&document, "", schema, &mut emit, active)
    });
}

/// Walks `node`'s properties. `active` names the references expanded on the way here; one
/// of them is not expanded again, so a schema that refers to itself is walked once.
fn walk_object_schema<F: FnMut(WalkerField)>(
    node: &Value,
    path: &str,
    root: &Value,
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
        let (resolved, passed) = resolve_ref(property, root);
        let field_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}.{name}")
        };
        let field_type = type_label(property, &resolved, root, &mut Vec::new());
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

        let typed = if property.get("type").is_some() {
            property
        } else {
            &resolved
        };
        if declared_types(typed).contains(&"array") {
            let array = if property.get("items").is_some() {
                property
            } else {
                &resolved
            };
            if let Some(items) = array.get("items") {
                let (item_schema, item_passed) = resolve_ref(items, root);
                if matches_object(&item_schema) {
                    let array_path = format!("{field_path}[]");
                    let references = [passed.as_slice(), &item_passed].concat();
                    within(active, &references, |active| {
                        walk_object_schema(&item_schema, &array_path, root, emit, active)
                    });
                }
            }
        } else if matches_object(&resolved) {
            within(active, &passed, |active| {
                walk_object_schema(&resolved, &field_path, root, emit, active)
            });
        }
    }
}

/// Runs `walk` with `references` added to `active`, unless one of them is already there.
fn within(active: &mut Vec<String>, references: &[String], walk: impl FnOnce(&mut Vec<String>)) {
    if references
        .iter()
        .any(|reference| active.contains(reference))
    {
        return;
    }
    let depth = active.len();
    active.extend_from_slice(references);
    walk(active);
    active.truncate(depth);
}

/// `fragment` with its percent-encoded bytes decoded, as a URI fragment is read before its
/// JSON Pointer; `None` when it does not decode to UTF-8.
fn percent_decoded(fragment: &str) -> Option<String> {
    let bytes = fragment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .filter(|_| bytes[index] == b'%')
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match hex {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

/// `target` with the `properties` and `required` written beside the reference in `node`, which
/// apply alongside it.
fn with_siblings(node: &Value, target: Value) -> Value {
    let mut merged = target;
    let Some(target) = merged.as_object_mut() else {
        return merged;
    };
    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        let held = target
            .entry("properties")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(held) = held.as_object_mut() {
            held.extend(properties.clone());
        }
    }
    if let Some(required) = node.get("required").and_then(Value::as_array) {
        let held = target
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(held) = held.as_array_mut() {
            held.extend(
                required
                    .iter()
                    .filter(|name| !held.contains(name))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
    }
    merged
}

/// The reference `node` makes, when it makes one.
fn ref_name(node: &Value) -> Option<&str> {
    node.get("$ref")?.as_str()
}

/// The schema `node` refers to, through any chain of local references resolved as JSON Pointers
/// into `root`, or `node` when it refers to none, with each reference passed on the way. The
/// `properties` and `required` beside each reference are kept. A chain that returns to a
/// reference it passed stops there.
fn resolve_ref(node: &Value, root: &Value) -> (Value, Vec<String>) {
    let mut resolved = node.clone();
    let mut passed = Vec::new();
    while let Some(reference) = ref_name(&resolved).map(str::to_string) {
        let Some(target) = reference
            .strip_prefix('#')
            .and_then(percent_decoded)
            .and_then(|pointer| root.pointer(&pointer))
        else {
            break;
        };
        if passed.contains(&reference) {
            break;
        }
        passed.push(reference);
        resolved = with_siblings(&resolved, target.clone());
    }
    (resolved, passed)
}

/// `node`'s type, naming an array's item type. `active` names the item references being
/// labelled; one met again is labelled as a bare array.
fn type_label(node: &Value, resolved: &Value, root: &Value, active: &mut Vec<String>) -> String {
    let typed = if node.get("type").is_some() {
        node
    } else {
        resolved
    };
    let declared = declared_types(typed);
    if declared.is_empty() {
        return untyped_label(resolved);
    }
    let mut labels = Vec::with_capacity(declared.len());
    for name in declared {
        let items = typed.get("items").filter(|_| name == "array");
        let Some(items) = items else {
            labels.push(name.to_string());
            continue;
        };
        let (item, passed) = resolve_ref(items, root);
        let mut label = "array".to_string();
        within(active, &passed, |active| {
            label = format!("array of {}", type_label(&item, &item, root, active));
        });
        labels.push(label);
    }
    labels.join(" or ")
}

/// The type an untyped schema's structure shows: `object` for one with properties, the types of
/// its `enum` or `const` values, or `any`.
fn untyped_label(schema: &Value) -> String {
    if schema.get("properties").is_some() {
        return "object".to_string();
    }
    let values = match (
        schema.get("enum").and_then(Value::as_array),
        schema.get("const"),
    ) {
        (Some(values), _) => values.iter().collect(),
        (None, Some(value)) => vec![value],
        (None, None) => Vec::new(),
    };
    let mut names = Vec::new();
    for value in values {
        let name = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        "any".to_string()
    } else {
        names.join(" or ")
    }
}

/// The types `node` declares: its `type` name, or each name in its `type` list.
fn declared_types(node: &Value) -> Vec<&str> {
    match node.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
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
    declared_types(node).contains(&"object") || node.get("properties").is_some()
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
    fn a_reference_into_draft_7_definitions_is_walked() {
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

    #[test]
    fn a_definition_named_with_pointer_characters_is_reached_by_its_escaped_reference() {
        let object = |field: &str| json!({ "type": "object", "properties": { field: { "type": "string" } } });
        let schema = json!({
            "$defs": { "a/b": object("slashed"), "c~d": object("tilded") },
            "type": "object",
            "properties": {
                "first": { "$ref": "#/$defs/a~1b" },
                "second": { "$ref": "#/$defs/c~0d" }
            }
        });
        let fields = fields(&schema);
        let paths = fields
            .iter()
            .map(|field| field.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"first.slashed"), "{paths:?}");
        assert!(paths.contains(&"second.tilded"), "{paths:?}");
    }

    #[test]
    fn a_reference_into_a_nested_schema_is_walked() {
        let schema = json!({
            "$defs": {
                "Wrapper": {
                    "type": "object",
                    "properties": {
                        "inner": { "type": "object", "properties": { "deep": { "type": "string" } } }
                    }
                }
            },
            "type": "object",
            "properties": {
                "common": { "type": "object", "properties": { "shared": { "type": "string" } } },
                "nested": { "$ref": "#/$defs/Wrapper/properties/inner" },
                "again": { "$ref": "#/properties/common" }
            }
        });
        let fields = fields(&schema);
        let paths = fields
            .iter()
            .map(|field| field.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"nested.deep"), "{paths:?}");
        assert!(paths.contains(&"again.shared"), "{paths:?}");
    }

    #[test]
    fn a_root_that_is_a_reference_is_walked_as_its_definition() {
        let schema = json!({
            "$ref": "#/$defs/Document",
            "$defs": {
                "Document": { "type": "object", "properties": { "title": { "type": "string" } } }
            }
        });
        let fields = fields(&schema);
        assert!(fields.iter().any(|field| field.path == "title"));
    }

    #[test]
    fn a_percent_encoded_reference_reaches_its_definition() {
        let schema = json!({
            "$defs": { "a b": { "type": "object", "properties": { "inner": { "type": "string" } } } },
            "type": "object",
            "properties": { "spaced": { "$ref": "#/$defs/a%20b" } }
        });
        let fields = fields(&schema);
        assert!(fields.iter().any(|field| field.path == "spaced.inner"));
    }

    #[test]
    fn properties_beside_a_reference_are_walked_with_its_target() {
        let schema = json!({
            "$defs": { "Base": { "type": "object", "properties": { "name": { "type": "string" } } } },
            "type": "object",
            "properties": {
                "item": {
                    "$ref": "#/$defs/Base",
                    "properties": { "extra": { "type": "string" } },
                    "required": ["extra"]
                }
            }
        });
        let fields = fields(&schema);
        assert!(fields.iter().any(|field| field.path == "item.name"));
        let extra = fields.iter().find(|field| field.path == "item.extra");
        assert!(extra.is_some_and(|field| field.required));
    }

    #[test]
    fn properties_beside_an_item_reference_are_walked_with_its_target() {
        let schema = json!({
            "$defs": { "Base": { "type": "object", "properties": { "name": { "type": "string" } } } },
            "type": "object",
            "properties": {
                "list": {
                    "type": "array",
                    "items": {
                        "$ref": "#/$defs/Base",
                        "properties": { "extra": { "type": "string" } },
                        "required": ["extra"]
                    }
                }
            }
        });
        let fields = fields(&schema);
        assert!(fields.iter().any(|field| field.path == "list[].name"));
        let extra = fields.iter().find(|field| field.path == "list[].extra");
        assert!(extra.is_some_and(|field| field.required));
    }

    #[test]
    fn a_root_reference_to_a_recursive_definition_is_walked_once() {
        let node = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "child": { "$ref": "#/$defs/Node" }
            }
        });
        let at_root = json!({ "$defs": { "Node": node }, "$ref": "#/$defs/Node" });
        let as_property = json!({
            "$defs": { "Node": node },
            "type": "object",
            "properties": { "child": { "$ref": "#/$defs/Node" } }
        });
        let paths = |schema: &Value| {
            fields(schema)
                .into_iter()
                .map(|field| field.path)
                .collect::<Vec<_>>()
        };
        let nested = paths(&as_property)
            .into_iter()
            .filter_map(|path| path.strip_prefix("child.").map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(paths(&at_root), nested);
    }

    #[test]
    fn properties_beside_every_reference_in_a_chain_are_walked() {
        let schema = json!({
            "$defs": {
                "Base": { "type": "object", "properties": { "name": { "type": "string" } } },
                "Alias": {
                    "$ref": "#/$defs/Base",
                    "properties": { "alias": { "type": "string" } },
                    "required": ["alias"]
                }
            },
            "type": "object",
            "properties": { "item": { "$ref": "#/$defs/Alias" } }
        });
        let fields = fields(&schema);
        assert!(fields.iter().any(|field| field.path == "item.name"));
        let alias = fields.iter().find(|field| field.path == "item.alias");
        assert!(alias.is_some_and(|field| field.required));
    }

    #[test]
    fn a_type_list_is_walked_and_labelled_by_each_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "list": {
                    "type": ["array", "null"],
                    "items": { "type": ["object", "null"], "properties": { "name": { "type": "string" } } }
                },
                "label": { "type": ["string", "null"] }
            }
        });
        let fields = fields(&schema);
        assert!(fields.iter().any(|field| field.path == "list[].name"));
        let list = fields.iter().find(|field| field.path == "list").unwrap();
        assert!(list.field_type.contains("array of") && list.field_type.contains("null"));
        let label = fields.iter().find(|field| field.path == "label").unwrap();
        assert!(label.field_type.contains("string") && label.field_type.contains("null"));
    }

    #[test]
    fn a_recursive_definition_reached_through_an_alias_is_walked_once() {
        let schema = json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "child": { "$ref": "#/$defs/Node" }
                    }
                },
                "Alias": { "$ref": "#/$defs/Node" }
            },
            "type": "object",
            "properties": {
                "direct": { "$ref": "#/$defs/Node" },
                "aliased": { "$ref": "#/$defs/Alias" }
            }
        });
        let fields = fields(&schema);
        let under = |prefix: &str| {
            fields
                .iter()
                .filter_map(|field| field.path.strip_prefix(prefix).map(str::to_string))
                .collect::<Vec<_>>()
        };
        assert_eq!(under("aliased."), under("direct."));
    }

    #[test]
    fn an_untyped_reference_is_labelled_by_what_its_target_holds() {
        let schema = json!({
            "$defs": {
                "Status": { "enum": ["draft", "final"] },
                "Pair": { "properties": { "left": { "type": "string" } } },
                "Anything": { "description": "no constraint" }
            },
            "type": "object",
            "properties": {
                "status": { "$ref": "#/$defs/Status" },
                "pair": { "$ref": "#/$defs/Pair" },
                "anything": { "$ref": "#/$defs/Anything" }
            }
        });
        let fields = fields(&schema);
        let label = |path: &str| {
            fields
                .iter()
                .find(|field| field.path == path)
                .map(|field| field.field_type.clone())
                .unwrap()
        };
        assert!(label("status").contains("string") && !label("status").contains("object"));
        assert!(label("pair").contains("object"));
        assert!(!label("anything").contains("object"));
    }
}
