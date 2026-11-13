//! A small JSON Schema validator for `--output-schema` (see `headless`).
//!
//! A headless run can ask for an answer of a given shape; the answer is
//! checked here before it is handed to the calling script, and the first
//! violation is fed back to the model for one retry. That needs the subset of
//! JSON Schema people actually write for "give me an object with these
//! fields", not a conforming implementation, so this stays in-tree and
//! dependency-free.
//!
//! Supported keywords: `type` (a name or a list of names), `properties`,
//! `required`, `additionalProperties` (`false` or a schema), `items`, `enum`
//! and `const`, nested to any depth, plus the boolean schemas `true`/`false`.
//! Every other keyword (`pattern`, `minimum`, `$ref`, `oneOf`, …) is ignored:
//! a value is never rejected on account of a rule this validator doesn't
//! know, so an unsupported schema is checked loosely rather than refused.

use serde_json::Value;

/// Check `value` against `schema`. The error names where the first violation
/// is (`$` is the root, `$.items[2].name` a path into it) and what was
/// expected — worded for the model that has to fix it.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "$")
}

fn check(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let schema = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{path}: no value is allowed here")),
        Value::Object(o) => o,
        // Not a schema at all; nothing to hold the value to.
        _ => return Ok(()),
    };

    if let Some(t) = schema.get("type") {
        let wanted: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !wanted.is_empty() && !wanted.iter().any(|w| is_type(value, w)) {
            return Err(format!(
                "{path}: expected {}, got {}",
                wanted.join(" or "),
                type_name(value)
            ));
        }
    }

    if let Some(c) = schema.get("const")
        && c != value
    {
        return Err(format!("{path}: must be {c}"));
    }

    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.contains(value)
    {
        let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
        return Err(format!("{path}: must be one of {}", list.join(", ")));
    }

    if let Value::Object(obj) = value {
        let props = schema.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(required)) = schema.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !obj.contains_key(name) {
                    return Err(format!("{path}: missing required property \"{name}\""));
                }
            }
        }
        for (name, v) in obj {
            let at = format!("{path}.{name}");
            match props.and_then(|p| p.get(name)) {
                Some(sub) => check(sub, v, &at)?,
                // Not a declared property: `additionalProperties` decides.
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        return Err(format!("{path}: unexpected property \"{name}\""));
                    }
                    Some(extra) => check(extra, v, &at)?,
                    None => {}
                },
            }
        }
    }

    if let (Value::Array(items), Some(sub)) = (value, schema.get("items")) {
        for (i, v) in items.iter().enumerate() {
            check(sub, v, &format!("{path}[{i}]"))?;
        }
    }

    Ok(())
}

/// Whether `value` is of the JSON Schema type `name`. An `integer` is any
/// number without a fractional part (`3.0` counts, as the spec has it).
fn is_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "array" => value.is_array(),
        "object" => value.is_object(),
        // An unknown type name can't be checked; don't reject on it.
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn primitive_types() {
        assert!(validate(&json!({"type": "string"}), &json!("x")).is_ok());
        assert!(validate(&json!({"type": "number"}), &json!(1.5)).is_ok());
        assert!(validate(&json!({"type": "integer"}), &json!(3)).is_ok());
        assert!(validate(&json!({"type": "integer"}), &json!(3.0)).is_ok());
        assert!(validate(&json!({"type": "boolean"}), &json!(true)).is_ok());
        assert!(validate(&json!({"type": "null"}), &json!(null)).is_ok());
        assert_eq!(
            validate(&json!({"type": "integer"}), &json!(1.5)).unwrap_err(),
            "$: expected integer, got number"
        );
        assert_eq!(
            validate(&json!({"type": "string"}), &json!(7)).unwrap_err(),
            "$: expected string, got number"
        );
    }

    #[test]
    fn type_lists_accept_any_member() {
        let s = json!({"type": ["string", "null"]});
        assert!(validate(&s, &json!("x")).is_ok());
        assert!(validate(&s, &json!(null)).is_ok());
        assert_eq!(
            validate(&s, &json!(1)).unwrap_err(),
            "$: expected string or null, got number"
        );
    }

    #[test]
    fn required_and_properties() {
        let s = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "age": {"type": "integer"}},
            "required": ["name"]
        });
        assert!(validate(&s, &json!({"name": "a"})).is_ok());
        assert!(validate(&s, &json!({"name": "a", "age": 3, "extra": true})).is_ok());
        assert_eq!(
            validate(&s, &json!({"age": 3})).unwrap_err(),
            "$: missing required property \"name\""
        );
        assert_eq!(
            validate(&s, &json!({"name": "a", "age": "old"})).unwrap_err(),
            "$.age: expected integer, got string"
        );
        assert_eq!(
            validate(&s, &json!([])).unwrap_err(),
            "$: expected object, got array"
        );
    }

    #[test]
    fn additional_properties() {
        let closed = json!({
            "type": "object",
            "properties": {"a": {}},
            "additionalProperties": false
        });
        assert!(validate(&closed, &json!({"a": 1})).is_ok());
        assert_eq!(
            validate(&closed, &json!({"a": 1, "b": 2})).unwrap_err(),
            "$: unexpected property \"b\""
        );
        let typed = json!({"type": "object", "additionalProperties": {"type": "number"}});
        assert!(validate(&typed, &json!({"x": 1, "y": 2})).is_ok());
        assert_eq!(
            validate(&typed, &json!({"x": "no"})).unwrap_err(),
            "$.x: expected number, got string"
        );
    }

    #[test]
    fn items_and_nesting() {
        let s = json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["files"]
        });
        assert!(validate(&s, &json!({"files": [{"path": "a"}, {"path": "b"}]})).is_ok());
        assert!(validate(&s, &json!({"files": []})).is_ok());
        assert_eq!(
            validate(&s, &json!({"files": [{"path": "a"}, {"path": 2}]})).unwrap_err(),
            "$.files[1].path: expected string, got number"
        );
        assert_eq!(
            validate(&s, &json!({"files": [{}]})).unwrap_err(),
            "$.files[0]: missing required property \"path\""
        );
    }

    #[test]
    fn enum_and_const() {
        let s = json!({"enum": ["low", "high", 3]});
        assert!(validate(&s, &json!("low")).is_ok());
        assert!(validate(&s, &json!(3)).is_ok());
        assert_eq!(
            validate(&s, &json!("mid")).unwrap_err(),
            "$: must be one of \"low\", \"high\", 3"
        );
        assert!(validate(&json!({"const": "v1"}), &json!("v1")).is_ok());
        assert_eq!(
            validate(&json!({"const": "v1"}), &json!("v2")).unwrap_err(),
            "$: must be \"v1\""
        );
    }

    #[test]
    fn boolean_schemas_and_unknown_keywords() {
        assert!(validate(&json!(true), &json!(1)).is_ok());
        assert!(validate(&json!(false), &json!(1)).is_err());
        // Keywords outside the subset never reject.
        let s = json!({"type": "string", "pattern": "^a", "minLength": 99});
        assert!(validate(&s, &json!("b")).is_ok());
    }
}
