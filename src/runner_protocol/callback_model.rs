use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken as Cancellation;

use crate::reviewer_kernel::kernel_types::{
    ConversationItem, ModelToolCall, ModelTurn, RuntimeError, RuntimeResult, SessionScope,
    ToolCallId, ToolId, TurnId,
};
use crate::reviewer_kernel::model::ConcurrentModelClient;

use super::callback_types::{RunnerModelCompleteParams, RunnerModelCompleteResult};
use super::transport::RunnerCallbackTransport;

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

fn runtime_error(error: anyhow::Error) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}
