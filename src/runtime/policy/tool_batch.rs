use crate::runtime::contracts::{ModelToolCall, ToolCallId, ToolErrorCode, ToolId};

use super::{ReviewerPolicy, SessionEvidence};
impl ReviewerPolicy {
    pub(crate) fn plan_tool_batch(
        &self,
        calls: Vec<ModelToolCall>,
        _evidence: &SessionEvidence,
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
            allowed_calls.push(call);
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
