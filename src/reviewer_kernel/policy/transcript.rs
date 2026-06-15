use serde_json::{json, Value};

use crate::reviewer_kernel::kernel_types::{
    CapabilitySet, ConversationItem, ModelOutputPolicy, ToolId, ToolResultEnvelope,
};
use crate::reviewer_kernel::review_contract::ToolName;
use crate::reviewer_kernel::tool_engine::ToolRegistry;

use super::ReviewerPolicy;
impl ReviewerPolicy {
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
        _transcript: &[ConversationItem],
        capabilities: &CapabilitySet,
    ) -> Vec<Value> {
        schemas_for_capabilities(registry, capabilities)
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

// Tool exposure is capability-driven: every tool the session has been granted
// (allow == true) and that the registry can describe is advertised to the model.
// Builtin exploration tools are listed first in their canonical order so the
// prompt surface stays stable; any other granted tool (context.* semantic
// tools or host-registered custom tools) follows in
// deterministic ToolId order. This is the single path that decides what the
// model can call, so a granted tool that is not surfaced here is effectively
// invisible regardless of its grant.
fn schemas_for_capabilities(registry: &ToolRegistry, capabilities: &CapabilitySet) -> Vec<Value> {
    let mut schemas = Vec::new();
    let mut surfaced: std::collections::BTreeSet<ToolId> = std::collections::BTreeSet::new();
    for tool in exploration_tools() {
        let tool_id = ToolId::from(*tool);
        if !capabilities.allow_tool(&tool_id) {
            continue;
        }
        if let Some(definition) = registry.definition(&tool_id) {
            schemas.push(tool_schema_value(definition));
            surfaced.insert(tool_id);
        }
    }
    for (tool_id, grant) in &capabilities.tool_grants {
        if !grant.allow || surfaced.contains(tool_id) {
            continue;
        }
        if let Some(definition) = registry.definition(tool_id) {
            schemas.push(tool_schema_value(definition));
        }
    }
    schemas
}

fn tool_schema_value(
    definition: &crate::reviewer_kernel::tool_engine::registry::ToolDefinition,
) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": definition.model_alias.as_str(),
            "description": definition.description.clone(),
            "parameters": definition.parameters.clone()
        }
    })
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

fn exploration_tools() -> &'static [ToolName] {
    EXPLORATION_TOOLS
}

// Reviewers must be able to read whole changed hunks; snippets sized below a
// typical diff hunk starve every downstream evidence validator.
const MODEL_VISIBLE_DIFF_CHARS: usize = 12_000;
const MODEL_VISIBLE_FILE_CHARS: usize = 10_000;

fn compact_tool_data(tool: Option<ToolName>, data: &Value) -> Value {
    match tool {
        Some(ToolName::ReadDiff) => json!({
            "contentHash": data.get("contentHash").cloned(),
            "contentSnippet": data.get("content").and_then(Value::as_str).map(|value| truncate_chars(value, MODEL_VISIBLE_DIFF_CHARS)),
        }),
        Some(ToolName::ReadFile | ToolName::ReadHeadFile | ToolName::ReadBaseFile) => json!({
            "path": data.get("path").cloned(),
            "available": data.get("available").cloned(),
            "evidenceId": data.get("evidenceId").cloned(),
            "contentSnippet": data.get("content").and_then(Value::as_str).map(|value| truncate_chars(value, MODEL_VISIBLE_FILE_CHARS)),
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
