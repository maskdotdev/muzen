use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken as Cancellation;

use crate::reviewer_kernel::kernel_types::{
    ConversationItem, ModelToolCall, ModelTurn, RuntimeError, RuntimeResult, SessionScope,
    ToolCallId, ToolId, TurnId,
};
use crate::reviewer_kernel::model::ConcurrentModelClient;
#[cfg(test)]
use crate::reviewer_kernel::review_contract::TokenUsage;

use super::callback_types::{RunnerModelCompleteParams, RunnerModelCompleteResult};
use super::transport::RunnerCallbackTransport;

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

fn runtime_error(error: anyhow::Error) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}
