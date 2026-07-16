use async_trait::async_trait;
use serde_json::Value;

use crate::agent_runtime::{AgentDefinition, AgentMessage, ContentBlock, ModelProfile, Usage};

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub agent: AgentDefinition,
    pub model: ModelProfile,
    pub transcript: Vec<AgentMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStop {
    EndTurn,
}

#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub content: Vec<ContentBlock>,
    pub usage: Usage,
    pub stop: ModelStop,
}

#[derive(Debug, Clone)]
pub struct ModelProviderError {
    message: String,
    retryable: bool,
    details: Option<Value>,
}

impl ModelProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }

    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelTurn, ModelProviderError>;
}
