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
#[derive(Debug, Clone, Default)]
pub struct ReviewerPolicy;

impl ReviewerPolicy {
    pub fn new() -> Self {
        Self
    }

    pub fn should_retry_model_error(&self, error: &RuntimeError) -> bool {
        match error {
            RuntimeError::Provider { retryable, .. } => *retryable,
            RuntimeError::ProviderMessage { retryable, .. } => *retryable,
            RuntimeError::Timeout => true,
            RuntimeError::Cancelled => false,
            _ => false,
        }
    }

    pub(crate) fn should_cancel_job_after_model_error(&self, error: &RuntimeError) -> bool {
        matches!(
            error,
            RuntimeError::Provider {
                retryable: false,
                ..
            } | RuntimeError::ProviderMessage {
                retryable: false,
                ..
            }
        )
    }
}
