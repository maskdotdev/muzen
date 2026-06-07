use std::sync::Arc;

use async_trait::async_trait;

use crate::contracts::{Role, TokenUsage};
use crate::runtime::contracts::{
    ArtifactId, ConversationItem, ModelCostEstimate, ModelToolCall, ModelTurn, RuntimeResult,
    SessionScope, SnapshotId, ToolCallId, ToolErrorCode, ToolId, TurnId,
};
use crate::runtime::model::{ConcurrentModelClient as RuntimeModelClient, StaticModelRouter};

use crate::reviewer::adapters::{model_adapters, Cancellation};
#[async_trait]
pub trait ReviewModel: Send + Sync {
    async fn complete_review(
        &self,
        request: ReviewModelRequest,
        cancel: Cancellation,
    ) -> RuntimeResult<ReviewModelTurn>;

    fn estimate_cost(&self, _usage: &TokenUsage) -> Option<ModelCostEstimate> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct ReviewModelRequest {
    pub session_id: String,
    pub role: Role,
    pub objective: String,
    pub snapshot_id: Option<SnapshotId>,
    pub model_profile_id: Option<String>,
    pub turn: u32,
    pub transcript: Vec<ReviewTranscriptItem>,
}

impl ReviewModelRequest {
    fn from_runtime(
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
    ) -> Self {
        Self {
            session_id: scope.id.0.clone(),
            role: scope.role,
            objective: scope.objective.clone(),
            snapshot_id: scope.snapshot_id.clone(),
            model_profile_id: scope.model_profile_id.clone(),
            turn: turn_id.0,
            transcript: transcript
                .iter()
                .map(ReviewTranscriptItem::from_conversation_item)
                .collect(),
        }
    }

    pub fn transcript_item_count(&self) -> usize {
        self.transcript.len()
    }

    pub fn tool_result_count(&self) -> usize {
        self.transcript
            .iter()
            .filter(|item| matches!(item, ReviewTranscriptItem::ToolResult { .. }))
            .count()
    }

    pub fn tool_call_id(&self, suffix: impl AsRef<str>) -> String {
        format!("{}-{}-{}", self.session_id, self.turn, suffix.as_ref())
    }
}

#[derive(Debug, Clone)]
pub enum ReviewTranscriptItem {
    System {
        content: String,
    },
    User {
        content: String,
    },
    AssistantText {
        content: String,
    },
    AssistantToolCalls {
        calls: Vec<ReviewToolCall>,
    },
    ToolResult {
        call_id: String,
        tool_id: String,
        ok: bool,
        artifact_id: Option<ArtifactId>,
        data: Option<serde_json::Value>,
        error_code: Option<ToolErrorCode>,
    },
}

impl ReviewTranscriptItem {
    fn from_conversation_item(item: &ConversationItem) -> Self {
        match item {
            ConversationItem::System { content } => Self::System {
                content: content.clone(),
            },
            ConversationItem::User { content } => Self::User {
                content: content.clone(),
            },
            ConversationItem::AssistantText { content } => Self::AssistantText {
                content: content.clone(),
            },
            ConversationItem::AssistantToolCalls { calls } => Self::AssistantToolCalls {
                calls: calls
                    .iter()
                    .map(ReviewToolCall::from_model_tool_call)
                    .collect(),
            },
            ConversationItem::ToolResult {
                call_id,
                name,
                content,
            } => Self::ToolResult {
                call_id: call_id.0.clone(),
                tool_id: name.as_str().to_string(),
                ok: content.ok,
                artifact_id: content.artifact_id.clone(),
                data: content.data.clone(),
                error_code: content.error.as_ref().map(|error| error.code),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewToolCall {
    pub call_id: Option<String>,
    pub tool_id: String,
    pub arguments: serde_json::Value,
}

impl ReviewToolCall {
    pub fn new(tool_id: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            call_id: None,
            tool_id: tool_id.into(),
            arguments,
        }
    }

    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    fn from_model_tool_call(call: &ModelToolCall) -> Self {
        let arguments = serde_json::from_str(&call.raw_arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.raw_arguments.clone()));
        Self {
            call_id: Some(call.call_id.0.clone()),
            tool_id: call.name.as_str().to_string(),
            arguments,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ReviewModelTurn {
    Text {
        content: String,
        usage: TokenUsage,
    },
    ToolCalls {
        calls: Vec<ReviewToolCall>,
        usage: TokenUsage,
    },
}

struct ReviewModelClientAdapter {
    model: Arc<dyn ReviewModel>,
}

impl ReviewModelClientAdapter {
    fn new(model: Arc<dyn ReviewModel>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl RuntimeModelClient for ReviewModelClientAdapter {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
        cancel: Cancellation,
    ) -> RuntimeResult<ModelTurn> {
        let request = ReviewModelRequest::from_runtime(scope, transcript, turn_id);
        match self.model.complete_review(request, cancel).await? {
            ReviewModelTurn::Text { content, usage } => Ok(ModelTurn::Text { content, usage }),
            ReviewModelTurn::ToolCalls { calls, usage } => {
                let mut model_calls = Vec::with_capacity(calls.len());
                for (index, call) in calls.into_iter().enumerate() {
                    model_calls.push(ModelToolCall {
                        call_id: ToolCallId(
                            call.call_id
                                .unwrap_or_else(|| format!("{}-{}-{index}", scope.id.0, turn_id.0)),
                        ),
                        index,
                        name: ToolId::parse(&call.tool_id)?,
                        raw_arguments: call.arguments.to_string(),
                    });
                }
                Ok(ModelTurn::ToolCalls {
                    calls: model_calls,
                    usage,
                })
            }
        }
    }

    fn estimate_cost(&self, usage: &TokenUsage) -> Option<ModelCostEstimate> {
        self.model.estimate_cost(usage)
    }
}

pub fn review_model_router(model: Arc<dyn ReviewModel>) -> Arc<dyn model_adapters::ModelRouter> {
    Arc::new(StaticModelRouter::new(Arc::new(
        ReviewModelClientAdapter::new(model),
    )))
}
