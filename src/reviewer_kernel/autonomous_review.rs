use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
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

use delegates::{
    child_budget, lead_generation_child_budget, run_child_delegate_with_budget,
    AutonomousDelegateState, ChildCandidateDiscovery,
};
pub(crate) use delegates::{register_autonomous_delegate_tools, AutonomousDelegateHost};
use diff_risk::{diff_risk_inventory, truncate_chars, DiffRiskEntry};
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

use crate::reviewer_kernel::agent_loop::AgentLoopConfig;
use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::model::ConcurrentModelRouter;
use crate::reviewer_kernel::policy::ReviewerPolicy;
use crate::reviewer_kernel::report::SessionOutput;
#[cfg(test)]
use crate::reviewer_kernel::review_contract::BudgetSource;
use crate::reviewer_kernel::review_contract::{AgentBudget, FileReviewV1, FindingV1, Role};
use crate::reviewer_kernel::spec::RunMode;
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::workspace::RepoSnapshot;

const ORCHESTRATOR_SESSION_ID: &str = "review-orchestrator";
const DEFAULT_MAX_CHILD_SESSIONS: usize = 16;
const MAX_LEAD_GENERATION_ENTRIES: usize = 3;
const MAX_MANDATORY_VALIDATIONS_PER_REVIEW: usize = 6;

pub(crate) struct AutonomousReviewRuntime {
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) model_router: Arc<dyn ConcurrentModelRouter>,
    pub(crate) tools: Arc<ToolEngine>,
    pub(crate) policy: Arc<ReviewerPolicy>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) run_mode: RunMode,
    pub(crate) review_revision_id: String,
    pub(crate) events: RuntimeEventDispatcher,
    pub(crate) active_sessions: Arc<Semaphore>,
    pub(crate) delegate_host: AutonomousDelegateHost,
}

pub(crate) struct AutonomousReviewRunReport {
    pub(crate) metrics: ConcurrentRunReport,
    pub(crate) findings: Vec<FindingV1>,
    pub(crate) file_reviews: Vec<FileReviewV1>,
    pub(crate) session_outputs: Vec<SessionOutput>,
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

        if self.run_mode == RunMode::DirectSessions {
            return self
                .run_direct_sessions(state, sessions, started, cancel)
                .await;
        }

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
        let risk_entries = diff_risk_inventory(&self.snapshot.diff.content, 40);
        self.run_lead_generation(
            Arc::clone(&state),
            &trace_scope,
            &parsed,
            &risk_entries,
            &cancel,
        )
        .await;
        let child_discoveries = state.child_candidate_discoveries();
        let mut candidates = merged_candidate_findings(&parsed.candidates, &child_discoveries);
        let seed_candidates = risk_seed_candidate_findings(&risk_entries, &candidates, 2);
        for candidate in &seed_candidates {
            self.emit_candidate_lifecycle_trace(
                &trace_scope,
                "risk_seed_candidate_created",
                format!("risk seed candidate {} created", candidate.id),
                json!({
                    "candidateId": candidate.id,
                    "title": truncate_chars(&candidate.title, 240),
                    "claim": truncate_chars(&candidate.claim, 600),
                    "path": candidate.path,
                    "startLine": candidate.start_line,
                    "endLine": candidate.end_line,
                }),
            );
        }
        candidates.extend(seed_candidates);
        for discovery in &child_discoveries {
            self.emit_candidate_lifecycle_trace(
                &trace_scope,
                "child_candidate_discovered",
                format!(
                    "{} discovered candidate {}",
                    discovery.task_type, discovery.candidate.id
                ),
                json!({
                    "candidateId": discovery.candidate.id,
                    "taskType": discovery.task_type,
                    "childSessionId": discovery.child_session_id,
                    "artifactId": discovery.artifact_id.as_ref().map(|artifact_id| artifact_id.0.clone()),
                    "title": truncate_chars(&discovery.candidate.title, 240),
                    "claim": truncate_chars(&discovery.candidate.claim, 600),
                    "path": discovery.candidate.path,
                    "startLine": discovery.candidate.start_line,
                    "endLine": discovery.candidate.end_line,
                }),
            );
        }
        for (index, candidate) in candidates.iter().enumerate() {
            self.emit_candidate_lifecycle_trace(
                &trace_scope,
                "candidate_finding_emitted",
                format!("candidate {} emitted", candidate.id),
                json!({
                    "candidateId": candidate.id,
                    "index": index,
                    "title": truncate_chars(&candidate.title, 240),
                    "claim": truncate_chars(&candidate.claim, 600),
                    "negativeOutcome": truncate_chars(&candidate.negative_outcome, 600),
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
                        + candidate.negative_outcome.len()
                        + candidate.behavior_before.as_deref().unwrap_or_default().len()
                        + candidate.behavior_after.as_deref().unwrap_or_default().len(),
                    "evidenceArtifactIds": candidate.evidence_artifact_ids,
                    "relatedPaths": candidate.related_paths,
                    "orchestratorCandidateCount": parsed.candidates.len(),
                    "childCandidateCount": child_discoveries.len(),
                    "mergedCandidateCount": candidates.len(),
                    "orchestratorStatus": report.status,
                    "orchestratorCompleted": report.completed,
                    "orchestratorToolCallsUsed": report.tool_calls_used,
                    "orchestratorMaxToolCalls": trace_scope.budget.max_tool_calls,
                    "orchestratorExhaustedToolBudget": report.exhausted_tool_budget,
                }),
            );
        }
        let validation_outcome = self
            .run_mandatory_validations(Arc::clone(&state), &trace_scope, &candidates, &cancel)
            .await;

        let finding_outcome = build_findings(
            &self.tools,
            &self.snapshot,
            &self.review_revision_id,
            &candidates,
            &validation_outcome.validations,
        );
        if candidates.is_empty() && report.exhausted_tool_budget && !report.completed {
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
        let mut session_outputs = vec![SessionOutput {
            session_id: trace_scope.id.0.clone(),
            status: report.status.clone(),
            completed: report.completed,
            output: report.output.clone(),
        }];
        let mut all_reports = Vec::new();
        all_reports.push(report);
        all_reports.extend(state.child_reports());
        session_outputs.extend(all_reports.iter().skip(1).map(|report| SessionOutput {
            session_id: report.diagnostic.session_id.clone(),
            status: report.status.clone(),
            completed: report.completed,
            output: report.output.clone(),
        }));
        let metrics = build_run_metrics(
            "autonomous_review",
            started,
            &self.tools,
            &self.snapshot.snapshot_id,
            &all_reports,
            findings.len(),
            parsed.candidates.len(),
            child_discoveries.len(),
            candidates.len(),
            validation_outcome.rescue_attempts,
            validation_outcome.rescue_supported,
            rejection_reasons,
            parsed.notes.len(),
            parsed.verdict.as_str(),
        );
        AutonomousReviewRunReport {
            metrics,
            findings,
            file_reviews,
            session_outputs,
        }
    }

    async fn run_direct_sessions(
        &self,
        state: Arc<AutonomousDelegateState>,
        sessions: Vec<SessionScope>,
        started: Instant,
        cancel: CancellationToken,
    ) -> AutonomousReviewRunReport {
        let mut joins = JoinSet::new();
        for (index, mut scope) in sessions.into_iter().enumerate() {
            scope.snapshot_id = Some(self.snapshot.snapshot_id.clone());
            let state = Arc::clone(&state);
            let cancel = cancel.child_token();
            joins.spawn(async move {
                let _permit = state.active_sessions.clone().acquire_owned().await.ok();
                let report = state
                    .agent_loop_runtime()
                    .run_session_loop(direct_session_config(scope.clone()), cancel)
                    .await;
                (index, scope, report)
            });
        }

        let mut reports = Vec::new();
        while let Some(result) = joins.join_next().await {
            if let Ok((index, scope, report)) = result {
                reports.push((index, scope, report));
            }
        }
        reports.sort_by_key(|(index, _, _)| *index);
        let session_outputs = reports
            .iter()
            .map(|(_, scope, report)| SessionOutput {
                session_id: scope.id.0.clone(),
                status: report.status.clone(),
                completed: report.completed,
                output: report.output.clone(),
            })
            .collect::<Vec<_>>();
        let agent_reports = reports
            .into_iter()
            .map(|(_, _, report)| report)
            .collect::<Vec<_>>();
        let metrics = build_run_metrics(
            "direct_sessions",
            started,
            &self.tools,
            &self.snapshot.snapshot_id,
            &agent_reports,
            0,
            0,
            0,
            0,
            0,
            0,
            BTreeMap::new(),
            0,
            "direct_sessions",
        );
        AutonomousReviewRunReport {
            metrics,
            findings: Vec::new(),
            file_reviews: Vec::new(),
            session_outputs,
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
    ) -> MandatoryValidationOutcome {
        let mut validations = Vec::new();
        let mut rescue_attempts = 0usize;
        let mut rescue_supported = 0usize;
        let selected_candidate_indexes =
            select_validation_candidate_indexes(candidates, MAX_MANDATORY_VALIDATIONS_PER_REVIEW);
        for (index, candidate) in candidates.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            if !selected_candidate_indexes.contains(&index) {
                self.emit_candidate_lifecycle_trace(
                    trace_scope,
                    "candidate_validation_skipped",
                    format!("validation skipped for candidate {}", candidate.id),
                    json!({
                        "candidateId": candidate.id,
                        "path": candidate.path,
                        "startLine": candidate.start_line,
                        "endLine": candidate.end_line,
                        "reason": "validation_budget_exhausted",
                        "candidateIndex": index,
                        "maxMandatoryValidations": MAX_MANDATORY_VALIDATIONS_PER_REVIEW,
                        "candidateCount": candidates.len(),
                    }),
                );
                continue;
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
            let (mut validation, validation_context) = match run_child_delegate_with_budget(
                Arc::clone(&state),
                DelegateTaskKind::ValidateFinding,
                task,
                validation_budget_for_candidate(candidate),
                cancel.child_token(),
            )
            .await
            {
                Ok(packet) => {
                    let context = packet.compact.clone();
                    (
                        ValidationPacket {
                            candidate_id: candidate.id.clone(),
                            status: packet.status,
                            summary: packet.summary,
                            artifact_id: packet.artifact_id,
                            child_session_id: Some(packet.session_id),
                        },
                        context,
                    )
                }
                Err(error) => {
                    let summary = format!("validation failed: {error}");
                    (
                        ValidationPacket {
                            candidate_id: candidate.id.clone(),
                            status: "insufficient".to_string(),
                            summary: summary.clone(),
                            artifact_id: None,
                            child_session_id: None,
                        },
                        json!({
                            "status": "insufficient",
                            "summary": summary,
                            "openQuestions": [],
                            "suggestedNextSearches": [],
                            "evidence": [],
                        }),
                    )
                }
            };
            if validation_status_needs_rescue_for_candidate(&validation.status, candidate)
                && !cancel.is_cancelled()
            {
                rescue_attempts += 1;
                self.emit_candidate_lifecycle_trace(
                    trace_scope,
                    "candidate_validation_rescue_started",
                    format!("validation rescue started for candidate {}", candidate.id),
                    json!({
                        "candidateId": candidate.id,
                        "previousValidatorSessionId": validation.child_session_id,
                        "previousArtifactId": validation.artifact_id.as_ref().map(|artifact_id| artifact_id.0.clone()),
                        "previousSummary": truncate_chars(&validation.summary, 800),
                    }),
                );
                let rescue_task = DelegateTaskRequest {
                    objective: format!(
                        "Resolve missing evidence for candidate finding {}",
                        candidate.id
                    ),
                    prompt: serde_json::to_string(&json!({
                        "candidate": candidate,
                        "previousValidation": {
                            "status": validation.status.clone(),
                            "summary": validation.summary.clone(),
                            "artifactId": validation.artifact_id.as_ref().map(|artifact_id| artifact_id.0.clone()),
                            "childSessionId": validation.child_session_id.clone(),
                            "compact": validation_context.clone(),
                        },
                        "instruction": "Inspect only the missing evidence needed to decide whether this exact changed-code candidate is supported. Start from previousValidation.compact.openQuestions and previousValidation.compact.suggestedNextSearches when present; do not repeat reads that previousValidation.compact.evidence already summarizes unless the body range itself is the missing proof. Return supported only when raw code or diff evidence establishes the concrete negative outcome stated by candidate.claim and candidate.negativeOutcome. If the evidence supports only a sibling, inverse, broader, narrower, or adjacent issue, return refuted or insufficient for this candidate rather than rewriting it into the neighboring issue. Return review_concern only for diagnostic notes where the concern is actionable but the exact failure is still unproven; review_concern is not publishable. For localized resource language/script candidates, if the candidate points at a changed localized resource line and raw current-line evidence shows a value in the wrong language or script for that locale, support the localized mismatch without requiring a base-file before value; publication separately verifies the line is changed. For Optional.get()/unwrap candidates, treat a changed producer, changed lookup order, changed data source, or changed absence condition as changed behavior even when the consumer unwrap existed before; support it if the changed path can now return empty without a dominating presence check. If previous validation found connected raw List.class/Map.class, unchecked cast, or erased collection-shape evidence for the same domain value, either fold that evidence into one supported candidate with the unwrap outcome or cite raw code proving the shape is validated or unrelated. For persisted identity candidates, spend the first targeted reads/searches on the id contract and one downstream consumer: constructor/accessor semantics for the id field, update/remove/lookup/audit delegation, and storage implementations that compare or persist that id. Support the candidate when raw evidence shows the changed reconstruction/creation path can pass a missing, blank, or wrong identity into a later operation that depends on that identity; a later id-based remove/update/lookup contract is sufficient even if the exact production data instance is not replayed. If previous validation identified a missing downstream identity consumer, read that consumer body or the named storage method before finalizing. If the previous validation was review_concern for such an identity contract, resolve that concern by either supporting the id-contract failure or citing the exact raw code that makes the blank/missing id impossible or irrelevant. For misspelled identifier candidates, support a low-severity finding when raw code shows a changed misspelled method/type/field identifier and adjacent naming, comments, or conventional spelling prove the intended term; maintenance/search/discoverability confusion is a concrete low-severity outcome. For documentation-contract candidates, if the previous result has suggested reads for implementations or validators, read those before refuting; support when public API documentation, Javadoc, schema text, examples, or generated docs contradict built-in implementations or executable behavior in a way that would guide callers or implementers incorrectly. Treat inline TODO/comment cleanup observations as notes unless they prove a concrete changed-code failure beyond the comment itself. Otherwise return insufficient or refuted. Do not include negative-evidence disclaimers such as \"I did not find...\" in supported candidate claims."
                    }))
                    .unwrap_or_else(|_| candidate.claim.clone()),
                    candidate: Some(serde_json::to_value(candidate).unwrap_or(Value::Null)),
                };
                let rescue = match run_child_delegate_with_budget(
                    Arc::clone(&state),
                    DelegateTaskKind::ValidateFinding,
                    rescue_task,
                    validation_budget_for_candidate(candidate),
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
                        summary: format!("validation rescue failed: {error}"),
                        artifact_id: None,
                        child_session_id: None,
                    },
                };
                self.emit_candidate_lifecycle_trace(
                    trace_scope,
                    "candidate_validation_rescue_completed",
                    format!(
                        "validation rescue completed for candidate {}: {}",
                        candidate.id, rescue.status
                    ),
                    json!({
                        "candidateId": candidate.id,
                        "status": rescue.status,
                        "validatorSessionId": rescue.child_session_id,
                        "artifactId": rescue.artifact_id.as_ref().map(|artifact_id| artifact_id.0.clone()),
                        "summaryBytes": rescue.summary.len(),
                        "rescuedToSupported": rescue.status.trim().eq_ignore_ascii_case("supported"),
                    }),
                );
                if rescue.status.trim().eq_ignore_ascii_case("supported") {
                    rescue_supported += 1;
                }
                validation = rescue;
            }
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
        MandatoryValidationOutcome {
            validations,
            rescue_attempts,
            rescue_supported,
        }
    }

    async fn run_lead_generation(
        &self,
        state: Arc<AutonomousDelegateState>,
        trace_scope: &SessionScope,
        parsed: &findings::ParsedOrchestratorOutput,
        risk_entries: &[DiffRiskEntry],
        cancel: &CancellationToken,
    ) {
        let selected = select_lead_generation_entries(
            risk_entries,
            &parsed.candidates,
            &parsed.completeness,
            MAX_LEAD_GENERATION_ENTRIES,
        );
        for entry in selected {
            if cancel.is_cancelled() {
                break;
            }
            self.emit_candidate_lifecycle_trace(
                trace_scope,
                "lead_generation_started",
                format!("lead generation started for {}", entry.id),
                json!({
                    "riskId": entry.id,
                    "path": entry.path,
                    "line": entry.line,
                    "category": entry.category,
                    "code": truncate_chars(&entry.code, 240),
                }),
            );
            let task = DelegateTaskRequest {
                objective: format!("Investigate review lead {}", entry.id),
                prompt: serde_json::to_string(&json!({
                    "riskEntry": {
                        "id": entry.id,
                        "path": entry.path,
                        "line": entry.line,
                        "category": entry.category,
                        "code": entry.code,
                        "obligation": entry.obligation,
                    },
                    "orchestratorSummary": truncate_chars(&parsed.summary, 1200),
                    "categoryInstruction": lead_generation_category_instruction(entry.category),
                    "instruction": "Inspect this risk entry as a candidate-finding lead, not a full review. Use at most two targeted evidence reads/searches before finalizing; prefer precise file/range reads over broad repository searches. Anchor any candidate's primary location to riskEntry.path unless raw evidence proves the same changed behavior moved to another changed path. For localized resource changes, stay on the same message key/resource family shown in riskEntry.code; do not return unrelated localization issues found while searching. Verify prerequisite evidence before claiming missing companion files, missing callers, or changed build failures. Use raw diff/code evidence and related callers/tests when relevant. If the lead is supported as a review issue, put the issue in candidateFindings; do not mark status=supported while leaving candidateFindings empty. Return empty candidateFindings only with status=refuted, insufficient, or needs_more_evidence and include the refuting evidence or open question."
                }))
                .unwrap_or_else(|_| entry.obligation.to_string()),
                candidate: None,
            };
            let mut result = run_child_delegate_with_budget(
                Arc::clone(&state),
                DelegateTaskKind::ExploreCode,
                task.clone(),
                lead_generation_budget_for_category(entry.category),
                cancel.child_token(),
            )
            .await;
            if let Ok(packet) = &result {
                if let Some(retry_kind) = lead_generation_retry_kind(
                    packet.candidate_count,
                    &packet.status,
                    packet.completed,
                    &packet.finalization_reason,
                    packet.schema_validation_success,
                    entry.category,
                )
                .filter(|_| !cancel.is_cancelled())
                {
                    self.emit_candidate_lifecycle_trace(
                        trace_scope,
                        "lead_generation_retry_started",
                        format!("lead generation retry started for {}", entry.id),
                        json!({
                            "riskId": entry.id,
                            "previousChildSessionId": packet.session_id,
                            "previousArtifactId": packet.artifact_id.as_ref().map(|artifact_id| artifact_id.0.clone()),
                            "previousStatus": packet.status,
                            "previousCompleted": packet.completed,
                            "previousFinalizationReason": packet.finalization_reason,
                            "previousSchemaValidationSuccess": packet.schema_validation_success,
                            "retryKind": retry_kind,
                        }),
                    );
                    let retry_task = if retry_kind == "missing_evidence" {
                        DelegateTaskRequest {
                            objective: format!("Resolve review lead evidence {}", entry.id),
                            prompt: serde_json::to_string(&json!({
                                "riskEntry": {
                                    "id": entry.id,
                                    "path": entry.path,
                                    "line": entry.line,
                                    "category": entry.category,
                                    "code": entry.code,
                                    "obligation": entry.obligation,
                                },
                                "previousLeadResult": packet.compact,
                                "categoryInstruction": lead_generation_category_instruction(entry.category),
                                "instruction": "Resolve only the previous lead's open questions and suggested next searches. Read the missing raw code/diff evidence needed to decide the lead. If the additional evidence establishes an actionable changed-code issue with a concrete negative outcome, emit it in candidateFindings. If raw evidence proves a dominating precondition, type-safety guarantee, or other refutation, return refuted with that evidence. Do not finish with needs_more_evidence again unless the needed evidence is outside the available repository snapshot."
                            }))
                            .unwrap_or_else(|_| entry.obligation.to_string()),
                            candidate: None,
                        }
                    } else {
                        task.clone()
                    };
                    result = run_child_delegate_with_budget(
                        Arc::clone(&state),
                        DelegateTaskKind::ExploreCode,
                        retry_task,
                        lead_generation_budget_for_category(entry.category),
                        cancel.child_token(),
                    )
                    .await;
                }
            }
            let (status, session_id, artifact_id, candidate_count) = match result {
                Ok(packet) => (
                    packet.status,
                    Some(packet.session_id),
                    packet.artifact_id.map(|artifact_id| artifact_id.0),
                    packet.candidate_count,
                ),
                Err(error) => (format!("error: {error}"), None, None, 0),
            };
            let supported_without_candidate =
                status.trim().eq_ignore_ascii_case("supported") && candidate_count == 0;
            self.emit_candidate_lifecycle_trace(
                trace_scope,
                "lead_generation_completed",
                format!("lead generation completed for {}", entry.id),
                json!({
                    "riskId": entry.id,
                    "status": status,
                    "childSessionId": session_id,
                    "artifactId": artifact_id,
                    "candidateCount": candidate_count,
                    "supportedWithoutCandidate": supported_without_candidate,
                }),
            );
        }
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

struct MandatoryValidationOutcome {
    validations: Vec<ValidationPacket>,
    rescue_attempts: usize,
    rescue_supported: usize,
}

fn select_validation_candidate_indexes(
    candidates: &[CandidateFinding],
    max_validations: usize,
) -> BTreeSet<usize> {
    if max_validations == 0 {
        return BTreeSet::new();
    }
    let mut ranked = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate_validation_priority(candidate), index))
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(priority, index)| (*priority, *index));
    ranked
        .into_iter()
        .take(max_validations)
        .map(|(_, index)| index)
        .collect()
}

fn candidate_validation_priority(candidate: &CandidateFinding) -> u8 {
    if candidate_is_high_signal_optional_unwrap(candidate)
        || candidate_is_high_signal_persisted_identity(candidate)
    {
        return 0;
    }

    let text = candidate_validation_text(candidate);
    if candidate_text_contains_any(
        &text,
        &[
            "regex",
            "matcher",
            "group",
            "localized",
            "translation",
            "language",
            "script",
            "documentation",
            "contract",
            "shortcut",
        ],
    ) {
        return 1;
    }

    if !candidate.id.starts_with("child_") && !candidate.id.starts_with("risk_seed_") {
        return 2;
    }

    if !candidate.evidence_artifact_ids.is_empty() {
        return 3;
    }

    4
}

fn candidate_validation_text(candidate: &CandidateFinding) -> String {
    format!(
        "{} {} {} {} {}",
        candidate.title,
        candidate.claim,
        candidate.negative_outcome,
        candidate.behavior_before.as_deref().unwrap_or_default(),
        candidate.behavior_after.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase()
}

fn candidate_text_contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn validation_status_needs_rescue_for_candidate(
    status: &str,
    candidate: &CandidateFinding,
) -> bool {
    let normalized_status = status.trim().to_ascii_lowercase();
    normalized_status == "needs_more_evidence"
        || normalized_status == "review_concern"
            && candidate_is_high_signal_persisted_identity(candidate)
        || normalized_status == "insufficient"
            && (candidate_is_high_signal_optional_unwrap(candidate)
                || candidate_is_high_signal_persisted_identity(candidate)
                || candidate_is_high_signal_spelling(candidate))
}

fn validation_budget_for_candidate(candidate: &CandidateFinding) -> AgentBudget {
    let mut budget = child_budget(DelegateTaskKind::ValidateFinding);
    if candidate_is_high_signal_persisted_identity(candidate)
        || candidate_is_high_signal_optional_unwrap(candidate)
    {
        budget.max_turns = budget.max_turns.max(6);
        budget.max_tool_calls = budget.max_tool_calls.max(12);
    }
    budget
}

fn candidate_is_high_signal_optional_unwrap(candidate: &CandidateFinding) -> bool {
    let text = format!(
        "{} {} {} {}",
        candidate.title,
        candidate.claim,
        candidate.negative_outcome,
        candidate.behavior_after.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let unwrap_signal = text.contains("optional")
        || text.contains("unwrap")
        || text.contains("unchecked")
        || text.contains("nosuchelement")
        || text.contains(".get()");
    let absence_signal = text.contains("empty")
        || text.contains("absence")
        || text.contains("presence")
        || text.contains("without proving")
        || text.contains("without checking")
        || text.contains("without a guard");
    unwrap_signal && absence_signal
}

fn candidate_is_high_signal_persisted_identity(candidate: &CandidateFinding) -> bool {
    let text = format!(
        "{} {} {} {}",
        candidate.title,
        candidate.claim,
        candidate.negative_outcome,
        candidate.behavior_after.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let identity_signal = text.contains("id")
        || text.contains("identifier")
        || text.contains("identity")
        || text.contains("key");
    let propagation_signal = text.contains("preserve")
        || text.contains("copy")
        || text.contains("missing")
        || text.contains("blank")
        || text.contains("null")
        || text.contains("wrong")
        || text.contains("not set");
    let downstream_signal = text.contains("remove")
        || text.contains("update")
        || text.contains("lookup")
        || text.contains("persist")
        || text.contains("stored")
        || text.contains("stale")
        || text.contains("target");
    identity_signal && propagation_signal && downstream_signal
}

fn candidate_is_high_signal_spelling(candidate: &CandidateFinding) -> bool {
    let text = format!(
        "{} {} {} {}",
        candidate.title,
        candidate.claim,
        candidate.negative_outcome,
        candidate.behavior_after.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let spelling_signal = text.contains("misspell")
        || text.contains("spelling")
        || text.contains("typo")
        || text.contains("missing letter")
        || text.contains("method name")
        || text.contains("identifier");
    let maintenance_signal = text.contains("maintenance")
        || text.contains("search")
        || text.contains("discoverability")
        || text.contains("confusing")
        || text.contains("future caller")
        || text.contains("future call");
    spelling_signal && maintenance_signal
}

fn lead_generation_retry_kind(
    candidate_count: usize,
    status: &str,
    completed: bool,
    finalization_reason: &str,
    schema_validation_success: bool,
    category: &str,
) -> Option<&'static str> {
    if candidate_count != 0 {
        return None;
    }
    if !completed
        || finalization_reason.eq_ignore_ascii_case("model_failed")
        || !schema_validation_success
    {
        return Some("failed_empty_packet");
    }
    if status.trim().eq_ignore_ascii_case("needs_more_evidence") {
        return Some("missing_evidence");
    }
    if status.trim().eq_ignore_ascii_case("insufficient")
        && matches!(
            category,
            "persisted_identity_propagation"
                | "documentation_contract_consistency"
                | "suspicious_identifier_spelling"
        )
    {
        return Some("missing_evidence");
    }
    None
}

fn merged_candidate_findings(
    orchestrator_candidates: &[CandidateFinding],
    child_discoveries: &[ChildCandidateDiscovery],
) -> Vec<CandidateFinding> {
    let mut merged = Vec::new();
    let mut seen_keys = std::collections::BTreeSet::new();
    let mut seen_ids = std::collections::BTreeSet::new();

    for candidate in orchestrator_candidates {
        let key = candidate_merge_key(candidate);
        seen_keys.insert(key);
        let id = candidate.id.trim();
        if !id.is_empty() {
            seen_ids.insert(id.to_string());
        }
        merged.push(candidate.clone());
    }

    for discovery in child_discoveries {
        let mut candidate = discovery.candidate.clone();
        let key = candidate_merge_key(&candidate);
        if !seen_keys.insert(key.clone()) {
            continue;
        }
        let id = candidate.id.trim();
        if id.is_empty() || seen_ids.contains(id) {
            candidate.id = format!("child_{}", stable_id(&[&key, &discovery.child_session_id]));
        }
        seen_ids.insert(candidate.id.clone());
        if let Some(artifact_id) = &discovery.artifact_id {
            let artifact = artifact_id.0.clone();
            if !candidate.evidence_artifact_ids.contains(&artifact) {
                candidate.evidence_artifact_ids.push(artifact);
            }
        }
        merged.push(candidate);
    }

    merged
}

fn risk_seed_candidate_findings(
    entries: &[DiffRiskEntry],
    existing_candidates: &[CandidateFinding],
    max_candidates: usize,
) -> Vec<CandidateFinding> {
    let mut seeds = Vec::new();
    for entry in entries {
        if seeds.len() >= max_candidates {
            break;
        }
        if entry.category != "persisted_identity_propagation" {
            continue;
        }
        if existing_candidates
            .iter()
            .chain(seeds.iter())
            .any(|candidate| candidate_covers_risk_entry(candidate, entry))
        {
            continue;
        }
        seeds.push(persisted_identity_risk_seed_candidate(entry));
    }
    seeds
}

fn persisted_identity_risk_seed_candidate(entry: &DiffRiskEntry) -> CandidateFinding {
    let code = entry.code.trim();
    let path = entry.path.clone();
    let line = entry.line;
    CandidateFinding {
        id: format!("risk_seed_{}", stable_id(&[&entry.id, &entry.path, code])),
        title: "Persisted model reconstruction may drop the stored identity".to_string(),
        claim: format!(
            "The changed persisted-model reconstruction line `{code}` rebuilds a stored domain model from payload data without explicit id/identity propagation in the changed line. If the authoritative stored id is not copied into the reconstructed model, later id-based update, remove, lookup, audit, or callback paths can operate on a blank, missing, or wrong identity."
        ),
        negative_outcome:
            "Downstream identity-dependent operations can target no persisted object, target the wrong object, or leave stale stored state behind."
                .to_string(),
        severity: Some("low".to_string()),
        path,
        start_line: line,
        end_line: line,
        behavior_before: Some(
            "The stored object identity needed by downstream operations was available from the authoritative stored record."
                .to_string(),
        ),
        behavior_after: Some(format!(
            "The changed reconstruction path rebuilds the model at `{code}` and needs proof that the authoritative stored id/identity is preserved before downstream identity-dependent operations."
        )),
        evidence_artifact_ids: Vec::new(),
        related_paths: Vec::new(),
    }
}

fn candidate_merge_key(candidate: &CandidateFinding) -> String {
    stable_id(&[
        candidate.path.trim(),
        candidate.title.trim(),
        candidate.claim.trim(),
        candidate.negative_outcome.trim(),
    ])
}

fn select_lead_generation_entries(
    entries: &[DiffRiskEntry],
    candidates: &[CandidateFinding],
    completeness: &Value,
    max_entries: usize,
) -> Vec<DiffRiskEntry> {
    let unreviewed = completeness_string_set(completeness, "unreviewedRiskEntries");
    let mut ranked = entries
        .iter()
        .filter_map(|entry| {
            let category_priority = lead_generation_category_priority(entry.category)?;
            if candidates
                .iter()
                .any(|candidate| candidate_covers_risk_entry(candidate, entry))
            {
                return None;
            }
            let reviewed_priority = if unreviewed.contains(entry.id.as_str()) {
                0
            } else {
                1
            };
            Some((
                category_priority,
                reviewed_priority,
                entry.id.clone(),
                entry.clone(),
            ))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        (left.0, left.1, left.2.as_str()).cmp(&(right.0, right.1, right.2.as_str()))
    });
    let mut selected = Vec::new();
    let mut seen_leads = BTreeSet::new();
    let mut seen_categories = BTreeSet::new();
    for (_, _, _, entry) in &ranked {
        if seen_categories.contains(entry.category) {
            continue;
        }
        if !seen_leads.insert((entry.path.clone(), entry.category)) {
            continue;
        }
        seen_categories.insert(entry.category);
        selected.push(entry.clone());
        if selected.len() >= max_entries {
            return selected;
        }
    }
    for (_, _, _, entry) in ranked {
        if !seen_leads.insert((entry.path.clone(), entry.category)) {
            continue;
        }
        selected.push(entry);
        if selected.len() >= max_entries {
            break;
        }
    }
    selected
}

fn candidate_covers_risk_entry(candidate: &CandidateFinding, entry: &DiffRiskEntry) -> bool {
    if candidate.path != entry.path {
        return false;
    }
    if !candidate_text_covers_risk_category(candidate, entry.category) {
        return false;
    }
    let Some(entry_line) = entry.line else {
        return false;
    };
    let candidate_start = candidate.start_line.or(candidate.end_line);
    let candidate_end = candidate.end_line.or(candidate.start_line);
    match (candidate_start, candidate_end) {
        (Some(start), Some(end)) => {
            let expanded_start = start.saturating_sub(2);
            let expanded_end = end.saturating_add(2);
            (expanded_start..=expanded_end).contains(&entry_line)
        }
        _ => false,
    }
}

fn candidate_text_covers_risk_category(candidate: &CandidateFinding, category: &str) -> bool {
    let text = format!(
        "{} {} {} {} {}",
        candidate.title,
        candidate.claim,
        candidate.negative_outcome,
        candidate.behavior_before.as_deref().unwrap_or_default(),
        candidate.behavior_after.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    match category {
        "persisted_identity_propagation" => {
            let identity = text.contains("id")
                || text.contains("identifier")
                || text.contains("identity")
                || text.contains("key");
            let consequence = text.contains("null")
                || text.contains("missing")
                || text.contains("wrong")
                || text.contains("copy")
                || text.contains("preserve")
                || text.contains("remove")
                || text.contains("update")
                || text.contains("lookup")
                || text.contains("stale");
            identity && consequence
        }
        "unchecked_optional_access" => {
            (text.contains("optional")
                || text.contains("unwrap")
                || text.contains(".get()")
                || text.contains("nosuchelement"))
                && (text.contains("empty")
                    || text.contains("presence")
                    || text.contains("absence")
                    || text.contains("without checking")
                    || text.contains("without a guard"))
        }
        "unchecked_collection_shape" => {
            text.contains("list.class")
                || text.contains("map.class")
                || text.contains("unchecked")
                || text.contains("deserialize")
                || text.contains("cast")
                || text.contains("shape")
                || text.contains("type")
        }
        "nullability_contract" => {
            text.contains("null")
                || text.contains("nonnull")
                || text.contains("non-null")
                || text.contains("require")
                || text.contains("checked value")
        }
        "identifier_lookup_contract" => {
            text.contains("lookup")
                || text.contains("find")
                || text.contains("identifier")
                || text.contains(" id")
                || text.contains("name")
                || text.contains("owner")
        }
        "documentation_contract_consistency" => {
            text.contains("doc")
                || text.contains("contract")
                || text.contains("comment")
                || text.contains("example")
        }
        "regex_matcher_contract" => {
            text.contains("regex")
                || text.contains("matcher")
                || text.contains("group")
                || text.contains("replace")
                || text.contains("pattern")
        }
        "localized_resource_change" | "localized_script_mismatch" => {
            text.contains("locale")
                || text.contains("localized")
                || text.contains("translation")
                || text.contains("language")
                || text.contains("script")
        }
        "async_callback" | "async_boundary" => {
            text.contains("async")
                || text.contains("await")
                || text.contains("promise")
                || text.contains("callback")
        }
        "side_effect_aggregation" => {
            text.contains("side effect")
                || text.contains("promise")
                || text.contains("await")
                || text.contains("delete")
                || text.contains("update")
                || text.contains("write")
        }
        "lazy_module_loading" => text.contains("import") || text.contains("module"),
        "offset_or_slice_boundary" => {
            text.contains("offset")
                || text.contains("slice")
                || text.contains("substring")
                || text.contains("sublist")
                || text.contains("index")
                || text.contains("bound")
        }
        "suspicious_identifier_spelling" => {
            text.contains("spell")
                || text.contains("typo")
                || text.contains("identifier")
                || text.contains("override")
        }
        "broad_exception_boundary" => {
            text.contains("exception")
                || text.contains("catch")
                || text.contains("throwable")
                || text.contains("runtime")
        }
        "feature_gate_consistency" => {
            text.contains("feature")
                || text.contains("gate")
                || text.contains("flag")
                || text.contains("profile")
        }
        "process_exit_boundary" => text.contains("exit") || text.contains("process"),
        _ => true,
    }
}

fn completeness_string_set<'a>(completeness: &'a Value, key: &str) -> BTreeSet<&'a str> {
    completeness
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn lead_generation_category_priority(category: &str) -> Option<u8> {
    match category {
        "regex_matcher_contract" => Some(0),
        "localized_script_mismatch" => Some(1),
        "broad_exception_boundary" => Some(1),
        "suspicious_identifier_spelling" => Some(2),
        "documentation_contract_consistency" => Some(3),
        "unchecked_optional_access" => Some(2),
        "localized_resource_change" => Some(3),
        "async_callback" | "side_effect_aggregation" | "lazy_module_loading" => Some(2),
        "persisted_identity_propagation" => Some(4),
        "nullability_contract"
        | "offset_or_slice_boundary"
        | "unchecked_collection_shape"
        | "identifier_lookup_contract" => Some(3),
        _ => None,
    }
}

fn lead_generation_budget_for_category(category: &str) -> AgentBudget {
    let mut budget = lead_generation_child_budget();
    if category == "persisted_identity_propagation"
        || category == "documentation_contract_consistency"
    {
        budget.max_turns = budget.max_turns.max(6);
        budget.max_tool_calls = budget.max_tool_calls.max(10);
    }
    budget
}

fn lead_generation_category_instruction(category: &str) -> &'static str {
    match category {
        "regex_matcher_contract" => {
            "For matcher/parser/sanitizer leads, inspect the actual stateful consumption site first: find()/matches(), group(), replaceFirst()/replaceAll(), and loops. Check source-vs-target parity, not only whether group() is guarded. If one matcher advances while consuming groups from a second source matcher, prove extra target groups, missing source groups, remaining unmatched source groups, and replacement/break behavior cannot produce incorrect validation. When mismatch directions have different outcomes, emit separate candidates or one candidate that explicitly names each supported direction; do not emit only the inverse direction of the concrete false accept/false reject that the code proves."
        }
        "suspicious_identifier_spelling" => {
            "For spelling leads, do not refute solely because the changed code compiles, is private, or calls itself consistently. Compare the new identifier to nearby APIs, overrides, call sites, comments, and conventional spelling. Return a low-severity candidate when raw code proves a changed misspelled identifier and the intended term is clear from adjacent naming/comment context; the negative outcome may be maintenance/search/discoverability confusion for future callers. Require stronger evidence only for claims about missed overrides or split call paths."
        }
        "unchecked_optional_access" => {
            "For optional/result unwrapping leads, trace the exact value from producer to get()/unwrap()/expect() and prove presence on every changed path before refuting. Anchor the candidate to the changed producer/unwrap pair. Do not bundle an older pre-existing unwrap into the same candidate unless the changed producer newly makes that older unwrap unsafe; otherwise omit the older unwrap or split it. Before finalizing, search/read changed code for nearby raw collection deserialization, List.class/Map.class, unchecked casts, or collection-shape changes involving the same domain value. If the optional unwrap and untyped collection shape are connected, emit one candidate that explicitly names both the unchecked shape source and the unchecked unwrap outcome instead of splitting them. Refute only when raw code proves a dominating precondition guarantees the unwrapped value is present and the collection shape is type-safe on the changed path."
        }
        "unchecked_collection_shape" => {
            "For raw/unchecked collection-shape leads, trace the collection from deserialization/cast through the first typed consumer. Check whether List.class/Map.class, raw List/Map, unchecked casts, or erased element types can feed a typed model, Optional/result producer, or unwrap without proving element shape. If the same changed path also has an Optional.get()/unwrap()/expect() on the derived value, emit one candidate that names both the raw collection shape source and the unsafe unwrap/absence outcome; do not leave the raw collection evidence as notes only. Refute only when raw code proves element type/shape is validated before typed consumption and any unwrap is dominated by a presence check."
        }
        "broad_exception_boundary" => {
            "For broad exception leads, identify the precise failure mode the code intended to handle and verify unrelated runtime errors still surface. Return a candidate only when the broad catch can hide or transform a distinct real failure."
        }
        "documentation_contract_consistency" => {
            "For docs/contract leads, compare public API documentation, Javadocs, generated schemas, examples, and implementer-facing comments against executable validation, existing callers, and built-in implementations. Pay special attention to numeric length/count claims such as shortcut size, bounds, limits, and accepted formats. Emit a candidate when code or implementations establish a different fixed constraint or when documented typical/example wording would guide callers or implementers toward a shape contradicted by built-in implementations. Do not refute solely because wording says usually or example when the surrounding API contract would guide implementers. Treat inline TODO/comment cleanup observations as notes unless raw code proves a concrete behavior failure beyond the comment itself."
        }
        "persisted_identity_propagation" => {
            "For persisted identity leads, read the changed construction/reconstruction site around riskEntry.line first, then read one downstream update/remove/lookup/audit/callback consumer that uses id/identity before refuting. Avoid starting with broad repository searches unless the changed site does not expose the model or consumer names. Trace created/stored domain models through later operations that depend on id/identity. Verify the changed path copies or preserves the persisted id from the authoritative stored object, not only its payload fields. Return a candidate when the missing or wrong identity makes later operations target nothing, target the wrong object, or leave stale persisted state."
        }
        _ => {
            "Use the risk entry's obligation as the investigation checklist, and explicitly state the evidence that supports or refutes the lead."
        }
    }
}

fn direct_session_config(scope: SessionScope) -> AgentLoopConfig {
    let turn_guard = scope.budget.max_turns.max(1);
    let response_format = scope.response_format.clone().unwrap_or_else(|| {
        ModelResponseFormat::json_schema(
            "muzen_direct_session_result_v1",
            json!({
                "type": "object",
                "additionalProperties": true
            }),
        )
    });
    AgentLoopConfig {
        scope,
        task_packet: None,
        trace_kind: "direct_session",
        completion_kind: "direct_session",
        response_format,
        final_instruction: "Return the final output for this session now. Do not call more tools."
            .to_string(),
        turn_guard,
        should_force_final_turn: Box::new(move |turn_index, tool_calls_used, budget| {
            tool_calls_used >= budget.max_tool_calls || turn_index >= turn_guard.saturating_sub(1)
        }),
        output_valid: Box::new(|output| output.is_some()),
        schema_repair_instruction: Box::new(|_, _| {
            "Return the final output for this session now.".to_string()
        }),
        schema_repair_attempts: 0,
    }
}

#[cfg(test)]
mod tests;
