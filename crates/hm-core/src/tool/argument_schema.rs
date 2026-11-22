//! Supported JSON Schema subset for declarative socket arguments. Reject
//! unsupported assertions during configuration so they cannot appear enforced.
use serde_json::Value;

const TYPES: &[&str] = &[
    "object", "array", "string", "integer", "number", "boolean", "null",
];
const MAX_DEPTH: usize = 12;

pub(super) fn valid_parameters(schema: &Value) -> bool {
    schema.is_object()
        && schema.get("type").is_none_or(|kind| kind == "object")
        && valid_schema(schema, 0)
}

fn valid_schema(schema: &Value, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    if schema.is_boolean() {
        return true;
    }
    let Some(rules) = schema.as_object() else {
        return false;
    };
    rules.iter().all(|(key, rule)| match key.as_str() {
        "type" => rule.as_str().is_some_and(|kind| TYPES.contains(&kind)),
        "properties" => rule.as_object().is_some_and(|properties| {
            properties
                .values()
                .all(|child| valid_schema(child, depth + 1))
        }),
        "required" => rule
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        "items" | "additionalProperties" => valid_schema(rule, depth + 1),
        "enum" => rule.as_array().is_some_and(|items| !items.is_empty()),
        "title" | "description" => rule.is_string(),
        "examples" => rule.is_array(),
        "const" | "default" => true,
        _ => false,
    })
}

pub(super) fn validate_arguments(args: &Value, schema: &Value) -> Result<(), &'static str> {
    if !args.is_object() {
        return Err("arguments must be a JSON object");
    }
    validate_value(args, schema)
}

fn validate_value(value: &Value, schema: &Value) -> Result<(), &'static str> {
    if schema == &Value::Bool(false) {
        return Err("argument rejected by the capability schema");
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let matches = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => {
                value.is_i64()
                    || value.is_u64()
                    || value.as_f64().is_some_and(|number| number.fract() == 0.0)
            }
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        if !matches {
            return Err("argument has the wrong type");
        }
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
        || schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.contains(value))
    {
        return Err("argument has a value outside the capability schema");
    }
    if let Some(object) = value.as_object() {
        if schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| {
                required
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|key| !object.contains_key(key))
            })
        {
            return Err("a required argument is missing");
        }
        for (key, child) in object {
            if let Some(rule) = schema
                .get("properties")
                .and_then(|properties| properties.get(key))
                .or_else(|| schema.get("additionalProperties"))
            {
                validate_value(child, rule)?;
            }
        }
    }
    if let (Some(items), Some(rule)) = (value.as_array(), schema.get("items")) {
        for item in items {
            validate_value(item, rule)?;
        }
    }
    Ok(())
}
