use std::collections::BTreeSet;

use serde_json::Value;

const SUPPORTED_KEYWORDS: &[&str] = &[
    "type",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "enum",
    "const",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaError {
    pub path: String,
    pub message: String,
}

impl SchemaError {
    fn at(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Validates the deliberately small JSON Schema subset enforced by the runtime.
/// Keywords outside this subset are rejected before a session can be created.
pub(crate) fn validate_schema_definition(schema: &Value, path: &str) -> Result<(), SchemaError> {
    let Some(object) = schema.as_object() else {
        return if schema.is_boolean() {
            Ok(())
        } else {
            Err(SchemaError::at(
                path,
                "output schema must be a JSON Schema boolean or object",
            ))
        };
    };

    for keyword in object.keys() {
        if !SUPPORTED_KEYWORDS.contains(&keyword.as_str()) {
            return Err(SchemaError::at(
                format!("{path}.{keyword}"),
                format!("unsupported output schema keyword `{keyword}`"),
            ));
        }
    }

    if let Some(value) = object.get("type") {
        let supported = matches!(
            value.as_str(),
            Some("object" | "array" | "string" | "number" | "integer" | "boolean" | "null")
        );
        if !supported {
            return Err(SchemaError::at(
                format!("{path}.type"),
                "type must be one of object, array, string, number, integer, boolean, or null",
            ));
        }
    }

    if let Some(value) = object.get("required") {
        let Some(required) = value.as_array() else {
            return Err(SchemaError::at(
                format!("{path}.required"),
                "required must be an array of unique strings",
            ));
        };
        let mut names = BTreeSet::new();
        for (index, name) in required.iter().enumerate() {
            let Some(name) = name.as_str() else {
                return Err(SchemaError::at(
                    format!("{path}.required[{index}]"),
                    "required entries must be strings",
                ));
            };
            if !names.insert(name) {
                return Err(SchemaError::at(
                    format!("{path}.required[{index}]"),
                    "required entries must be unique",
                ));
            }
        }
    }

    if let Some(value) = object.get("properties") {
        let Some(properties) = value.as_object() else {
            return Err(SchemaError::at(
                format!("{path}.properties"),
                "properties must be an object of schemas",
            ));
        };
        for (name, property_schema) in properties {
            validate_schema_definition(
                property_schema,
                &format!("{path}.properties{}", property_path(name)),
            )?;
        }
    }

    if object
        .get("additionalProperties")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(SchemaError::at(
            format!("{path}.additionalProperties"),
            "additionalProperties must be a boolean",
        ));
    }

    if let Some(items) = object.get("items") {
        validate_schema_definition(items, &format!("{path}.items"))?;
    }

    if object
        .get("enum")
        .is_some_and(|value| value.as_array().is_none_or(Vec::is_empty))
    {
        return Err(SchemaError::at(
            format!("{path}.enum"),
            "enum must be a non-empty array",
        ));
    }

    Ok(())
}

pub(crate) fn validate_instance(schema: &Value, instance: &Value) -> Result<(), SchemaError> {
    validate_instance_at(schema, instance, "$")
}

fn validate_instance_at(schema: &Value, instance: &Value, path: &str) -> Result<(), SchemaError> {
    if let Some(allowed) = schema.as_bool() {
        return if allowed {
            Ok(())
        } else {
            Err(SchemaError::at(path, "the schema rejects every value"))
        };
    }
    let object = schema
        .as_object()
        .expect("validated output schemas are objects or booleans");

    if let Some(expected) = object.get("type").and_then(Value::as_str) {
        let matches = match expected {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "number" => instance.is_number(),
            "integer" => is_integer(instance),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => unreachable!("schema type is validated at session creation"),
        };
        if !matches {
            return Err(SchemaError::at(
                path,
                format!("expected {expected}, found {}", value_type(instance)),
            ));
        }
    }

    if let Some(expected) = object.get("const") {
        if instance != expected {
            return Err(SchemaError::at(path, "value does not match const"));
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if !values.contains(instance) {
            return Err(SchemaError::at(
                path,
                "value is not one of the enum choices",
            ));
        }
    }

    if let Some(instance_object) = instance.as_object() {
        let properties = object.get("properties").and_then(Value::as_object);
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !instance_object.contains_key(name) {
                    return Err(SchemaError::at(
                        format!("{path}{}", property_path(name)),
                        "required property is missing",
                    ));
                }
            }
        }
        if let Some(properties) = properties {
            for (name, property_schema) in properties {
                if let Some(value) = instance_object.get(name) {
                    validate_instance_at(
                        property_schema,
                        value,
                        &format!("{path}{}", property_path(name)),
                    )?;
                }
            }
        }
        if object.get("additionalProperties") == Some(&Value::Bool(false)) {
            for name in instance_object.keys() {
                if properties.is_none_or(|properties| !properties.contains_key(name)) {
                    return Err(SchemaError::at(
                        format!("{path}{}", property_path(name)),
                        "additional property is not allowed",
                    ));
                }
            }
        }
    }

    if let (Some(items), Some(values)) = (object.get("items"), instance.as_array()) {
        for (index, value) in values.iter().enumerate() {
            validate_instance_at(items, value, &format!("{path}[{index}]"))?;
        }
    }

    Ok(())
}

fn is_integer(value: &Value) -> bool {
    value.as_i64().is_some()
        || value.as_u64().is_some()
        || value.as_f64().is_some_and(|number| number.fract() == 0.0)
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn property_path(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
        && name
            .chars()
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
    {
        format!(".{name}")
    } else {
        format!(
            "[{}]",
            serde_json::to_string(name).expect("property names serialize")
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{validate_instance, validate_schema_definition};

    #[test]
    fn structural_subset_reports_nested_paths() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "count": { "type": "integer" } },
                        "required": ["count"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["items"]
        });
        validate_schema_definition(&schema, "schema").expect("supported schema");
        let error = validate_instance(&schema, &json!({ "items": [{}] })).expect_err("invalid");
        assert_eq!(error.path, "$.items[0].count");
    }
}
