use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::kernel_types::{
    ConversationItem, ModelToolCall, ModelTurn, RuntimeResult, SessionScope, ToolCallId, ToolId,
    TurnId,
};
use crate::reviewer_kernel::model::ConcurrentModelClient;
use crate::reviewer_kernel::review_contract::TokenUsage;

pub(crate) struct DeterministicReviewModel {
    target_path: String,
    search_query: String,
}

impl DeterministicReviewModel {
    pub(crate) fn new(target_path: String, search_query: String) -> Self {
        Self {
            target_path,
            search_query,
        }
    }
}

#[async_trait]
impl ConcurrentModelClient for DeterministicReviewModel {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
        _cancel: CancellationToken,
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
                    "reviewedChangedFiles": [self.target_path],
                    "reviewedRiskEntries": [],
                    "unreviewedRiskEntries": [],
                    "unresolvedQuestions": [],
                    "incompleteReasons": [],
                    "ignoredChildCandidates": []
                }
            })
        };
        Ok(ModelTurn::Text {
            usage,
            content: content.to_string(),
        })
    }
}

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
