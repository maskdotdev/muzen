use serde_json::json;

use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::{
    ConversationItem, SessionScope, ToolResultEnvelope, TurnId,
};
use crate::reviewer_kernel::policy::{ReviewerPolicy, SessionEvidence};
use crate::reviewer_kernel::review_contract::ToolCounts;
use crate::reviewer_kernel::tool_engine::{count_tool_result, ToolEngine};

pub(crate) struct ToolResultEffectProcessor<'a> {
    policy: &'a ReviewerPolicy,
    tools: &'a ToolEngine,
    events: &'a RuntimeEventDispatcher,
}

impl<'a> ToolResultEffectProcessor<'a> {
    pub(crate) fn new(
        policy: &'a ReviewerPolicy,
        tools: &'a ToolEngine,
        events: &'a RuntimeEventDispatcher,
        _review_revision_id: &'a str,
    ) -> Self {
        Self {
            policy,
            tools,
            events,
        }
    }

    pub(crate) fn apply_batch(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        results: Vec<ToolResultEnvelope>,
        mut state: ToolResultBatchState<'_>,
    ) -> ToolResultBatchOutcome {
        for result in &results {
            if result.ok {
                self.policy.observe_evidence_result(state.evidence, result);
            }
        }
        for result in results {
            self.apply_result(scope, turn_id, result, &mut state);
        }
        ToolResultBatchOutcome {
            terminal_seen: false,
            corrective_feedback: None,
        }
    }

    fn apply_result(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        result: ToolResultEnvelope,
        state: &mut ToolResultBatchState<'_>,
    ) {
        let mut artifact = None;
        if let Some(artifact_id) = &result.artifact_id {
            artifact = self.tools.artifact(artifact_id);
        }
        let runtime_events = self.policy.plan_tool_result_runtime_events(
            scope,
            turn_id,
            &result,
            artifact.as_ref(),
            None,
        );
        for planned in runtime_events.events {
            self.events
                .emit_runtime_with_context(planned.context, planned.event);
        }
        count_tool_result(state.tool_counts, &result);
        let transcript_result = artifact
            .as_ref()
            .map(|artifact| {
                tool_result_with_artifact_content(result.clone(), artifact.content.as_str())
            })
            .unwrap_or(result);
        state.transcript.push(
            self.policy
                .plan_tool_result_transcript_item(transcript_result),
        );
    }
}

fn tool_result_with_artifact_content(
    mut result: ToolResultEnvelope,
    artifact_content: &str,
) -> ToolResultEnvelope {
    let artifact_content = if artifact_content.len() > 45_000 {
        format!(
            "{}\n...[truncated]",
            artifact_content.chars().take(45_000).collect::<String>()
        )
    } else {
        artifact_content.to_string()
    };
    let mut data = result
        .data
        .take()
        .and_then(|data| data.as_object().cloned())
        .unwrap_or_default();
    data.insert("artifactContent".to_string(), json!(artifact_content));
    result.data = Some(serde_json::Value::Object(data));
    result
}

pub(crate) struct ToolResultBatchState<'a> {
    pub(crate) evidence: &'a mut SessionEvidence,
    pub(crate) tool_counts: &'a mut ToolCounts,
    pub(crate) transcript: &'a mut Vec<ConversationItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResultBatchOutcome {
    pub(crate) terminal_seen: bool,
    pub(crate) corrective_feedback: Option<String>,
}

#[cfg(test)]
mod tests;
