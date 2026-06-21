use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

mod delegates;
mod diff_risk;
mod finding_filters;
mod findings;
mod prompts;
mod reporting;
mod schemas;
mod sessions;
mod tasks;

pub(crate) use delegates::{register_autonomous_delegate_tools, AutonomousDelegateHost};
use delegates::{run_child_delegate, AutonomousDelegateState};
#[cfg(test)]
use diff_risk::diff_risk_inventory;
use diff_risk::truncate_chars;
#[cfg(test)]
use finding_filters::autonomous_candidate_rejection_reason;
use findings::{
    build_file_reviews, build_findings, parse_orchestrator_output, CandidateFinding,
    ValidationPacket,
};
use prompts::neutral_starter_context;
use reporting::build_run_metrics;
#[cfg(test)]
use schemas::child_response_format;
#[cfg(test)]
use schemas::session_output_valid;
use schemas::{orchestrator_final_instruction, orchestrator_response_format};
use sessions::{autonomous_orchestrator_budget, run_session_loop, SessionKind, SessionRunConfig};
#[cfg(test)]
use sessions::{session_turn_guard, should_force_final_turn};
use tasks::{
    DelegateTaskKind, DelegateTaskRequest, EXPLORE_CODE_TOOL, SEARCH_CODE_TOOL,
    VALIDATE_FINDING_TOOL,
};

use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::model::ConcurrentModelRouter;
use crate::reviewer_kernel::policy::ReviewerPolicy;
#[cfg(test)]
use crate::reviewer_kernel::review_contract::BudgetSource;
use crate::reviewer_kernel::review_contract::{AgentBudget, FileReviewV1, FindingV1, Role};
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::workspace::RepoSnapshot;

const ORCHESTRATOR_SESSION_ID: &str = "review-orchestrator";
const DEFAULT_MAX_CHILD_SESSIONS: usize = 32;

pub(crate) struct AutonomousReviewRuntime {
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) model_router: Arc<dyn ConcurrentModelRouter>,
    pub(crate) tools: Arc<ToolEngine>,
    pub(crate) policy: Arc<ReviewerPolicy>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) review_revision_id: String,
    pub(crate) events: RuntimeEventDispatcher,
    pub(crate) active_sessions: Arc<Semaphore>,
    pub(crate) delegate_host: AutonomousDelegateHost,
}

pub(crate) struct AutonomousReviewRunReport {
    pub(crate) metrics: ConcurrentRunReport,
    pub(crate) findings: Vec<FindingV1>,
    pub(crate) file_reviews: Vec<FileReviewV1>,
}

impl AutonomousReviewRuntime {
    pub(crate) async fn run_with_cancel(
        self: Arc<Self>,
        sessions: Vec<SessionScope>,
        cancel: CancellationToken,
    ) -> AutonomousReviewRunReport {
        let started = Instant::now();
        let state = Arc::new(AutonomousDelegateState::new(&self));
        let _guard = self
            .delegate_host
            .register(self.snapshot.snapshot_id.clone(), Arc::clone(&state));

        let template = sessions.into_iter().next();
        let scope = self.orchestrator_scope(template);
        let trace_scope = scope.clone();
        let report = run_session_loop(
            Arc::clone(&state),
            SessionRunConfig {
                scope,
                kind: SessionKind::Orchestrator,
                task_packet: None,
                response_format: orchestrator_response_format(),
                final_instruction: orchestrator_final_instruction(),
            },
            cancel.child_token(),
        )
        .await;
        let parsed = parse_orchestrator_output(report.output.as_deref());
        for (index, candidate) in parsed.candidates.iter().enumerate() {
            self.emit_candidate_lifecycle_trace(
                &trace_scope,
                "candidate_finding_emitted",
                format!("candidate {} emitted", candidate.id),
                json!({
                    "candidateId": candidate.id,
                    "index": index,
                    "title": truncate_chars(&candidate.title, 240),
                    "claim": truncate_chars(&candidate.claim, 600),
                    "severity": candidate.severity,
                    "path": candidate.path,
                    "startLine": candidate.start_line,
                    "endLine": candidate.end_line,
                    "behaviorBefore": candidate
                        .behavior_before
                        .as_deref()
                        .map(|value| truncate_chars(value, 600)),
                    "behaviorAfter": candidate
                        .behavior_after
                        .as_deref()
                        .map(|value| truncate_chars(value, 600)),
                    "candidateTextBytes": candidate.title.len()
                        + candidate.claim.len()
                        + candidate.behavior_before.as_deref().unwrap_or_default().len()
                        + candidate.behavior_after.as_deref().unwrap_or_default().len(),
                    "evidenceArtifactIds": candidate.evidence_artifact_ids,
                    "relatedPaths": candidate.related_paths,
                    "orchestratorStatus": report.status,
                    "orchestratorCompleted": report.completed,
                    "orchestratorToolCallsUsed": report.tool_calls_used,
                    "orchestratorMaxToolCalls": trace_scope.budget.max_tool_calls,
                    "orchestratorExhaustedToolBudget": report.exhausted_tool_budget,
                }),
            );
        }
        let validations = self
            .run_mandatory_validations(
                Arc::clone(&state),
                &trace_scope,
                &parsed.candidates,
                &cancel,
            )
            .await;

        let finding_outcome = build_findings(
            &self.tools,
            &self.snapshot,
            &self.review_revision_id,
            &parsed.candidates,
            &validations,
        );
        if parsed.candidates.is_empty() && report.exhausted_tool_budget && !report.completed {
            self.emit_candidate_lifecycle_trace(
                &trace_scope,
                "candidate_publication_skipped",
                "candidate publication skipped after orchestrator exhausted tool budget"
                    .to_string(),
                json!({
                    "reason": "orchestrator_exhausted_tool_budget",
                    "orchestratorStatus": report.status,
                    "orchestratorCompleted": report.completed,
                    "orchestratorToolCallsUsed": report.tool_calls_used,
                    "orchestratorMaxToolCalls": trace_scope.budget.max_tool_calls,
                    "orchestratorExhaustedToolBudget": report.exhausted_tool_budget,
                    "publicationSkippedBudgetExhausted": true,
                }),
            );
        }
        let findings = finding_outcome.findings;
        let rejection_reasons = finding_outcome.rejection_reasons;
        for decision in &finding_outcome.publication_decisions {
            let publication_skipped_budget_exhausted = decision.decision == "rejected"
                && decision.reason == "missing_validation"
                && report.exhausted_tool_budget;
            self.emit_candidate_lifecycle_trace(
                &trace_scope,
                "candidate_finding_decision",
                format!(
                    "candidate {} {}: {}",
                    decision.candidate_id, decision.decision, decision.reason
                ),
                json!({
                    "candidateId": decision.candidate_id,
                    "decision": decision.decision,
                    "reason": decision.reason,
                    "phase": "publication",
                    "validatorStatus": decision.validator_status,
                    "validatorSessionId": decision.validator_session_id,
                    "orchestratorStatus": report.status,
                    "orchestratorCompleted": report.completed,
                    "orchestratorToolCallsUsed": report.tool_calls_used,
                    "orchestratorMaxToolCalls": trace_scope.budget.max_tool_calls,
                    "orchestratorExhaustedToolBudget": report.exhausted_tool_budget,
                    "publicationSkippedBudgetExhausted": publication_skipped_budget_exhausted,
                }),
            );
        }
        for finding in &findings {
            let tool_call_id = ToolCallId(format!("{}-autonomous-review", finding.id));
            self.events.emit_runtime_with_context(
                RuntimeEventContext {
                    session_id: Some(SessionId(ORCHESTRATOR_SESSION_ID.to_string())),
                    tool_call_id: Some(tool_call_id.clone()),
                    finding_id: Some(finding.id.clone()),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::FindingRecorded {
                    finding_id: finding.id.clone(),
                    session_id: SessionId(ORCHESTRATOR_SESSION_ID.to_string()),
                    tool_call_id,
                },
            );
        }
        let file_reviews = build_file_reviews(
            &self.snapshot,
            &parsed,
            &findings,
            report.output.as_deref().unwrap_or_default(),
        );
        let mut all_reports = Vec::new();
        all_reports.push(report);
        all_reports.extend(state.child_reports());
        let metrics = build_run_metrics(
            "autonomous_review",
            started,
            &self.tools,
            &self.snapshot.snapshot_id,
            &all_reports,
            findings.len(),
            parsed.candidates.len(),
            rejection_reasons,
            parsed.notes.len(),
            parsed.verdict.as_str(),
        );
        AutonomousReviewRunReport {
            metrics,
            findings,
            file_reviews,
        }
    }

    fn orchestrator_scope(&self, template: Option<SessionScope>) -> SessionScope {
        let mut scope = template.unwrap_or_else(|| {
            SessionScope::review_read_only(
                SessionId(ORCHESTRATOR_SESSION_ID.to_string()),
                Role::Generalist,
                "Autonomously review the changed code.",
                AgentBudget::planned_baseline(),
            )
        });
        scope.id = SessionId(ORCHESTRATOR_SESSION_ID.to_string());
        scope.role = Role::Generalist;
        scope.objective = "You are Muzen's autonomous review orchestrator. Review the diff as a senior code reviewer. Use direct read-only tools for decisive evidence. Batch independent search_code, explore_code, and validate_finding delegate calls when separate investigations can run in parallel. Publish no finding unless it is supported by raw code or diff evidence. Infer importance from the raw diff and repository context.".to_string();
        scope.snapshot_id = Some(self.snapshot.snapshot_id.clone());
        scope.budget = autonomous_orchestrator_budget(
            scope.budget,
            self.snapshot.manifest.changed_file_entries.len(),
        );
        if let Some(model_profile_id) = self.limits.orchestrator_model_profile_id.clone() {
            scope.model_profile_id = Some(model_profile_id);
        }
        scope.response_format = None;
        for tool in [SEARCH_CODE_TOOL, EXPLORE_CODE_TOOL, VALIDATE_FINDING_TOOL] {
            if let Ok(tool_id) = ToolId::parse(tool) {
                scope
                    .capabilities
                    .grant_tool(tool_id, ToolGrant::allow_review_read_only());
            }
        }
        let starter = neutral_starter_context(&self.snapshot, &scope.instructions);
        scope.instructions = vec![SessionInstruction {
            kind: "neutral_starter_context".to_string(),
            trusted: true,
            text: starter,
        }];
        scope
    }

    async fn run_mandatory_validations(
        &self,
        state: Arc<AutonomousDelegateState>,
        trace_scope: &SessionScope,
        candidates: &[CandidateFinding],
        cancel: &CancellationToken,
    ) -> Vec<ValidationPacket> {
        let mut validations = Vec::new();
        for candidate in candidates {
            if cancel.is_cancelled() {
                break;
            }
            self.emit_candidate_lifecycle_trace(
                trace_scope,
                "candidate_validation_started",
                format!("validation started for candidate {}", candidate.id),
                json!({
                    "candidateId": candidate.id,
                    "path": candidate.path,
                }),
            );
            let task = DelegateTaskRequest {
                objective: format!("Validate candidate finding {}", candidate.id),
                prompt: serde_json::to_string(candidate)
                    .unwrap_or_else(|_| candidate.claim.clone()),
                candidate: Some(serde_json::to_value(candidate).unwrap_or(Value::Null)),
            };
            let validation = match run_child_delegate(
                Arc::clone(&state),
                DelegateTaskKind::ValidateFinding,
                task,
                cancel.child_token(),
            )
            .await
            {
                Ok(packet) => ValidationPacket {
                    candidate_id: candidate.id.clone(),
                    status: packet.status,
                    summary: packet.summary,
                    artifact_id: packet.artifact_id,
                    child_session_id: Some(packet.session_id),
                },
                Err(error) => ValidationPacket {
                    candidate_id: candidate.id.clone(),
                    status: "insufficient".to_string(),
                    summary: format!("validation failed: {error}"),
                    artifact_id: None,
                    child_session_id: None,
                },
            };
            self.emit_candidate_lifecycle_trace(
                trace_scope,
                "candidate_validation_completed",
                format!(
                    "validation completed for candidate {}: {}",
                    candidate.id, validation.status
                ),
                json!({
                    "candidateId": candidate.id,
                    "status": validation.status,
                    "validatorSessionId": validation.child_session_id,
                    "artifactId": validation.artifact_id.as_ref().map(|artifact_id| artifact_id.0.clone()),
                    "summaryBytes": validation.summary.len(),
                }),
            );
            validations.push(validation);
        }
        validations
    }

    fn emit_candidate_lifecycle_trace(
        &self,
        scope: &SessionScope,
        trace_kind: &'static str,
        summary: String,
        details: Value,
    ) {
        self.events.emit_planned_runtime(
            self.policy
                .plan_agent_trace_event(scope, None, trace_kind, summary, details),
        );
    }
}

#[cfg(test)]
mod tests;
