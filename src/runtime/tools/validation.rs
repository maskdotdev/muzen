use serde::Deserialize;
use serde_json::Value;

use crate::contracts::{ToolCounts, ToolName};
use crate::runtime::contracts::*;

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordFindingArgs {
    title: String,
    claim: String,
    path: String,
    start_line: usize,
    end_line: usize,
}

#[derive(Debug)]
struct RecordFileReviewArgs {
    path: String,
    verdict: String,
    summary: String,
    finding_id: Option<String>,
    related_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChallengeFindingArgs {
    finding_id: String,
    rationale: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishArgs {
    reason: Option<String>,
}

pub(crate) fn validate_invocation(
    session_id: SessionId,
    turn_id: TurnId,
    call: crate::runtime::contracts::ModelToolCall,
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
        Some(ToolName::RecordFinding) => {
            let parsed: RecordFindingArgs =
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
            ToolArgs::RecordFinding {
                title: parsed.title,
                claim: parsed.claim,
                path,
                start_line: Some(parsed.start_line.max(1)),
                end_line: Some(parsed.end_line.max(parsed.start_line).max(1)),
            }
        }
        Some(ToolName::RecordFileReview) => {
            let parsed = parse_record_file_review_args(&call.raw_arguments).ok_or_else(|| {
                (
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::InvalidArgs,
                )
            })?;
            let verdict = parsed.verdict.trim().to_ascii_lowercase();
            if !matches!(verdict.as_str(), "clean" | "issue_found" | "skipped") {
                return Err((
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::InvalidArgs,
                ));
            }
            if parsed.summary.trim().is_empty() {
                return Err((
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::InvalidArgs,
                ));
            }
            let path = RepoPath::parse(&parsed.path).map_err(|_| {
                (
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::PathDenied,
                )
            })?;
            let related_paths = parsed
                .related_paths
                .into_iter()
                .map(|path| RepoPath::parse(&path))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    (
                        call.call_id.clone(),
                        tool_id.clone(),
                        ToolErrorCode::PathDenied,
                    )
                })?;
            ToolArgs::RecordFileReview {
                path,
                verdict,
                summary: parsed.summary,
                finding_id: parsed
                    .finding_id
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                related_paths,
            }
        }
        Some(ToolName::ChallengeFinding) => {
            let parsed: ChallengeFindingArgs =
                serde_json::from_str(&call.raw_arguments).map_err(|_| {
                    (
                        call.call_id.clone(),
                        tool_id.clone(),
                        ToolErrorCode::InvalidArgs,
                    )
                })?;
            ToolArgs::ChallengeFinding {
                finding_id: parsed.finding_id,
                rationale: parsed.rationale,
            }
        }
        Some(ToolName::Finish) => {
            let parsed: FinishArgs = serde_json::from_str(&call.raw_arguments).map_err(|_| {
                (
                    call.call_id.clone(),
                    tool_id.clone(),
                    ToolErrorCode::InvalidArgs,
                )
            })?;
            ToolArgs::Finish {
                reason: parsed.reason.unwrap_or_else(|| "finished".to_string()),
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

fn parse_record_file_review_args(raw: &str) -> Option<RecordFileReviewArgs> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let object = value.as_object()?;
    let path = object.get("path")?.as_str()?.to_string();
    let verdict = object.get("verdict")?.as_str()?.to_string();
    let summary = object.get("summary")?.as_str()?.to_string();
    let finding_id = object
        .get("finding_id")
        .or_else(|| object.get("findingId"))
        .and_then(|value| match value {
            Value::Null => None,
            Value::String(value) => Some(value.clone()),
            _ => None,
        });
    let related_paths = object
        .get("related_paths")
        .or_else(|| object.get("relatedPaths"))
        .map(parse_related_paths)
        .unwrap_or_else(|| Some(Vec::new()))?;
    Some(RecordFileReviewArgs {
        path,
        verdict,
        summary,
        finding_id,
        related_paths,
    })
}

fn parse_related_paths(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Null => Some(Vec::new()),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(ToString::to_string))
            .collect(),
        Value::String(value) if value.trim().is_empty() => Some(Vec::new()),
        Value::String(value) => Some(vec![value.clone()]),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::runtime::tools::registry::{
        CustomToolContext, CustomToolHandler, CustomToolOutput, ToolRegistry,
    };

    #[test]
    fn validation_rejects_arguments_over_capability_input_limit() {
        let registry = ToolRegistry::review_defaults().expect("registry");
        let mut capabilities = CapabilitySet::review_read_only();
        capabilities.tool_input.max_argument_bytes = 1;
        let call = ModelToolCall {
            call_id: ToolCallId("call".to_string()),
            index: 0,
            name: ToolId::from(ToolName::ReadDiff),
            raw_arguments: "{}".to_string(),
        };

        let err = validate_invocation(
            SessionId("session".to_string()),
            TurnId(1),
            call,
            capabilities,
            FsScope::repo_root().scope_key(&SnapshotId("snapshot".to_string())),
            &registry,
        )
        .expect_err("oversized arguments should be denied");

        assert_eq!(err.2, ToolErrorCode::TooLarge);
    }

    #[test]
    fn validation_accepts_flexible_record_file_review_optional_fields() {
        let registry = ToolRegistry::review_defaults().expect("registry");
        let invocation = validate_invocation(
            SessionId("session".to_string()),
            TurnId(1),
            raw_call(
                "review",
                ToolId::from(ToolName::RecordFileReview),
                r#"{"path":"src/a.ts","verdict":"clean","summary":"inspected","findingId":null,"relatedPaths":"src/b.ts"}"#,
            ),
            CapabilitySet::review_read_only(),
            FsScope::repo_root().scope_key(&SnapshotId("snapshot".to_string())),
            &registry,
        )
        .expect("record_file_review args should validate");

        let ToolArgs::RecordFileReview {
            finding_id,
            related_paths,
            ..
        } = invocation.args
        else {
            panic!("expected record_file_review args");
        };
        assert!(finding_id.is_none());
        assert_eq!(related_paths.len(), 1);
        assert_eq!(related_paths[0].display(), "src/b.ts");
    }

    #[test]
    fn validation_rejects_custom_arguments_outside_declared_schema() {
        let mut registry = ToolRegistry::review_defaults().expect("registry");
        let tool_id = ToolId::parse("argus.issue_context").expect("tool id");
        registry
            .register_custom(
                tool_id.clone(),
                "Issue context",
                json!({
                    "type": "object",
                    "required": ["issueId"],
                    "additionalProperties": false,
                    "properties": {
                        "issueId": { "type": "string" },
                        "includeHistory": { "type": "boolean" }
                    }
                }),
                false,
                Arc::new(NeverCalledTool),
            )
            .expect("custom tool");
        let mut capabilities = CapabilitySet::review_read_only();
        capabilities.grant(tool_id.clone(), ToolGrant::allow_custom_read_only());
        let scope_key = FsScope::repo_root().scope_key(&SnapshotId("snapshot".to_string()));

        let valid = validate_invocation(
            SessionId("session".to_string()),
            TurnId(1),
            raw_call(
                "valid",
                tool_id.clone(),
                r#"{"issueId":"123","includeHistory":false}"#,
            ),
            capabilities.clone(),
            scope_key.clone(),
            &registry,
        )
        .expect("valid custom args");
        assert!(matches!(valid.args, ToolArgs::Raw(_)));

        let wrong_type = validate_invocation(
            SessionId("session".to_string()),
            TurnId(1),
            raw_call("wrong-type", tool_id.clone(), r#"{"issueId":123}"#),
            capabilities.clone(),
            scope_key.clone(),
            &registry,
        )
        .expect_err("schema should reject wrong primitive type");
        assert_eq!(wrong_type.2, ToolErrorCode::InvalidArgs);

        let extra_property = validate_invocation(
            SessionId("session".to_string()),
            TurnId(1),
            raw_call("extra", tool_id, r#"{"issueId":"123","extra":true}"#),
            capabilities,
            scope_key,
            &registry,
        )
        .expect_err("schema should reject undeclared properties");
        assert_eq!(extra_property.2, ToolErrorCode::InvalidArgs);
    }

    fn raw_call(id: &str, tool_id: ToolId, raw_arguments: &str) -> ModelToolCall {
        ModelToolCall {
            call_id: ToolCallId(id.to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: raw_arguments.to_string(),
        }
    }

    struct NeverCalledTool;

    #[async_trait]
    impl CustomToolHandler for NeverCalledTool {
        async fn execute(
            &self,
            _context: CustomToolContext,
            _args: Value,
            _cancel: CancellationToken,
        ) -> RuntimeResult<CustomToolOutput> {
            unreachable!("validation should not execute custom tools")
        }
    }
}
