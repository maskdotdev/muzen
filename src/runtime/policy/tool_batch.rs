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
    pub(crate) fn terminal_denial_before_evidence(
        &self,
        tool_id: &ToolId,
        evidence: &SessionEvidence,
    ) -> Option<ToolPolicyDenial> {
        let evidence_ready = evidence.ready();
        if evidence_ready {
            if tool_id.as_builtin() == Some(ToolName::Finish) && !evidence.ready_to_finish() {
                return Some(ToolPolicyDenial {
                    code: ToolErrorCode::ToolNotAllowed,
                    message: evidence.finish_coverage_denial_message(),
                    retryable: true,
                });
            }
            return None;
        }
        if matches!(
            tool_id.as_builtin(),
            Some(ToolName::RecordFileReview | ToolName::RecordFinding | ToolName::Finish)
        ) {
            return Some(ToolPolicyDenial {
                code: ToolErrorCode::ToolNotAllowed,
                message: "terminal tool requires successful read_diff, read_file/read_file_range/read_head_file, and search_text evidence first".to_string(),
                retryable: false,
            });
        }
        None
    }

    pub(crate) fn plan_tool_batch(
        &self,
        calls: Vec<ModelToolCall>,
        evidence: &SessionEvidence,
        remaining_tool_calls: usize,
    ) -> ToolBatchPolicyPlan {
        let mut scheduled_count = 0usize;
        let mut allowed_calls = Vec::new();
        let mut denied_calls = Vec::new();
        for call in calls {
            if scheduled_count >= remaining_tool_calls {
                denied_calls.push(ToolPolicyDeniedCall {
                    index: call.index,
                    call_id: call.call_id,
                    tool_id: call.name,
                    denial: ToolPolicyDenial {
                        code: ToolErrorCode::BudgetExceeded,
                        message: "session tool-call budget exhausted".to_string(),
                        retryable: false,
                    },
                });
                continue;
            }
            scheduled_count = scheduled_count.saturating_add(1);
            if let Some(denial) = self.terminal_denial_before_evidence(&call.name, evidence) {
                denied_calls.push(ToolPolicyDeniedCall {
                    index: call.index,
                    call_id: call.call_id,
                    tool_id: call.name,
                    denial,
                });
            } else if let Some(denial) = evidence.file_review_scope_denial(&call) {
                denied_calls.push(ToolPolicyDeniedCall {
                    index: call.index,
                    call_id: call.call_id,
                    tool_id: call.name,
                    denial,
                });
            } else if let Some(denial) = evidence.finding_scope_denial(&call) {
                denied_calls.push(ToolPolicyDeniedCall {
                    index: call.index,
                    call_id: call.call_id,
                    tool_id: call.name,
                    denial,
                });
            } else {
                allowed_calls.push(call);
            }
        }
        ToolBatchPolicyPlan {
            scheduled_count,
            allowed_calls,
            denied_calls,
        }
    }
}

pub(crate) struct ToolBatchPolicyPlan {
    pub(crate) scheduled_count: usize,
    pub(crate) allowed_calls: Vec<ModelToolCall>,
    pub(crate) denied_calls: Vec<ToolPolicyDeniedCall>,
}

#[derive(Debug)]
pub(crate) struct ToolPolicyDeniedCall {
    pub(crate) index: usize,
    pub(crate) call_id: ToolCallId,
    pub(crate) tool_id: ToolId,
    pub(crate) denial: ToolPolicyDenial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolPolicyDenial {
    pub(crate) code: ToolErrorCode,
    pub(crate) message: String,
    pub(crate) retryable: bool,
}
