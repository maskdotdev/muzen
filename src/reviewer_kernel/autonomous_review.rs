use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

mod delegates;
mod diff_risk;
mod finding_filters;
mod findings;
mod prompts;
mod schemas;
mod sessions;
mod tasks;

pub(crate) use delegates::{register_autonomous_delegate_tools, AutonomousDelegateHost};
use delegates::{run_child_delegate, AutonomousDelegateState};
#[cfg(test)]
use diff_risk::diff_risk_inventory;
#[cfg(test)]
use finding_filters::autonomous_candidate_rejection_reason;
use findings::{
    build_file_reviews, build_findings, parse_orchestrator_output, CandidateFinding,
    ValidationPacket,
};
use prompts::neutral_starter_context;
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

use crate::reviewer_kernel::agent_loop::AgentLoopReport;
use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::model::ConcurrentModelRouter;
use crate::reviewer_kernel::policy::ReviewerPolicy;
#[cfg(test)]
use crate::reviewer_kernel::review_contract::BudgetSource;
use crate::reviewer_kernel::review_contract::{
    AgentBudget, FileReviewV1, FindingV1, Role, TokenUsage, ToolCounts,
};
use crate::reviewer_kernel::session_metrics::add_model_metrics;
use crate::reviewer_kernel::session_metrics::elapsed_ms;
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
        let child_reports = state.child_reports();
        let parsed = parse_orchestrator_output(report.output.as_deref());
        let validations = self
            .run_mandatory_validations(Arc::clone(&state), &parsed.candidates, &cancel)
            .await;
        let validation_reports = state.child_reports();

        let finding_outcome = build_findings(
            &self.tools,
            &self.snapshot,
            &self.review_revision_id,
            &parsed.candidates,
            &validations,
        );
        let findings = finding_outcome.findings;
        let rejection_reasons = finding_outcome.rejection_reasons;
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
        all_reports.extend(child_reports);
        for validation in validation_reports {
            if !all_reports
                .iter()
                .any(|existing| existing.session_id == validation.session_id)
            {
                all_reports.push(validation);
            }
        }
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
        candidates: &[CandidateFinding],
        cancel: &CancellationToken,
    ) -> Vec<ValidationPacket> {
        let mut validations = Vec::new();
        for candidate in candidates {
            if cancel.is_cancelled() {
                break;
            }
            let task = DelegateTaskRequest {
                objective: format!("Validate candidate finding {}", candidate.id),
                prompt: serde_json::to_string(candidate)
                    .unwrap_or_else(|_| candidate.claim.clone()),
                candidate: Some(serde_json::to_value(candidate).unwrap_or(Value::Null)),
            };
            match run_child_delegate(
                Arc::clone(&state),
                DelegateTaskKind::ValidateFinding,
                task,
                cancel.child_token(),
            )
            .await
            {
                Ok(packet) => validations.push(ValidationPacket {
                    candidate_id: candidate.id.clone(),
                    status: packet.status,
                    summary: packet.summary,
                    artifact_id: packet.artifact_id,
                    child_session_id: Some(packet.session_id),
                }),
                Err(error) => validations.push(ValidationPacket {
                    candidate_id: candidate.id.clone(),
                    status: "insufficient".to_string(),
                    summary: format!("validation failed: {error}"),
                    artifact_id: None,
                    child_session_id: None,
                }),
            }
        }
        validations
    }
}

fn build_run_metrics(
    runtime_name: &'static str,
    started: Instant,
    tools: &ToolEngine,
    snapshot_id: &SnapshotId,
    reports: &[AgentLoopReport],
    findings: usize,
    candidate_count: usize,
    rejection_reasons: BTreeMap<String, usize>,
    note_count: usize,
    verdict: &str,
) -> ConcurrentRunReport {
    let mut completed_sessions = 0usize;
    let mut model_calls = 0usize;
    let mut model_metrics = ModelMetricsSnapshot::default();
    let mut tool_counts = ToolCounts::default();
    let mut tokens = TokenUsage::default();
    let mut diagnostics = Vec::new();
    for report in reports {
        if report.completed {
            completed_sessions += 1;
        }
        model_calls += report.model_calls;
        add_model_metrics(&mut model_metrics, &report.model_metrics);
        tool_counts.add(report.tool_counts);
        tokens.add(report.tokens);
        diagnostics.push(report.diagnostic.clone());
    }
    diagnostics.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    let (artifacts, artifact_bytes) = tools.artifact_stats();
    let elapsed_ms = elapsed_ms(started);
    let sessions = reports.len();
    let mut quality_diagnostics = ReviewQualityDiagnostics::default();
    quality_diagnostics.sessions_run = sessions;
    quality_diagnostics.candidate_findings = candidate_count;
    quality_diagnostics.rejected_candidates = rejection_reasons.values().sum();
    quality_diagnostics.rejection_reasons = rejection_reasons;
    quality_diagnostics
        .rejection_reasons
        .insert(format!("verdict:{verdict}"), note_count);
    ConcurrentRunReport {
        runtime: runtime_name,
        sessions,
        completed_sessions,
        model_calls,
        tool_calls: tool_counts.total(),
        tool_counts,
        findings,
        publishable_findings: findings,
        quality_diagnostics,
        elapsed_ms,
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        total_tokens: tokens.total_tokens,
        cached_input_tokens: tokens.cached_input_tokens,
        artifacts,
        artifact_bytes,
        counters: tools.snapshot_counters(),
        tool_metrics: tools.snapshot_tool_metrics(),
        provider_health: tools.snapshot_provider_health(),
        snapshot_metrics: vec![SnapshotMetricsSnapshot {
            snapshot_id: snapshot_id.clone(),
            sessions,
            completed_sessions,
            model_calls,
            tool_calls: tool_counts.total(),
            artifacts,
            artifact_bytes,
            elapsed_ms,
        }],
        model_metrics,
        completion_diagnostics: diagnostics,
    }
}

#[cfg(test)]
mod tests;
