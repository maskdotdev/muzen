use serde::Deserialize;
use serde_json::Value;

use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::review_contract::{ToolCounts, ToolName};

use super::registry::ToolRegistry;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileRangeArgs {
    path: String,
    start_line: usize,
    end_line: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchTextArgs {
    query: String,
}

pub(crate) fn validate_invocation(
    session_id: SessionId,
    turn_id: TurnId,
    call: crate::reviewer_kernel::kernel_types::ModelToolCall,
    capabilities: CapabilitySet,
    scope_key: ScopeKey,
    registry: &ToolRegistry,
) -> Result<ToolInvocation, (ToolCallId, ToolId, ToolErrorCode)> {
    let tool_id = call.name;
    let builtin_name = tool_id.as_builtin();
    let input_bytes = call.raw_arguments.len();
    if input_bytes > capabilities.tool_input.max_argument_bytes {
        return Err((call.call_id, tool_id, ToolErrorCode::TooLarge));
    }
    let Some(definition) = registry.definition(&tool_id) else {
        return Err((call.call_id, tool_id, ToolErrorCode::UnknownTool));
    };
    if definition.builtin != builtin_name {
        return Err((call.call_id, tool_id, ToolErrorCode::UnknownTool));
    }
    if !capabilities.allow_tool(&tool_id) {
        return Err((call.call_id, tool_id, ToolErrorCode::ToolNotAllowed));
    }
    let args = match builtin_name {
        Some(ToolName::ListChangedFiles | ToolName::ReadDiff | ToolName::ListFiles) => {
            ToolArgs::Empty
        }
        Some(
            ToolName::ReadFile
            | ToolName::ReadBaseFile
            | ToolName::ReadHeadFile
            | ToolName::FindRelatedFiles
            | ToolName::FindTestsForFile
            | ToolName::ListImports,
        ) => {
            let parsed: ReadFileArgs = serde_json::from_str(&call.raw_arguments).map_err(|_| {
                (
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::InvalidArgs,
                )
            })?;
            let path = RepoPath::parse(&parsed.path).map_err(|_| {
                (
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::PathDenied,
                )
            })?;
            ToolArgs::ReadFile { path }
        }
        Some(ToolName::ReadFileRange) => {
            let parsed: ReadFileRangeArgs =
                serde_json::from_str(&call.raw_arguments).map_err(|_| {
                    (
                        call.call_id.clone(),
                        tool_id.clone(),
                        ToolErrorCode::InvalidArgs,
                    )
                })?;
            let path = RepoPath::parse(&parsed.path).map_err(|_| {
                (
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::PathDenied,
                )
            })?;
            ToolArgs::ReadFileRange {
                path,
                start_line: parsed.start_line.max(1),
                end_line: parsed.end_line.max(parsed.start_line).max(1),
            }
        }
        Some(ToolName::SearchText) => {
            let parsed: SearchTextArgs =
                serde_json::from_str(&call.raw_arguments).map_err(|_| {
                    (
                        call.call_id.clone(),
                        tool_id.clone(),
                        ToolErrorCode::InvalidArgs,
                    )
                })?;
            ToolArgs::SearchText {
                query: parsed.query,
            }
        }
        None => {
            let parsed: Value = serde_json::from_str(&call.raw_arguments).map_err(|_| {
                (
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::InvalidArgs,
                )
            })?;
            if !schema_accepts_value(&parsed, &definition.parameters) {
                return Err((call.call_id, tool_id, ToolErrorCode::InvalidArgs));
            }
            ToolArgs::Raw(parsed)
        }
    };
    Ok(ToolInvocation {
        session_id,
        turn_id,
        call_id: call.call_id,
        tool_id,
        builtin_name,
        input_bytes,
        args,
        capabilities,
        scope_key,
        assigned_changed_files: Vec::new(),
    })
}

pub(crate) fn count_tool_result(counts: &mut ToolCounts, result: &ToolResultEnvelope) {
    if result.ok {
        if let Some(tool_name) = result.tool_name.as_builtin() {
            counts.increment(tool_name);
        }
    }
}

fn schema_accepts_value(value: &Value, schema: &Value) -> bool {
    match schema {
        Value::Bool(accepts) => *accepts,
        Value::Object(_) => {
            if let Some(values) = schema.get("enum").and_then(Value::as_array) {
                if !values.iter().any(|candidate| candidate == value) {
                    return false;
                }
            }
            match schema.get("type") {
                Some(Value::String(kind)) => schema_type_accepts(value, schema, kind),
                Some(Value::Array(kinds)) => kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|kind| schema_type_accepts(value, schema, kind)),
                Some(_) => false,
                None => true,
            }
        }
        _ => true,
    }
}

fn schema_type_accepts(value: &Value, schema: &Value, kind: &str) -> bool {
    match kind {
        "object" => object_schema_accepts(value, schema),
        "array" => array_schema_accepts(value, schema),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn object_schema_accepts(value: &Value, schema: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        if !required
            .iter()
            .filter_map(Value::as_str)
            .all(|name| object.contains_key(name))
        {
            return false;
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        let Some(properties) = properties else {
            return object.is_empty();
        };
        if object.keys().any(|name| !properties.contains_key(name)) {
            return false;
        }
    }
    properties.is_none_or(|properties| {
        properties.iter().all(|(name, property_schema)| {
            object
                .get(name)
                .is_none_or(|property| schema_accepts_value(property, property_schema))
        })
    })
}

fn array_schema_accepts(value: &Value, schema: &Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    schema.get("items").is_none_or(|item_schema| {
        items
            .iter()
            .all(|item| schema_accepts_value(item, item_schema))
    })
}
