//! Lightweight JSON Schema validation for adapter input.
//!
//! Tool input schemas only use a small slice of JSON Schema: an object with
//! `required` names and `properties` whose types are string / integer /
//! number / boolean / array / object, sometimes with an `enum` constraint or
//! an `items` type for arrays. Rather than pull in a full validator we check
//! exactly that subset and return field-level errors.
//!
//! Validation runs in the invoke path before the circuit breaker and the
//! adapter, so adapters can trust required fields are present and typed.

use serde_json::{Map, Value};

/// A single field-level validation failure.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct FieldError {
    /// The field name (or `(root)` for top-level structural errors).
    pub field: String,
    /// Human-readable description of the failure.
    pub message: String,
}

impl FieldError {
    /// Construct a new field error.
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Validate `instance` against `schema`. A null/absent instance is treated as
/// an empty object so tools with no required fields accept empty input.
pub fn validate(schema: &Value, instance: &Value) -> Result<(), Vec<FieldError>> {
    // We only validate object schemas; anything else is accepted as-is.
    if schema.get("type").and_then(|v| v.as_str()) != Some("object") {
        return Ok(());
    }

    let empty = Map::new();
    let obj = match instance {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => {
            return Err(vec![FieldError::new("(root)", "args must be a JSON object")]);
        }
    };

    let mut errors = Vec::new();

    // Required fields must be present and non-null.
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for name in required.iter().filter_map(|v| v.as_str()) {
            match obj.get(name) {
                None | Some(Value::Null) => {
                    errors.push(FieldError::new(name, format!("'{name}' is required")));
                }
                _ => {}
            }
        }
    }

    // Each present property is checked against its declared type / enum.
    if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
        for (name, subschema) in props {
            match obj.get(name) {
                None | Some(Value::Null) => continue,
                Some(value) => check_property(name, subschema, value, &mut errors),
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Check one property value against its subschema, appending any errors to
/// `errors`. Handles type checking and enum constraints; performs a shallow
/// item-type check for arrays.
fn check_property(name: &str, subschema: &Value, value: &Value, errors: &mut Vec<FieldError>) {
    if let Some(expected) = subschema.get("type").and_then(|v| v.as_str()) {
        if !type_matches(expected, value) {
            errors.push(FieldError::new(
                name,
                format!("'{name}' must be of type {expected}"),
            ));
            return; // wrong type -> skip further checks for this field
        }
        // Shallow array item-type check when declared.
        if expected == "array" {
            if let (Some(arr), Some(item_ty)) = (
                value.as_array(),
                subschema.get("items").and_then(|i| i.get("type")).and_then(|t| t.as_str()),
            ) {
                for (idx, el) in arr.iter().enumerate() {
                    if !type_matches(item_ty, el) {
                        errors.push(FieldError::new(
                            format!("{name}[{idx}]"),
                            format!("array items must be of type {item_ty}"),
                        ));
                    }
                }
            }
        }
    }

    if let Some(allowed) = subschema.get("enum").and_then(|v| v.as_array()) {
        if !allowed.iter().any(|a| a == value) {
            let opts: Vec<String> = allowed
                .iter()
                .map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
                .collect();
            errors.push(FieldError::new(
                name,
                format!("'{name}' must be one of: {}", opts.join(", ")),
            ));
        }
    }
}

/// Return `true` when `value`'s JSON type matches the declared `expected` type
/// string. Unknown type strings pass through (don't reject).
fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        // JSON has no integer type; accept whole numbers only.
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true, // unknown declared type -> don't reject
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["name", "count"],
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer" },
                "mode": { "type": "string", "enum": ["a", "b"] },
                "tags": { "type": "array", "items": { "type": "string" } }
            }
        })
    }

    #[test]
    fn accepts_valid() {
        let ok = json!({ "name": "x", "count": 3, "mode": "a", "tags": ["p", "q"] });
        assert!(validate(&schema(), &ok).is_ok());
    }

    #[test]
    fn reports_missing_required() {
        let bad = json!({ "name": "x" });
        let errs = validate(&schema(), &bad).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "count"));
    }

    #[test]
    fn null_required_is_missing() {
        let bad = json!({ "name": "x", "count": null });
        let errs = validate(&schema(), &bad).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "count"));
    }

    #[test]
    fn rejects_wrong_type() {
        let bad = json!({ "name": 5, "count": 3 });
        let errs = validate(&schema(), &bad).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "name"));
    }

    #[test]
    fn integer_rejects_float() {
        let bad = json!({ "name": "x", "count": 1.5 });
        let errs = validate(&schema(), &bad).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "count"));
    }

    #[test]
    fn enforces_enum() {
        let bad = json!({ "name": "x", "count": 1, "mode": "z" });
        let errs = validate(&schema(), &bad).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "mode"));
    }

    #[test]
    fn checks_array_item_types() {
        let bad = json!({ "name": "x", "count": 1, "tags": ["ok", 7] });
        let errs = validate(&schema(), &bad).unwrap_err();
        assert!(errs.iter().any(|e| e.field == "tags[1]"));
    }

    #[test]
    fn null_instance_is_empty_object() {
        let no_required = json!({ "type": "object", "properties": { "q": { "type": "string" } } });
        assert!(validate(&no_required, &Value::Null).is_ok());
    }

    #[test]
    fn non_object_schema_passes() {
        assert!(validate(&json!({ "type": "string" }), &json!("anything")).is_ok());
    }
}
