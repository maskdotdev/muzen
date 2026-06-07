#![allow(unused_imports)]

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::contracts::{EventLevel, EventType, TokenUsage, ToolCounts, ToolName};
use crate::events::EventRecord;
use crate::runtime::contracts::{
    ArtifactView, CapabilitySet, ConversationItem, ModelOutputPolicy, ModelToolCall, RuntimeError,
    RuntimeEvent, RuntimeEventContext, SessionId, SessionScope, SessionTerminalDiagnostic,
    ToolCallId, ToolErrorCode, ToolId, ToolResultEnvelope, TurnId,
};
use crate::runtime::repo::RepoSnapshot;
use crate::runtime::tools::ToolRegistry;
use crate::util::redact_known_secrets;

use super::*;
impl ReviewerPolicy {
    pub(crate) fn plan_assistant_text_transcript_item(&self, content: String) -> ConversationItem {
        ConversationItem::AssistantText { content }
    }

    pub(crate) fn plan_assistant_tool_calls_transcript_item(
        &self,
        calls: &[ModelToolCall],
    ) -> ConversationItem {
        ConversationItem::AssistantToolCalls {
            calls: calls.to_vec(),
        }
    }

    pub(crate) fn plan_tool_result_transcript_item(
        &self,
        result: ToolResultEnvelope,
    ) -> ConversationItem {
        ConversationItem::ToolResult {
            call_id: result.tool_call_id.clone(),
            name: result.tool_name.clone(),
            content: Box::new(result),
        }
    }

    pub fn tool_schemas_for_transcript(
        &self,
        registry: &ToolRegistry,
        transcript: &[ConversationItem],
        capabilities: &CapabilitySet,
    ) -> Vec<Value> {
        let has_read_diff = transcript_has_successful_tool(transcript, ToolName::ReadDiff);
        let has_read_file = transcript_has_successful_tool(transcript, ToolName::ReadFile)
            || transcript_has_successful_tool(transcript, ToolName::ReadFileRange)
            || transcript_has_successful_tool(transcript, ToolName::ReadHeadFile);
        let has_search = transcript_has_successful_tool(transcript, ToolName::SearchText);

        let evidence_ready = has_read_diff && has_read_file && has_search;
        let tools = if evidence_ready {
            exploration_and_terminal_tools()
        } else {
            exploration_tools()
        };
        schemas_for_tools(registry, capabilities, tools)
    }

    pub fn compact_tool_result(
        &self,
        result: &ToolResultEnvelope,
        capabilities: &CapabilitySet,
    ) -> Value {
        let data = capabilities
            .model_output
            .include_tool_data
            .then(|| {
                result
                    .data
                    .as_ref()
                    .map(|data| compact_tool_data(result.tool_name.as_builtin(), data))
            })
            .flatten()
            .map(|data| apply_model_output_policy(data, &capabilities.model_output));
        json!({
            "ok": result.ok,
            "toolName": result.tool_name.as_str(),
            "artifactId": if capabilities.model_output.include_artifact_refs {
                result.artifact_id.as_ref().map(|id| id.0.as_str())
            } else {
                None
            },
            "cacheStatus": result.cache.status,
            "limits": {
                "truncated": result.limits.truncated,
                "outputBytes": result.limits.output_bytes,
                "searchedFiles": result.limits.searched_files,
                "skippedFiles": result.limits.skipped_files,
                "bytesScanned": result.limits.bytes_scanned,
            },
            "data": data,
            "error": result.error,
        })
    }
}

fn transcript_has_successful_tool(transcript: &[ConversationItem], expected: ToolName) -> bool {
    transcript.iter().any(|item| {
        matches!(
            item,
            ConversationItem::ToolResult { content, .. }
                if content.ok && content.tool_name.as_builtin() == Some(expected)
        )
    })
}
fn schemas_for_tools(
    registry: &ToolRegistry,
    capabilities: &CapabilitySet,
    tools: &[ToolName],
) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let tool_id = ToolId::from(*tool);
            if !capabilities.allow_tool(&tool_id) {
                return None;
            }
            registry.definition(&tool_id)
        })
        .map(|definition| {
            json!({
                "type": "function",
                "function": {
                    "name": definition.model_alias.as_str(),
                    "description": definition.description.clone(),
                    "parameters": definition.parameters.clone()
                }
            })
        })
        .collect()
}

const EXPLORATION_TOOLS: &[ToolName] = &[
    ToolName::ListChangedFiles,
    ToolName::ReadDiff,
    ToolName::ListFiles,
    ToolName::ReadFile,
    ToolName::ReadFileRange,
    ToolName::ReadBaseFile,
    ToolName::ReadHeadFile,
    ToolName::SearchText,
    ToolName::FindRelatedFiles,
    ToolName::FindTestsForFile,
    ToolName::ListImports,
];

const EXPLORATION_AND_TERMINAL_TOOLS: &[ToolName] = &[
    ToolName::ListChangedFiles,
    ToolName::ReadDiff,
    ToolName::ListFiles,
    ToolName::ReadFile,
    ToolName::ReadFileRange,
    ToolName::ReadBaseFile,
    ToolName::ReadHeadFile,
    ToolName::SearchText,
    ToolName::FindRelatedFiles,
    ToolName::FindTestsForFile,
    ToolName::ListImports,
    ToolName::RecordFinding,
    ToolName::RecordFileReview,
    ToolName::Finish,
];

fn exploration_tools() -> &'static [ToolName] {
    EXPLORATION_TOOLS
}

fn exploration_and_terminal_tools() -> &'static [ToolName] {
    EXPLORATION_AND_TERMINAL_TOOLS
}

fn compact_tool_data(tool: Option<ToolName>, data: &Value) -> Value {
    match tool {
        Some(ToolName::ReadDiff) => json!({
            "contentHash": data.get("contentHash").cloned(),
            "contentSnippet": data.get("content").and_then(Value::as_str).map(|value| truncate_chars(value, 1200)),
        }),
        Some(ToolName::ReadFile | ToolName::ReadHeadFile | ToolName::ReadBaseFile) => json!({
            "path": data.get("path").cloned(),
            "available": data.get("available").cloned(),
            "evidenceId": data.get("evidenceId").cloned(),
            "contentSnippet": data.get("content").and_then(Value::as_str).map(|value| truncate_chars(value, 1200)),
            "message": data.get("message").and_then(Value::as_str).map(|value| truncate_chars(value, 400)),
        }),
        Some(ToolName::SearchText) => json!({
            "query": data.get("query").cloned(),
            "returnedMatches": data.get("returnedMatches").cloned(),
            "truncated": data.get("truncated").cloned(),
            "matches": compact_string_array(data.get("matches"), 30, 300),
        }),
        Some(
            ToolName::ListChangedFiles
            | ToolName::ListFiles
            | ToolName::FindRelatedFiles
            | ToolName::FindTestsForFile,
        ) => json!({
            "changedFiles": compact_string_array(data.get("changedFiles"), 80, 240),
            "files": compact_string_array(data.get("files"), 80, 240),
            "path": data.get("path").cloned(),
        }),
        Some(ToolName::ListImports) => json!({
            "path": data.get("path").cloned(),
            "imports": compact_string_array(data.get("imports"), 80, 300),
        }),
        Some(ToolName::RecordFileReview) => json!({
            "path": data.get("path").cloned(),
            "verdict": data.get("verdict").cloned(),
            "summary": data
                .get("summary")
                .and_then(Value::as_str)
                .map(|value| truncate_chars(value, 500)),
            "findingId": data.get("findingId").cloned(),
            "relatedPaths": compact_string_array(data.get("relatedPaths"), 20, 300),
        }),
        _ => data.clone(),
    }
}

pub(crate) fn compact_string_array(
    value: Option<&Value>,
    max_items: usize,
    max_chars: usize,
) -> Value {
    let Some(items) = value.and_then(Value::as_array) else {
        return Value::Null;
    };
    Value::Array(
        items
            .iter()
            .take(max_items)
            .filter_map(Value::as_str)
            .map(|item| Value::String(truncate_chars(item, max_chars)))
            .collect(),
    )
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str("\n[truncated]");
    }
    output
}
fn apply_model_output_policy(data: Value, policy: &ModelOutputPolicy) -> Value {
    if policy.max_tool_data_bytes == 0 {
        return json!({
            "withheld": true,
            "reason": "model-visible tool data disabled by capability policy",
        });
    }
    let Ok(serialized) = serde_json::to_string(&data) else {
        return json!({
            "withheld": true,
            "reason": "tool data could not be serialized for model output",
        });
    };
    if serialized.len() <= policy.max_tool_data_bytes {
        return data;
    }
    json!({
        "truncated": true,
        "reason": "model-visible tool data exceeded capability policy",
        "bytes": serialized.len(),
        "snippet": truncate_chars(&serialized, policy.max_tool_data_bytes),
    })
}
