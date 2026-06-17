use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken as Cancellation;

use crate::reviewer_kernel::kernel_types::{
    ConversationItem, ModelToolCall, ModelTurn, RuntimeError, RuntimeResult, SessionScope,
    ToolCallId, ToolId, TurnId,
};
use crate::reviewer_kernel::model::ConcurrentModelClient;
use crate::reviewer_kernel::review_contract::{Role, TokenUsage};

use super::transport::RunnerCallbackTransport;
use super::RUNNER_PROTOCOL_VERSION;

#[cfg(test)]
pub(crate) struct TestRunnerModel {
    target_path: String,
    search_query: String,
}

#[cfg(test)]
impl TestRunnerModel {
    pub(crate) fn new(target_path: String, search_query: String) -> Self {
        Self {
            target_path,
            search_query,
        }
    }
}

#[cfg(test)]
#[async_trait]
impl ConcurrentModelClient for TestRunnerModel {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
        _cancel: Cancellation,
    ) -> RuntimeResult<ModelTurn> {
        let usage = TokenUsage {
            input_tokens: transcript.len() as u64 * 64,
            output_tokens: 32,
            total_tokens: transcript.len() as u64 * 64 + 32,
            cached_input_tokens: 0,
        };
        let tool_result_count = transcript
            .iter()
            .filter(|item| matches!(item, ConversationItem::ToolResult { .. }))
            .count();
        if tool_result_count == 0 {
            return Ok(ModelTurn::ToolCalls {
                usage,
                calls: vec![
                    model_tool_call(scope, turn_id, 0, "read-diff", "read_diff", json!({}))?,
                    model_tool_call(
                        scope,
                        turn_id,
                        1,
                        "read-file",
                        "read_file",
                        json!({ "path": self.target_path }),
                    )?,
                    model_tool_call(
                        scope,
                        turn_id,
                        2,
                        "search",
                        "search_text",
                        json!({ "query": self.search_query }),
                    )?,
                ],
            });
        }
        let content = if scope.id.0.contains('/') {
            json!({
                "status": if scope.id.0.contains("/validate-") { "supported" } else { "insufficient" },
                "summary": "deterministic SDK smoke child packet completed",
                "checkedPaths": [self.target_path],
                "evidence": [],
                "openQuestions": [],
                "candidateFindings": []
            })
        } else {
            json!({
                "verdict": "clean",
                "summary": "deterministic SDK smoke review completed",
                "candidates": [],
                "notes": [],
                "completeness": {
                    "status": "complete",
                    "checkedChangedFiles": [self.target_path],
                    "incompleteReasons": []
                }
            })
        };
        Ok(ModelTurn::Text {
            usage,
            content: content.to_string(),
        })
    }
}

pub(crate) struct CallbackReviewModel {
    run_id: String,
    transport: Arc<dyn RunnerCallbackTransport>,
}

impl CallbackReviewModel {
    pub(crate) fn new(run_id: String, transport: Arc<dyn RunnerCallbackTransport>) -> Self {
        Self { run_id, transport }
    }
}

#[async_trait]
impl ConcurrentModelClient for CallbackReviewModel {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
        _cancel: Cancellation,
    ) -> RuntimeResult<ModelTurn> {
        let params =
            RunnerModelCompleteParams::from_runtime(&self.run_id, scope, transcript, turn_id);
        let value = self
            .transport
            .request("model.complete", json!(params))
            .map_err(runtime_error)?;
        let result =
            serde_json::from_value::<RunnerModelCompleteResult>(value).map_err(|error| {
                RuntimeError::InvalidInput(format!("invalid model.complete result: {error}"))
            })?;
        let usage = result.usage.unwrap_or_default().into_token_usage();
        if !result.tool_calls.is_empty() {
            let calls = result
                .tool_calls
                .into_iter()
                .enumerate()
                .map(|(index, call)| {
                    Ok(ModelToolCall {
                        call_id: ToolCallId(
                            call.call_id
                                .unwrap_or_else(|| format!("{}-{}-{index}", scope.id.0, turn_id.0)),
                        ),
                        index,
                        name: ToolId::parse(&call.tool_id)?,
                        raw_arguments: call.arguments.to_string(),
                    })
                })
                .collect::<RuntimeResult<Vec<_>>>()?;
            return Ok(ModelTurn::ToolCalls { calls, usage });
        }
        Ok(ModelTurn::Text {
            content: result.content.unwrap_or_default(),
            usage,
        })
    }
}

#[cfg(test)]
fn model_tool_call(
    scope: &SessionScope,
    turn_id: TurnId,
    index: usize,
    suffix: &str,
    tool_id: &str,
    arguments: serde_json::Value,
) -> RuntimeResult<ModelToolCall> {
    Ok(ModelToolCall {
        call_id: ToolCallId(format!("{}-{}-{suffix}", scope.id.0, turn_id.0)),
        index,
        name: ToolId::parse(tool_id)?,
        raw_arguments: arguments.to_string(),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunnerModelCompleteParams {
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
    fn from_runtime(
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
struct RunnerModelCompleteResult {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RunnerModelToolCallResult>,
    #[serde(default)]
    usage: Option<RunnerTokenUsage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunnerModelToolCallResult {
    #[serde(default)]
    call_id: Option<String>,
    tool_id: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Copy, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunnerTokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
}

impl RunnerTokenUsage {
    fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            cached_input_tokens: self.cached_input_tokens,
        }
    }
}

fn runtime_error(error: anyhow::Error) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}
