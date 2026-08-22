//! Generated OpenAPI for the public command surface.
//!
//! The document is honest: one concrete path per public command, its request
//! body derived from the declared input schema (so the field types are real, not
//! a generic blob), and internal commands are absent because they are not routed.
//! The field-type mapping mirrors the wire forms kiln validates: `money` is a
//! decimal string (not an integer, despite its storage type), `one_of` is a
//! string enum, and an `optional` field is simply omitted from `required`.

use serde_json::{Map, Value, json};

use crate::starlark_builtins::{FieldKind, InputSchema};

/// Build the OpenAPI 3.1 document for the given public commands.
pub fn build(commands: &[(&str, &InputSchema)]) -> Value {
    let mut paths = Map::new();
    for (name, schema) in commands {
        paths.insert(format!("/commands/{name}"), command_path(name, schema));
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "kiln",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "generated command API",
        },
        "paths": Value::Object(paths),
    })
}

fn command_path(name: &str, schema: &InputSchema) -> Value {
    json!({
        "post": {
            "operationId": format!("execute_{}", name.replace('-', "_")),
            "summary": format!("execute the `{name}` command"),
            "requestBody": {
                "required": true,
                "content": {
                    "application/json": { "schema": input_schema(schema) },
                },
            },
            "responses": responses(),
        }
    })
}

fn input_schema(schema: &InputSchema) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (field, kind) in &schema.fields {
        properties.insert(field.clone(), field_schema(kind));
        if !kind.is_nullable() {
            required.push(Value::String(field.clone()));
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

/// One field's JSON Schema. `optional` reaches through to the inner kind; its
/// absence from `required` is what encodes optionality.
fn field_schema(kind: &FieldKind) -> Value {
    match kind.base() {
        FieldKind::Text { max_length } => {
            let mut schema = json!({ "type": "string" });
            if let Some(max) = max_length {
                schema["maxLength"] = json!(max);
            }
            schema
        }
        FieldKind::Uuid => json!({ "type": "string", "format": "uuid" }),
        FieldKind::Timestamp => json!({ "type": "string", "format": "date-time" }),
        FieldKind::Money => json!({ "type": "string", "description": "decimal amount" }),
        FieldKind::OneOf(variants) => json!({ "type": "string", "enum": variants }),
        FieldKind::I64 => json!({ "type": "integer", "format": "int64" }),
        // No standard format spans unsigned 64-bit. A numeric `maximum: 2^64-1` would
        // be silently wrong for the many JSON tools that parse bounds as f64 (it does
        // not round-trip past 2^53), so state the floor and describe the ceiling in
        // words rather than declare a bound consumers would corrupt.
        FieldKind::U64 => json!({
            "type": "integer",
            "minimum": 0,
            "description": "unsigned 64-bit integer (0 to 2^64-1)",
        }),
        FieldKind::Bool => json!({ "type": "boolean" }),
        FieldKind::Json => json!({}),
        FieldKind::Optional(_) => unreachable!("base() strips Optional"),
    }
}

fn responses() -> Value {
    json!({
        "200": { "description": "committed; the body carries the appended positions and emitted events" },
        "400": { "description": "the input was malformed" },
        "409": { "description": "the consistency boundary kept changing; retry" },
        "422": { "description": "the command rejected the request on state grounds" },
        "500": { "description": "internal error" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> InputSchema {
        InputSchema {
            fields: vec![
                ("id".to_owned(), FieldKind::Uuid),
                ("amount".to_owned(), FieldKind::Money),
                (
                    "kind".to_owned(),
                    FieldKind::OneOf(vec!["a".to_owned(), "b".to_owned()]),
                ),
                (
                    "note".to_owned(),
                    FieldKind::Optional(Box::new(FieldKind::Text {
                        max_length: Some(10),
                    })),
                ),
            ],
        }
    }

    #[test]
    fn maps_field_kinds_and_marks_optionals() {
        let binding = schema();
        let doc = build(&[("do-thing", &binding)]);
        let props = &doc["paths"]["/commands/do-thing"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["properties"];
        assert_eq!(props["amount"]["type"], "string");
        assert!(
            props["amount"].get("format").is_none(),
            "money is a bare string"
        );
        assert_eq!(props["id"]["format"], "uuid");
        assert_eq!(props["kind"]["enum"], json!(["a", "b"]));
        assert_eq!(props["note"]["maxLength"], json!(10));

        let required = doc["paths"]["/commands/do-thing"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"]["required"]
            .as_array()
            .unwrap()
            .clone();
        assert!(required.contains(&json!("id")));
        assert!(
            !required.contains(&json!("note")),
            "optional is not required"
        );
    }
}
