use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::policy::{ReviewerPolicy, SessionEvidence};
use crate::reviewer_kernel::tool_engine::ToolEngine;

pub(crate) struct ToolBatchRunner<'a> {
    policy: &'a ReviewerPolicy,
    tools: &'a ToolEngine,
    events: &'a RuntimeEventDispatcher,
}

#[derive(Debug, Clone)]
struct RequestedToolCallTrace {
    tool_id: ToolId,
    argument_bytes: usize,
    argument_hash: String,
    argument_summary: Value,
}

#[derive(Debug, Clone)]
struct AcceptedToolCallRepair {
    call_id: ToolCallId,
    index: usize,
    original_tool_id: ToolId,
    canonical_call: ModelToolCall,
    error_code: ToolErrorCode,
    repair_kinds: Vec<&'static str>,
}

impl<'a> ToolBatchRunner<'a> {
    pub(crate) fn new(
        policy: &'a ReviewerPolicy,
        tools: &'a ToolEngine,
        events: &'a RuntimeEventDispatcher,
    ) -> Self {
        Self {
            policy,
            tools,
            events,
        }
    }

    pub(crate) async fn execute(
        &self,
        scope: SessionScope,
        turn_id: TurnId,
        calls: Vec<ModelToolCall>,
        evidence: &SessionEvidence,
        remaining_tool_calls: usize,
        cancel: CancellationToken,
    ) -> Vec<ToolResultEnvelope> {
        let requested_calls = calls
            .iter()
            .map(|call| {
                (
                    call.call_id.clone(),
                    RequestedToolCallTrace {
                        tool_id: call.name.clone(),
                        argument_bytes: call.raw_arguments.len(),
                        argument_hash: blake3::hash(call.raw_arguments.as_bytes())
                            .to_hex()
                            .to_string(),
                        argument_summary: call.redacted_argument_summary(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        self.events.emit_planned_runtime(self.policy.plan_agent_trace_event(
            &scope,
            Some(turn_id),
            "tool_calls_requested",
            format!("model requested {} tool call(s)", calls.len()),
            json!({
                "remainingToolCalls": remaining_tool_calls,
                "calls": calls.iter().map(|call| json!({
                    "callId": call.call_id.0,
                    "index": call.index,
                    "toolName": call.name.as_str(),
                    "argumentBytes": call.raw_arguments.len(),
                    "argumentHash": blake3::hash(call.raw_arguments.as_bytes()).to_hex().to_string(),
                    "argumentSummary": call.redacted_argument_summary(),
                })).collect::<Vec<_>>()
            }),
        ));
        let (calls, accepted_repairs) = self.repair_tool_calls(calls);
        for repair in &accepted_repairs {
            self.emit_tool_call_repair_trace(
                &scope,
                turn_id,
                repair.call_id.clone(),
                repair.index,
                repair.canonical_call.name.clone(),
                repair.error_code,
                "tool call was canonicalized before policy planning",
                requested_calls.get(&repair.call_id),
                Some(repair),
                Some(repair.repair_kinds.as_slice()),
                true,
                true,
            );
        }
        let plan = self
            .policy
            .plan_tool_batch(calls, evidence, remaining_tool_calls);
        let policy_denied_call_ids = plan
            .denied_calls
            .iter()
            .map(|denied| denied.call_id.clone())
            .collect::<HashSet<_>>();
        for denied in &plan.denied_calls {
            self.emit_tool_call_repair_trace(
                &scope,
                turn_id,
                denied.call_id.clone(),
                denied.index,
                denied.tool_id.clone(),
                denied.denial.code,
                denied.denial.message.as_str(),
                requested_calls.get(&denied.call_id),
                None,
                None,
                false,
                false,
            );
        }
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                &scope,
                Some(turn_id),
                "tool_batch_planned",
                format!(
                    "tool policy scheduled {} call(s), denied {} call(s)",
                    plan.scheduled_count,
                    plan.denied_calls.len()
                ),
                json!({
                    "scheduledCount": plan.scheduled_count,
                    "deniedCount": plan.denied_calls.len(),
                    "allowed": plan.allowed_calls.iter().map(|call| json!({
                        "callId": call.call_id.0,
                        "index": call.index,
                        "toolId": call.name.as_str(),
                    })).collect::<Vec<_>>(),
                    "denied": plan.denied_calls.iter().map(|call| json!({
                        "callId": call.call_id.0,
                        "index": call.index,
                        "toolId": call.tool_id.as_str(),
                        "code": call.denial.code,
                        "retryable": call.denial.retryable,
                        "reason": call.denial.message,
                    })).collect::<Vec<_>>(),
                }),
            ));
        if let Some(planned) =
            self.policy
                .plan_tool_batch_started_runtime_event(&scope, turn_id, plan.scheduled_count)
        {
            self.events.emit_planned_runtime(planned);
        }
        let mut indexed_results = Vec::new();
        for denied in plan.denied_calls {
            let result = self.tools.error_result(
                denied.call_id,
                denied.tool_id,
                denied.denial.code,
                &denied.denial.message,
                denied.denial.retryable,
            );
            self.tools
                .record_tool_metrics(std::slice::from_ref(&result));
            indexed_results.push((denied.index, result));
        }

        if !plan.allowed_calls.is_empty() {
            let allowed_indices = plan
                .allowed_calls
                .iter()
                .map(|call| call.index)
                .collect::<Vec<_>>();
            let allowed_results = self
                .tools
                .execute_batch(scope.clone(), turn_id, plan.allowed_calls, cancel)
                .await;
            for (index, result) in allowed_indices.into_iter().zip(allowed_results) {
                indexed_results.push((index, result));
            }
        }
        indexed_results.sort_by_key(|(index, _)| *index);
        for (index, result) in &indexed_results {
            if policy_denied_call_ids.contains(&result.tool_call_id) {
                continue;
            }
            if let Some(error) = result
                .error
                .as_ref()
                .filter(|error| trace_tool_call_repair(error.code))
            {
                self.emit_tool_call_repair_trace(
                    &scope,
                    turn_id,
                    result.tool_call_id.clone(),
                    *index,
                    result.tool_name.clone(),
                    error.code,
                    error.message.as_str(),
                    requested_calls.get(&result.tool_call_id),
                    None,
                    None,
                    false,
                    false,
                );
            }
        }
        indexed_results
            .into_iter()
            .map(|(_, result)| result)
            .collect()
    }

    fn repair_tool_calls(
        &self,
        calls: Vec<ModelToolCall>,
    ) -> (Vec<ModelToolCall>, Vec<AcceptedToolCallRepair>) {
        let mut repaired_calls = Vec::with_capacity(calls.len());
        let mut accepted_repairs = Vec::new();
        for call in calls {
            let mut canonical = call.clone();
            let original_tool_id = call.name.clone();
            let mut repair_kinds = Vec::new();

            if self.tools.registry.definition(&canonical.name).is_none() {
                if let Some(tool_id) = self.tools.registry.tool_id_for_model_alias(&canonical.name)
                {
                    if tool_id != canonical.name {
                        canonical.name = tool_id;
                        repair_kinds.push("tool_alias");
                    }
                }
            }

            if !repair_kinds.is_empty() {
                accepted_repairs.push(AcceptedToolCallRepair {
                    call_id: canonical.call_id.clone(),
                    index: canonical.index,
                    original_tool_id: original_tool_id.clone(),
                    canonical_call: canonical.clone(),
                    error_code: ToolErrorCode::UnknownTool,
                    repair_kinds,
                });
            }
            repaired_calls.push(canonical);
        }
        (repaired_calls, accepted_repairs)
    }

    fn emit_tool_call_repair_trace(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        call_id: ToolCallId,
        index: usize,
        tool_id: ToolId,
        error_code: ToolErrorCode,
        reason: &str,
        requested_call: Option<&RequestedToolCallTrace>,
        accepted_repair: Option<&AcceptedToolCallRepair>,
        repair_kinds: Option<&[&'static str]>,
        repair_attempted: bool,
        repair_accepted: bool,
    ) {
        let (argument_bytes, argument_hash, argument_summary, requested_tool_id) = requested_call
            .map(|call| {
                (
                    call.argument_bytes,
                    call.argument_hash.clone(),
                    call.argument_summary.clone(),
                    call.tool_id.clone(),
                )
            })
            .unwrap_or_else(|| (0, String::new(), json!(null), tool_id.clone()));
        let original_tool_id = accepted_repair
            .map(|repair| repair.original_tool_id.clone())
            .unwrap_or(requested_tool_id);
        let canonical = accepted_repair.map(|repair| {
            json!({
                "canonicalToolId": repair.canonical_call.name.as_str(),
                "canonicalArgumentBytes": repair.canonical_call.raw_arguments.len(),
                "canonicalArgumentHash": blake3::hash(repair.canonical_call.raw_arguments.as_bytes()).to_hex().to_string(),
                "canonicalArgumentSummary": repair.canonical_call.redacted_argument_summary(),
                "repairKinds": repair.repair_kinds,
            })
        });
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                scope,
                Some(turn_id),
                "tool_call_repair",
                if repair_accepted {
                    format!("tool call {} was repaired: {reason}", call_id.0)
                } else {
                    format!("tool call {} was not repaired: {reason}", call_id.0)
                },
                json!({
                    "callId": call_id.0,
                    "index": index,
                    "toolId": tool_id.as_str(),
                    "originalToolId": original_tool_id.as_str(),
                    "errorCode": error_code,
                    "reason": reason,
                    "repairAttempted": repair_attempted,
                    "repairAccepted": repair_accepted,
                    "repairKinds": repair_kinds.unwrap_or(&[]),
                    "argumentBytes": argument_bytes,
                    "argumentHash": argument_hash,
                    "argumentSummary": argument_summary,
                    "acceptedRepair": canonical,
                }),
            ));
    }
}

fn trace_tool_call_repair(error_code: ToolErrorCode) -> bool {
    matches!(
        error_code,
        ToolErrorCode::InvalidArgs
            | ToolErrorCode::UnknownTool
            | ToolErrorCode::ToolNotAllowed
            | ToolErrorCode::PathDenied
            | ToolErrorCode::BudgetExceeded
    )
}

#[cfg(test)]
mod tests;
