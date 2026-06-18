use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::reviewer_kernel::kernel_types::{ConversationItem, SessionScope, TurnId};
use crate::reviewer_kernel::review_contract::{Role, TokenUsage};

use super::RUNNER_PROTOCOL_VERSION;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunnerModelCompleteParams {
    protocol_version: String,
    run_id: String,
    session_id: String,
    role: Role,
    objective: String,
    snapshot_id: Option<String>,
    model_profile_id: Option<String>,
    turn: u32,
    transcript: Vec<Value>,
}

impl RunnerModelCompleteParams {
    pub(super) fn from_runtime(
        run_id: &str,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
    ) -> Self {
        Self {
            protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
            run_id: run_id.to_string(),
            session_id: scope.id.0.clone(),
            role: scope.role,
            objective: scope.objective.clone(),
            snapshot_id: scope
                .snapshot_id
                .as_ref()
                .map(|snapshot_id| snapshot_id.0.clone()),
            model_profile_id: scope.model_profile_id.clone(),
            turn: turn_id.0,
            transcript: transcript.iter().map(runner_transcript_item).collect(),
        }
    }
}

fn runner_transcript_item(item: &ConversationItem) -> Value {
    match item {
        ConversationItem::System { content } => {
            json!({"kind": "system", "content": content})
        }
        ConversationItem::User { content } => {
            json!({"kind": "user", "content": content})
        }
        ConversationItem::AssistantText { content } => {
            json!({"kind": "assistant_text", "content": content})
        }
        ConversationItem::AssistantToolCalls { calls } => json!({
            "kind": "assistant_tool_calls",
            "calls": calls.iter().map(|call| json!({
                "callId": call.call_id.0,
                "toolId": call.name.as_str(),
                "arguments": serde_json::from_str::<Value>(&call.raw_arguments)
                    .unwrap_or_else(|_| Value::String(call.raw_arguments.clone())),
            })).collect::<Vec<_>>()
        }),
        ConversationItem::ToolResult {
            call_id,
            name,
            content,
        } => json!({
            "kind": "tool_result",
            "callId": call_id.0,
            "toolId": name.as_str(),
            "ok": content.ok,
            "artifactId": content.artifact_id,
            "data": content.data,
            "errorCode": content.error.as_ref().map(|error| error.code),
        }),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunnerModelCompleteResult {
    #[serde(default)]
    pub(super) content: Option<String>,
    #[serde(default)]
    pub(super) tool_calls: Vec<RunnerModelToolCallResult>,
    #[serde(default)]
    pub(super) usage: Option<RunnerTokenUsage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunnerModelToolCallResult {
    #[serde(default)]
    pub(super) call_id: Option<String>,
    pub(super) tool_id: String,
    #[serde(default)]
    pub(super) arguments: Value,
}

#[derive(Debug, Copy, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunnerTokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
}

impl RunnerTokenUsage {
    pub(super) fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            cached_input_tokens: self.cached_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunnerToolExecuteParams {
    pub(super) protocol_version: String,
    pub(super) run_id: String,
    pub(super) session_id: String,
    pub(super) turn: u32,
    pub(super) call_id: String,
    pub(super) tool_id: String,
    pub(super) snapshot_id: String,
    pub(super) provider_resources: Vec<String>,
    pub(super) arguments: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunnerToolExecuteResult {
    #[serde(default)]
    pub(super) data: Option<Value>,
    #[serde(default)]
    pub(super) artifact: Option<RunnerToolArtifactResult>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RunnerToolArtifactResult {
    pub(super) key: String,
    pub(super) content: String,
}
