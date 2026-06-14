use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::contracts::{
    AgentBudget, ArtifactKind, ByteRangeV1, ChallengeStatus, EvidenceLocationV1, EvidenceRefV1,
    EvidenceRevision, FileReviewV1, FindingPublishability, FindingSeverity, FindingV1, LineRangeV1,
    RedactionMetadataV1, RedactionState, ReportStatus, ReviewCoverage, ReviewVerdict, Role,
    TokenUsage, ToolCounts, ToolName, ValidationStatus,
};
use crate::review_plan::ReviewPlanFileMode;
use crate::review_plan::{build_review_plan, ReviewPlan};
use crate::review_units::{build_review_unit_plan, PlannedReviewUnit, ReviewUnitOptions};
use crate::runtime::contracts::*;
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::effects::{ToolResultBatchState, ToolResultEffectProcessor};
use crate::runtime::model::ConcurrentModelRouter;
use crate::runtime::model_retry::complete_model_turn;
use crate::runtime::policy::{ReviewerPolicy, SessionEvidence};
use crate::runtime::repo::RepoSnapshot;
use crate::runtime::tool_batch::ToolBatchRunner;
use crate::runtime::tools::{ConcurrentArtifactStore, ToolEngine};
use crate::runtime::transcript::{enforce_prompt_budget, estimate_prompt_tokens};
use crate::util::peak_rss_bytes;

#[derive(Debug, Clone, Copy, Default)]
struct DiffPackContext;

impl DiffPackContext {
    fn empty() -> Self {
        Self
    }

    fn pack_count(&self) -> usize {
        0
    }
}

pub(crate) struct PlannedReviewRuntime {
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) model_router: Arc<dyn ConcurrentModelRouter>,
    pub(crate) tools: Arc<ToolEngine>,
    pub(crate) policy: Arc<ReviewerPolicy>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) review_revision_id: String,
    pub(crate) events: RuntimeEventDispatcher,
    pub(crate) session_templates: Vec<SessionScope>,
    /// Bounds concurrently active sessions. Shared across shards of one run
    /// so multi-snapshot runs cannot multiply max_active_sessions.
    pub(crate) active_sessions: Arc<Semaphore>,
}

pub(crate) fn session_semaphore(limits: &RuntimeLimits) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(limits.max_active_sessions.max(1)))
}

impl PlannedReviewRuntime {
    pub(crate) async fn run_with_cancel(
        self: Arc<Self>,
        cancel: CancellationToken,
    ) -> PlannedReviewRunReport {
        let started = Instant::now();
        let review_plan = build_review_plan(&self.snapshot);
        let contract_packs = DiffPackContext::empty();
        let unit_plan =
            build_review_unit_plan(&review_plan, adaptive_review_unit_options(&review_plan));
        let contract_risk = build_contract_risk_plan(
            &review_plan,
            &unit_plan,
            self.snapshot.diff.content.as_str(),
        );
        let review_plan = Arc::new(review_plan);
        let contract_risk = Arc::new(contract_risk);
        let contract_packs = Arc::new(contract_packs);
        let mut completed_sessions = 0usize;
        let mut model_calls = 0usize;
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut tool_counts = ToolCounts::default();
        let mut tokens = TokenUsage::default();
        let mut completion_diagnostics = Vec::new();
        let mut candidate_findings = Vec::new();
        let mut file_reviews = skipped_file_reviews(review_plan.as_ref());

        // Units run concurrently, bounded by max_active_sessions. High-risk
        // units fan out into one session per distinct lens role; low-risk
        // units keep a single session so token cost scales with stakes.
        // Reports are re-ordered by (unit, lens) index before aggregation so
        // findings, file reviews, and synthesis input stay deterministic
        // regardless of completion order.
        let active = Arc::clone(&self.active_sessions);
        let mut joins = JoinSet::new();
        for (unit_index, unit) in unit_plan.units.iter().enumerate() {
            let high_risk = contract_risk.unit_risk(unit).high_risk;
            let lens_templates =
                unit_lens_template_indices(&self.session_templates, high_risk, unit.score_max);
            for (lens_index, template_index) in lens_templates.into_iter().enumerate() {
                let runtime = Arc::clone(&self);
                let review_plan = Arc::clone(&review_plan);
                let contract_risk = Arc::clone(&contract_risk);
                let contract_packs = Arc::clone(&contract_packs);
                let active = Arc::clone(&active);
                let unit = unit.clone();
                let cancel = cancel.child_token();
                joins.spawn(async move {
                    let Ok(_permit) = active.acquire_owned().await else {
                        return (
                            (unit_index, lens_index),
                            PlannedReviewUnitReport::empty(unit_diagnostic(
                                &unit,
                                false,
                                "cancelled",
                            )),
                        );
                    };
                    if cancel.is_cancelled() {
                        return (
                            (unit_index, lens_index),
                            PlannedReviewUnitReport::empty(unit_diagnostic(
                                &unit,
                                false,
                                "cancelled",
                            )),
                        );
                    }
                    let report = runtime
                        .run_unit(
                            review_plan.as_ref(),
                            contract_risk.as_ref(),
                            contract_packs.as_ref(),
                            unit,
                            lens_index,
                            template_index,
                            cancel,
                        )
                        .await;
                    ((unit_index, lens_index), report)
                });
            }
        }
        let mut unit_reports = Vec::with_capacity(unit_plan.units.len());
        while let Some(result) = joins.join_next().await {
            if let Ok(indexed) = result {
                unit_reports.push(indexed);
            }
        }
        unit_reports.sort_by_key(|(key, _)| *key);
        for ((_, lens_index), report) in unit_reports {
            if report.completed {
                completed_sessions += 1;
            }
            model_calls += report.model_calls;
            add_model_metrics(&mut model_metrics, &report.model_metrics);
            tool_counts.add(report.tool_counts);
            tokens.add(report.tokens);
            candidate_findings.extend(report.candidate_findings);
            // Only the primary lens owns per-file verdicts; secondary lens
            // findings still flip verdicts via reconcile_file_reviews_with_findings.
            if lens_index == 0 {
                file_reviews.extend(report.file_reviews);
            }
            completion_diagnostics.push(report.completion_diagnostic);
        }
        append_unverdicted_file_reviews(&unit_plan.units, &mut file_reviews);
        let synthesis = synthesize_findings(
            review_plan.as_ref(),
            &unit_plan.units,
            contract_risk.as_ref(),
            self.snapshot.diff.content.as_str(),
            candidate_findings,
            &self.tools.artifacts,
            &self.review_revision_id,
        );
        let synthesis_scope = SessionScope::review_read_only(
            SessionId("synthesis".to_string()),
            Role::Generalist,
            "planned review synthesis",
            AgentBudget {
                max_turns: 1,
                max_tool_calls: 0,
                max_prompt_tokens: 0,
                max_output_tokens: 0,
                budget_source: crate::contracts::BudgetSource::PlannedDefault,
            },
        );
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                &synthesis_scope,
                None,
                "candidate_synthesis_summary",
                format!(
                    "synthesized {} candidate(s) into {} finding(s)",
                    synthesis.candidate_count,
                    synthesis.findings.len()
                ),
                json!({
                    "candidateCount": synthesis.candidate_count,
                    "rescuedCount": synthesis.rescued_count,
                    "rejectedCount": synthesis.rejected_count,
                    "findingCount": synthesis.findings.len(),
                    "rejectionReasons": synthesis.rejection_reasons,
                }),
            ));
        if synthesis.candidate_count == 0 {
            self.events
                .emit_planned_runtime(self.policy.plan_agent_trace_event(
                    &synthesis_scope,
                    None,
                    "candidate_finding_decision",
                    "synthesis produced no candidate findings".to_string(),
                    json!({
                        "phase": "synthesis",
                        "decision": "none",
                        "reason": "no_candidate_findings",
                        "candidateCount": 0,
                        "findingCount": 0,
                        "rejectedCount": synthesis.rejected_count,
                        "rescuedCount": synthesis.rescued_count,
                        "rejectionReasons": synthesis.rejection_reasons,
                    }),
                ));
        }
        let mut findings = synthesis.findings;
        for finding in &findings {
            self.events.emit_runtime_with_context(
                RuntimeEventContext {
                    session_id: Some(SessionId("synthesis".to_string())),
                    tool_call_id: Some(ToolCallId(format!("{}-synthesis", finding.id))),
                    finding_id: Some(finding.id.clone()),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::FindingRecorded {
                    finding_id: finding.id.clone(),
                    session_id: SessionId("synthesis".to_string()),
                    tool_call_id: ToolCallId(format!("{}-synthesis", finding.id)),
                },
            );
        }
        if !cancel.is_cancelled() && !findings.is_empty() {
            let challenge_report = self
                .run_finding_challenge_pass(&mut findings, cancel.child_token())
                .await;
            model_calls += challenge_report.model_calls;
            add_model_metrics(&mut model_metrics, &challenge_report.model_metrics);
            tool_counts.add(challenge_report.tool_counts);
            tokens.add(challenge_report.tokens);
            completion_diagnostics.push(challenge_report.completion_diagnostic);
        }
        reconcile_file_reviews_with_findings(&mut file_reviews, &findings);
        completion_diagnostics.push(SessionCompletionDiagnostic {
            session_id: "synthesis".to_string(),
            completed: true,
            completion_kind: Some("structured_synthesis".to_string()),
            completion_summary: Some(format!(
                "candidateFindings={} rescuedCandidates={} rejectedCandidates={} rejectionReasons={}",
                synthesis.candidate_count,
                synthesis.rescued_count,
                synthesis.rejected_count,
                format_rejection_reasons(&synthesis.rejection_reasons)
            )),
            saw_diff: true,
            saw_file: false,
            saw_search: false,
            model_calls: 0,
            tool_counts: ToolCounts::default(),
        });

        let (artifacts, artifact_bytes) = self.tools.artifacts.stats();
        let counters = self.tools.snapshot_counters();
        let tool_metrics = self.tools.snapshot_tool_metrics();
        let provider_health = self.tools.snapshot_provider_health();
        let elapsed_ms = elapsed_ms(started);
        let review_sessions = completion_diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.session_id != "synthesis"
                    && diagnostic.session_id != "final-synthesis"
                    && diagnostic.session_id != "finding-challenge"
                    && !diagnostic.session_id.starts_with("pack-")
            })
            .count();
        let quality_diagnostics = planned_review_audit_diagnostics(
            review_plan.as_ref(),
            contract_risk.as_ref(),
            contract_packs.as_ref(),
            &file_reviews,
            &findings,
            &self.session_templates,
            review_sessions,
            synthesis.candidate_count,
            synthesis.rescued_count,
            synthesis.rejected_count,
            synthesis.rejection_reasons.clone(),
        );
        let mut metrics = ConcurrentRunReport {
            runtime: "planned_units",
            sessions: review_sessions,
            completed_sessions,
            model_calls,
            tool_calls: tool_counts.total(),
            tool_counts,
            findings: findings.len(),
            publishable_findings: findings
                .iter()
                .filter(|finding| {
                    matches!(finding.publishability, FindingPublishability::Publishable)
                })
                .count(),
            elapsed_ms,
            input_tokens: tokens.input_tokens,
            output_tokens: tokens.output_tokens,
            total_tokens: tokens.total_tokens,
            cached_input_tokens: tokens.cached_input_tokens,
            artifacts,
            artifact_bytes,
            counters,
            tool_metrics,
            provider_health,
            snapshot_metrics: vec![SnapshotMetricsSnapshot {
                snapshot_id: self.snapshot.snapshot_id.clone(),
                sessions: review_sessions,
                completed_sessions,
                model_calls,
                tool_calls: tool_counts.total(),
                artifacts,
                artifact_bytes,
                elapsed_ms,
            }],
            model_metrics,
            completion_diagnostics,
            quality_diagnostics,
            benchmark_valid: false,
            benchmark_failures: Vec::new(),
        };
        metrics.benchmark_failures = planned_benchmark_failures(&metrics);
        metrics.benchmark_valid = metrics.benchmark_failures.is_empty();
        PlannedReviewRunReport {
            metrics,
            findings,
            file_reviews,
        }
    }

    fn emit_transcript_compacted_trace(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        evicted_tool_results: usize,
        transcript: &[ConversationItem],
    ) {
        if evicted_tool_results == 0 {
            return;
        }
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                scope,
                Some(turn_id),
                "transcript_compacted",
                format!("evicted {evicted_tool_results} old tool result(s) before model turn"),
                json!({
                    "evictedToolResults": evicted_tool_results,
                    "transcriptItemsAfter": transcript.len(),
                    "estimatedPromptTokensAfter": estimate_prompt_tokens(transcript),
                    "maxPromptTokens": scope.budget.max_prompt_tokens,
                }),
            ));
    }

    fn emit_model_turn_prepared_trace(
        &self,
        scope: &SessionScope,
        model_scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
        final_turn: bool,
    ) {
        let schemas = self.policy.tool_schemas_for_transcript(
            &self.tools.registry,
            transcript,
            &model_scope.capabilities,
        );
        let alias_table = self.tools.registry.alias_table().ok();
        let schema_text = serde_json::to_string(&schemas).unwrap_or_default();
        let response_format = model_scope.response_format.as_ref();
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                scope,
                Some(turn_id),
                "model_turn_prepared",
                format!(
                    "prepared model turn with {} transcript item(s) and {} exposed tool(s)",
                    transcript.len(),
                    schemas.len()
                ),
                json!({
                    "finalTurn": final_turn,
                    "callScopeHasTools": !model_scope.capabilities.tool_grants.is_empty(),
                    "transcriptItems": transcript.len(),
                    "transcriptBytes": transcript_bytes(transcript),
                    "estimatedPromptTokens": estimate_prompt_tokens(transcript),
                        "maxPromptTokens": scope.budget.max_prompt_tokens,
                        "peakRssBytes": peak_rss_bytes(),
                        "systemDigest": first_system_digest(transcript),
                    "lastUserDigest": last_user_digest(transcript),
                    "exposedTools": schemas
                        .iter()
                        .filter_map(|schema| schema_tool_trace(schema, alias_table.as_ref()))
                        .collect::<Vec<_>>(),
                    "schemaDigest": stable_id(&[&schema_text]),
                    "structuredOutputRequested": response_format.is_some(),
                    "responseFormatName": response_format.map(|format| format.name.as_str()),
                    "responseFormatStrict": response_format.map(|format| format.strict),
                    "responseFormatSchemaDigest": response_format_schema_digest(response_format),
                }),
            ));
    }

    fn emit_model_turn_completed_trace(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        turn: &ModelTurn,
        attempts: usize,
    ) {
        let (output_kind, summary, details) = match turn {
            ModelTurn::Text { content, usage } => (
                "text",
                format!("model returned text output ({} bytes)", content.len()),
                json!({
                    "attempts": attempts,
                    "outputKind": "text",
                    "textBytes": content.len(),
                    "textDigest": stable_id(&[content]),
                    "usage": usage,
                }),
            ),
            ModelTurn::ToolCalls { calls, usage } => (
                "tool_calls",
                format!("model returned {} tool call(s)", calls.len()),
                json!({
                    "attempts": attempts,
                    "outputKind": "tool_calls",
                    "toolCallCount": calls.len(),
                    "toolCalls": calls.iter().map(|call| json!({
                        "callId": call.call_id.0,
                        "index": call.index,
                        "toolName": call.name.as_str(),
                        "argumentBytes": call.raw_arguments.len(),
                        "argumentHash": blake3::hash(call.raw_arguments.as_bytes()).to_hex().to_string(),
                        "argumentSummary": call.redacted_argument_summary(),
                    })).collect::<Vec<_>>(),
                    "usage": usage,
                }),
            ),
        };
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                scope,
                Some(turn_id),
                format!("model_turn_completed.{output_kind}"),
                summary,
                details,
            ));
    }

    fn emit_candidate_decision_trace(
        &self,
        scope: &SessionScope,
        candidate: &CandidateFinding,
        phase: &str,
    ) {
        let decision = candidate
            .rejection_reason
            .as_deref()
            .map(|_| "rejected")
            .unwrap_or("candidate");
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                scope,
                None,
                "candidate_finding_decision",
                format!("{phase} {decision}: {}", candidate.title),
                json!({
                    "phase": phase,
                    "decision": decision,
                    "reason": candidate.rejection_reason,
                    "sourceUnitId": candidate.source_unit_id,
                    "sourceSessionId": candidate.source_session_id,
                    "title": candidate.title,
                    "claimHash": stable_id(&[&candidate.claim]),
                    "path": candidate.path,
                    "relatedPaths": candidate.related_paths,
                    "startLine": candidate.start_line,
                    "endLine": candidate.end_line,
                    "evidenceArtifactIds": candidate.evidence_artifact_ids,
                    "sourceUnitAssignedPath": candidate.source_unit_assigned_path,
                }),
            ));
    }

    fn emit_risk_playbook_trace(
        &self,
        scope: &SessionScope,
        unit: &PlannedReviewUnit,
        unit_risk: &ContractUnitRisk,
        _contract_packs: &DiffPackContext,
    ) {
        let playbooks = unit_risk_playbooks(unit, unit_risk);
        if playbooks.is_empty() {
            return;
        }
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                scope,
                None,
                "risk_playbooks_selected",
                format!(
                    "selected risk playbooks: {}",
                    playbooks
                        .iter()
                        .map(|playbook| playbook.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                json!({
                    "playbooks": playbooks
                        .iter()
                        .map(|playbook| playbook.name())
                        .collect::<Vec<_>>(),
                    "unitId": unit.id.as_str(),
                    "highRisk": unit_risk.high_risk,
                    "reasons": unit_risk.reasons.clone(),
                    "seeds": unit_risk.seeds.clone(),
                    "suggestedQueries": unit_risk.suggested_queries.clone(),
                }),
            ));
    }

    async fn run_unit(
        &self,
        review_plan: &ReviewPlan,
        contract_risk: &ContractRiskPlan,
        contract_packs: &DiffPackContext,
        unit: PlannedReviewUnit,
        lens_index: usize,
        template_index: Option<usize>,
        cancel: CancellationToken,
    ) -> PlannedReviewUnitReport {
        let template = template_index.and_then(|index| self.session_templates.get(index));
        let scope = unit_scope(&unit, &self.snapshot.snapshot_id, template, lens_index);
        self.events
            .emit_planned_runtime(self.policy.plan_session_started_runtime_event(&scope));
        let model = match self.model_router.client_for(&scope).await {
            Ok(model) => model,
            Err(_error) => {
                self.events.emit_planned_runtime(
                    self.policy
                        .plan_session_finished_runtime_event(&scope, "failed"),
                );
                return PlannedReviewUnitReport::empty(unit_diagnostic(&unit, false, "failed"));
            }
        };

        let unit_risk = contract_risk.unit_risk(&unit);
        self.emit_risk_playbook_trace(&scope, &unit, unit_risk, contract_packs);
        let mut transcript = planned_unit_transcript(
            review_plan,
            &unit,
            unit_risk,
            lens_focus(lens_index, scope.role),
            contract_packs,
        );
        let mut evidence = SessionEvidence::for_scope(&scope);
        let mut tool_counts = ToolCounts::default();
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut tokens = TokenUsage::default();
        let mut model_calls = 0usize;
        let mut file_evidence = FileEvidenceTracker::new(&unit);
        let turn_limit = scope.budget.max_turns.max(1);

        bootstrap_unit_evidence(
            self,
            &scope,
            review_plan,
            &unit,
            unit_risk,
            contract_packs,
            &mut transcript,
            &mut evidence,
            &mut tool_counts,
            &mut file_evidence,
            cancel.child_token(),
        )
        .await;
        for turn_index in 0..turn_limit {
            if cancel.is_cancelled() {
                break;
            }
            let turn_id = TurnId(turn_index as u32);
            let evicted_tool_results =
                enforce_prompt_budget(&mut transcript, scope.budget.max_prompt_tokens);
            self.emit_transcript_compacted_trace(
                &scope,
                turn_id,
                evicted_tool_results,
                &transcript,
            );
            self.events.emit_planned_runtime(
                self.policy
                    .plan_model_started_runtime_event(&scope, turn_id),
            );
            let model_started = Instant::now();
            let final_turn =
                turn_index + 1 >= turn_limit || tool_counts.total() >= scope.budget.max_tool_calls;
            let model_scope = if final_turn {
                final_response_scope(&scope, unit_result_response_format())
            } else {
                scope.clone()
            };
            self.emit_model_turn_prepared_trace(
                &scope,
                &model_scope,
                &transcript,
                turn_id,
                final_turn,
            );
            let outcome = complete_model_turn(
                &*model,
                &self.policy,
                &self.events,
                &self.limits,
                &scope,
                &model_scope,
                &transcript,
                turn_id,
                &cancel,
            )
            .await;
            model_calls += outcome.attempts;
            model_metrics.calls += outcome.attempts;
            let turn = match outcome.result {
                Ok(turn) => {
                    model_metrics.errors += outcome.attempts - 1;
                    turn
                }
                Err(_) => {
                    model_metrics.errors += outcome.attempts;
                    break;
                }
            };
            self.emit_model_turn_completed_trace(&scope, turn_id, &turn, outcome.attempts);
            model_metrics.successes += 1;
            model_metrics.latency_ms += elapsed_ms(model_started);
            model_metrics.max_latency_ms =
                model_metrics.max_latency_ms.max(elapsed_ms(model_started));

            match turn {
                ModelTurn::Text { content, usage } => {
                    record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                    self.events.emit_planned_runtime(
                        self.policy
                            .plan_model_completed_runtime_event(&scope, turn_id, 0),
                    );
                    let parsed_result = parse_unit_result(&content, &unit);
                    let result = parsed_result.result;
                    self.events
                        .emit_planned_runtime(self.policy.plan_agent_trace_event(
                            &scope,
                            Some(turn_id),
                            "unit_result_parsed",
                            format!(
                            "parsed unit result parsed={} extracted={} fileVerdicts={} findings={}",
                            parsed_result.parsed,
                            parsed_result.extracted_json,
                            result.file_verdicts.len(),
                            result.findings.len()
                        ),
                            json!({
                                "parsed": parsed_result.parsed,
                                "extractedJson": parsed_result.extracted_json,
                                "fileVerdictCount": result.file_verdicts.len(),
                                "findingCount": result.findings.len(),
                                "summary": truncate_chars(&result.summary, 500),
                            }),
                        ));
                    let validation = validate_file_reviews(
                        &scope,
                        &unit,
                        unit_risk,
                        result.file_verdicts,
                        &file_evidence,
                    );
                    if !validation.missing_obligations.is_empty() && !final_turn {
                        transcript.push(ConversationItem::AssistantText { content });
                        remediate_missing_evidence(
                            self,
                            &scope,
                            review_plan,
                            unit_risk,
                            &validation.missing_obligations,
                            &mut transcript,
                            &mut evidence,
                            &mut tool_counts,
                            &mut file_evidence,
                            cancel.child_token(),
                        )
                        .await;
                        transcript.push(ConversationItem::User {
                            content: missing_evidence_instruction(
                                unit_risk,
                                &validation.missing_obligations,
                            ),
                        });
                        continue;
                    }
                    let mut file_reviews = validation.file_reviews;
                    append_needs_review_for_missing(
                        &scope,
                        &unit,
                        unit_risk,
                        &file_evidence,
                        &validation.missing_obligations,
                        &mut file_reviews,
                    );
                    let candidate_findings = collect_candidate_findings(
                        &scope,
                        &unit,
                        review_plan,
                        result.findings,
                        &file_evidence,
                    );
                    for candidate in &candidate_findings {
                        self.emit_candidate_decision_trace(&scope, candidate, "unit_result");
                    }
                    self.events.emit_planned_runtime(
                        self.policy
                            .plan_session_finished_runtime_event(&scope, "done"),
                    );
                    return PlannedReviewUnitReport {
                        completed: true,
                        model_calls,
                        model_metrics,
                        tool_counts,
                        tokens,
                        candidate_findings,
                        file_reviews,
                        completion_diagnostic: SessionCompletionDiagnostic {
                            session_id: scope.id.0,
                            completed: true,
                            completion_kind: Some("structured_unit_result".to_string()),
                            completion_summary: Some(format!(
                                "{} contractRisk={} missingEvidence={}",
                                result.summary,
                                unit_risk.high_risk,
                                validation.missing_obligations.len()
                            )),
                            saw_diff: true,
                            saw_file: true,
                            saw_search: file_evidence.has_contract_evidence(unit_risk),
                            model_calls,
                            tool_counts,
                        },
                    };
                }
                ModelTurn::ToolCalls { calls, usage } => {
                    record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                    self.events.emit_planned_runtime(
                        self.policy.plan_model_completed_runtime_event(
                            &scope,
                            turn_id,
                            calls.len(),
                        ),
                    );
                    transcript.push(ConversationItem::AssistantToolCalls {
                        calls: calls.clone(),
                    });
                    let results = ToolBatchRunner::new(
                        self.policy.as_ref(),
                        self.tools.as_ref(),
                        &self.events,
                    )
                    .execute(
                        scope.clone(),
                        turn_id,
                        calls,
                        &evidence,
                        scope
                            .budget
                            .max_tool_calls
                            .saturating_sub(tool_counts.total()),
                        cancel.child_token(),
                    )
                    .await;
                    file_evidence.observe_results(&results, &self.tools.artifacts);
                    ToolResultEffectProcessor::new(
                        self.policy.as_ref(),
                        self.tools.as_ref(),
                        &self.events,
                        &self.review_revision_id,
                    )
                    .apply_batch(
                        &scope,
                        turn_id,
                        results,
                        ToolResultBatchState {
                            evidence: &mut evidence,
                            tool_counts: &mut tool_counts,
                            transcript: &mut transcript,
                        },
                    );
                    transcript.push(ConversationItem::User {
                        content: next_unit_instruction(
                            next_turn_can_explore(turn_index, turn_limit, &scope, &tool_counts),
                            &unit,
                            unit_risk,
                            &file_evidence,
                        ),
                    });
                }
            }
        }

        self.events.emit_planned_runtime(
            self.policy
                .plan_session_finished_runtime_event(&scope, "partial"),
        );
        PlannedReviewUnitReport {
            completed: false,
            model_calls,
            model_metrics,
            tool_counts,
            tokens,
            candidate_findings: Vec::new(),
            file_reviews: Vec::new(),
            completion_diagnostic: unit_diagnostic(&unit, false, "partial"),
        }
    }

    /// Adversarial verification over validated findings: a single budgeted
    /// challenger session attempts to refute each finding against the diff
    /// evidence. Refuted findings stay in the report for audit but are
    /// suppressed from publication and excluded from file-verdict
    /// reconciliation.
    async fn run_finding_challenge_pass(
        &self,
        findings: &mut [FindingV1],
        cancel: CancellationToken,
    ) -> FindingChallengeReport {
        let scope =
            finding_challenge_scope(&self.snapshot.snapshot_id, self.session_templates.first());
        let model = match self.model_router.client_for(&scope).await {
            Ok(model) => model,
            Err(_error) => {
                return FindingChallengeReport::empty(finding_challenge_diagnostic(
                    false,
                    "model_router_failed",
                    0,
                ));
            }
        };
        let mut transcript =
            finding_challenge_transcript(findings, self.snapshot.diff.content.as_str());
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut evidence = SessionEvidence::for_scope(&scope);
        let mut tool_counts = ToolCounts::default();
        let mut tokens = TokenUsage::default();
        let mut model_calls = 0usize;
        let turn_limit = scope.budget.max_turns.max(1);
        for turn_index in 0..turn_limit {
            if cancel.is_cancelled() {
                break;
            }
            let turn_id = TurnId(turn_index as u32);
            let evicted_tool_results =
                enforce_prompt_budget(&mut transcript, scope.budget.max_prompt_tokens);
            self.emit_transcript_compacted_trace(
                &scope,
                turn_id,
                evicted_tool_results,
                &transcript,
            );
            self.events.emit_planned_runtime(
                self.policy
                    .plan_model_started_runtime_event(&scope, turn_id),
            );
            let model_started = Instant::now();
            let final_turn =
                turn_index + 1 >= turn_limit || tool_counts.total() >= scope.budget.max_tool_calls;
            let model_scope = if final_turn {
                final_response_scope(&scope, challenge_result_response_format())
            } else {
                scope.clone()
            };
            self.emit_model_turn_prepared_trace(
                &scope,
                &model_scope,
                &transcript,
                turn_id,
                final_turn,
            );
            let outcome = complete_model_turn(
                &*model,
                &self.policy,
                &self.events,
                &self.limits,
                &scope,
                &model_scope,
                &transcript,
                turn_id,
                &cancel,
            )
            .await;
            model_calls += outcome.attempts;
            model_metrics.calls += outcome.attempts;
            let turn = match outcome.result {
                Ok(turn) => {
                    model_metrics.errors += outcome.attempts - 1;
                    turn
                }
                Err(error) => {
                    model_metrics.errors += outcome.attempts;
                    let summary = match error {
                        RuntimeError::Timeout => "timeout",
                        _ => "model_failed",
                    };
                    mark_challenge_incomplete(findings);
                    return FindingChallengeReport {
                        model_calls,
                        model_metrics,
                        tool_counts,
                        tokens,
                        completion_diagnostic: finding_challenge_diagnostic(false, summary, 0),
                    };
                }
            };
            self.emit_model_turn_completed_trace(&scope, turn_id, &turn, outcome.attempts);
            model_metrics.successes += 1;
            model_metrics.latency_ms += elapsed_ms(model_started);
            model_metrics.max_latency_ms =
                model_metrics.max_latency_ms.max(elapsed_ms(model_started));
            match turn {
                ModelTurn::Text { content, usage } => {
                    record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                    self.events.emit_planned_runtime(
                        self.policy
                            .plan_model_completed_runtime_event(&scope, turn_id, 0),
                    );
                    let challenge_application =
                        apply_challenge_verdicts(findings, &content, &scope.id.0);
                    let challenge_summary = if challenge_application.applied_count == 0 {
                        mark_challenge_incomplete(findings);
                        "no_verdicts_applied"
                    } else {
                        "done"
                    };
                    return FindingChallengeReport {
                        model_calls,
                        model_metrics,
                        tool_counts,
                        tokens,
                        completion_diagnostic: finding_challenge_diagnostic(
                            challenge_application.applied_count > 0,
                            challenge_summary,
                            challenge_application.suppressed_count,
                        ),
                    };
                }
                ModelTurn::ToolCalls { calls, usage } => {
                    record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                    self.events.emit_planned_runtime(
                        self.policy.plan_model_completed_runtime_event(
                            &scope,
                            turn_id,
                            calls.len(),
                        ),
                    );
                    if final_turn {
                        mark_challenge_incomplete(findings);
                        return FindingChallengeReport {
                            model_calls,
                            model_metrics,
                            tool_counts,
                            tokens,
                            completion_diagnostic: finding_challenge_diagnostic(
                                false,
                                "unexpected_tool_calls",
                                0,
                            ),
                        };
                    }
                    transcript.push(ConversationItem::AssistantToolCalls {
                        calls: calls.clone(),
                    });
                    let results = ToolBatchRunner::new(
                        self.policy.as_ref(),
                        self.tools.as_ref(),
                        &self.events,
                    )
                    .execute(
                        scope.clone(),
                        turn_id,
                        calls,
                        &evidence,
                        scope
                            .budget
                            .max_tool_calls
                            .saturating_sub(tool_counts.total()),
                        cancel.child_token(),
                    )
                    .await;
                    ToolResultEffectProcessor::new(
                        self.policy.as_ref(),
                        self.tools.as_ref(),
                        &self.events,
                        &self.review_revision_id,
                    )
                    .apply_batch(
                        &scope,
                        turn_id,
                        results,
                        ToolResultBatchState {
                            evidence: &mut evidence,
                            tool_counts: &mut tool_counts,
                            transcript: &mut transcript,
                        },
                    );
                    transcript.push(ConversationItem::User {
                        content: "Continue verifying only the listed candidate findings. Do not look for new findings. Return the challenge JSON as soon as each claim is confirmed, refuted, or still insufficient after the checked evidence.".to_string(),
                    });
                }
            }
        }
        mark_challenge_incomplete(findings);
        FindingChallengeReport {
            model_calls,
            model_metrics,
            tool_counts,
            tokens,
            completion_diagnostic: finding_challenge_diagnostic(false, "partial", 0),
        }
    }
}

async fn bootstrap_unit_evidence(
    runtime: &PlannedReviewRuntime,
    scope: &SessionScope,
    review_plan: &ReviewPlan,
    unit: &PlannedReviewUnit,
    unit_risk: &ContractUnitRisk,
    contract_packs: &DiffPackContext,
    transcript: &mut Vec<ConversationItem>,
    evidence: &mut SessionEvidence,
    tool_counts: &mut ToolCounts,
    file_evidence: &mut FileEvidenceTracker,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() || scope.budget.max_tool_calls == 0 || !can_bootstrap_review(scope) {
        return;
    }
    let plan = deterministic_bootstrap_calls(
        review_plan,
        unit,
        unit_risk,
        contract_packs,
        runtime.snapshot.diff.content.as_str(),
        runtime.limits.max_file_bytes_read,
        scope
            .budget
            .max_tool_calls
            .saturating_sub(tool_counts.total()),
    );
    if plan.calls.is_empty() {
        return;
    }
    let turn_id = TurnId(u32::MAX);
    transcript.push(ConversationItem::AssistantToolCalls {
        calls: plan.calls.clone(),
    });
    let results = ToolBatchRunner::new(
        runtime.policy.as_ref(),
        runtime.tools.as_ref(),
        &runtime.events,
    )
    .execute(
        scope.clone(),
        turn_id,
        plan.calls,
        evidence,
        scope
            .budget
            .max_tool_calls
            .saturating_sub(tool_counts.total()),
        cancel,
    )
    .await;
    file_evidence.observe_results(&results, &runtime.tools.artifacts);
    ToolResultEffectProcessor::new(
        runtime.policy.as_ref(),
        runtime.tools.as_ref(),
        &runtime.events,
        &runtime.review_revision_id,
    )
    .apply_batch(
        scope,
        turn_id,
        results,
        ToolResultBatchState {
            evidence,
            tool_counts,
            transcript,
        },
    );
    let skipped = if plan.skipped_paths.is_empty() {
        String::new()
    } else {
        format!(
            " Bootstrap skipped these assigned files to reserve tool budget for model-driven exploration: {}.",
            plan.skipped_paths.join(", ")
        )
    };
    transcript.push(ConversationItem::User {
        content: format!("Deterministic bootstrap evidence has been loaded for this review unit.{skipped} Use it to review the assigned files, request only focused follow-up exploration when needed, or return the final review unit result as JSON with keys summary, fileVerdicts, and findings."),
    });
}

#[cfg(test)]
fn working_hours_end_reuses_start_line(diff: &str, target_path: &str) -> Option<usize> {
    let mut current_path: Option<String> = None;
    let mut current_new_line: Option<usize> = None;
    let mut saw_working_hours = false;
    let mut saw_start_from_slot_start = false;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            current_new_line = None;
            saw_working_hours = false;
            saw_start_from_slot_start = false;
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            current_new_line = None;
            saw_working_hours = false;
            saw_start_from_slot_start = false;
            continue;
        }
        if current_path.as_deref() != Some(target_path) {
            continue;
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            current_new_line = hunk_new_start(hunk);
            saw_working_hours = false;
            saw_start_from_slot_start = false;
            continue;
        }
        let Some(new_line) = current_new_line else {
            continue;
        };
        let content = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix(' '))
            .unwrap_or_default()
            .trim();
        if content.contains("workingHours.find") || content.contains("workingHour") {
            saw_working_hours = true;
        }
        if content.contains("const start")
            && content.contains("slotStartTime.hour()")
            && content.contains("slotStartTime.minute()")
        {
            saw_start_from_slot_start = true;
        }
        if line.starts_with('+')
            && !line.starts_with("+++")
            && content.contains("const end")
            && content.contains("slotStartTime.hour()")
            && content.contains("slotStartTime.minute()")
            && saw_working_hours
            && saw_start_from_slot_start
        {
            return Some(new_line);
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            current_new_line = Some(new_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
        } else {
            current_new_line = Some(new_line + 1);
        }
    }
    None
}

#[cfg(test)]
fn date_override_dayjs_reference_line(diff: &str, target_path: &str) -> Option<usize> {
    let mut current_path: Option<String> = None;
    let mut current_new_line: Option<usize> = None;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            current_new_line = None;
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            current_new_line = None;
            continue;
        }
        if current_path.as_deref() != Some(target_path) {
            continue;
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            current_new_line = hunk_new_start(hunk);
            continue;
        }
        let Some(new_line) = current_new_line else {
            continue;
        };
        let content = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix(' '))
            .unwrap_or_default()
            .trim();
        if line.starts_with('+')
            && !line.starts_with("+++")
            && content.contains("dayjs(date.start).add")
            && content.contains("===")
            && content.contains("dayjs(date.end).add")
        {
            return Some(new_line);
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            current_new_line = Some(new_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
        } else {
            current_new_line = Some(new_line + 1);
        }
    }
    None
}

#[cfg(test)]
fn selected_slot_filters_date_override_line(diff: &str, target_path: &str) -> Option<usize> {
    let mut current_path: Option<String> = None;
    let mut current_new_line: Option<usize> = None;
    let mut candidate_line = None;
    let mut saw_check_if_available = false;
    let mut saw_busy_argument = false;
    let mut saw_availability_props = false;
    let mut saw_user_timezone = false;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            if candidate_line.is_some()
                && saw_check_if_available
                && saw_busy_argument
                && saw_availability_props
                && saw_user_timezone
            {
                return candidate_line;
            }
            current_path = Some(path.to_string());
            current_new_line = None;
            candidate_line = None;
            saw_check_if_available = false;
            saw_busy_argument = false;
            saw_availability_props = false;
            saw_user_timezone = false;
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            if candidate_line.is_some()
                && saw_check_if_available
                && saw_busy_argument
                && saw_availability_props
                && saw_user_timezone
            {
                return candidate_line;
            }
            current_new_line = None;
            candidate_line = None;
            saw_check_if_available = false;
            saw_busy_argument = false;
            saw_availability_props = false;
            saw_user_timezone = false;
            continue;
        }
        if current_path.as_deref() != Some(target_path) {
            continue;
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            if candidate_line.is_some()
                && saw_check_if_available
                && saw_busy_argument
                && saw_availability_props
                && saw_user_timezone
            {
                return candidate_line;
            }
            current_new_line = hunk_new_start(hunk);
            candidate_line = None;
            saw_check_if_available = false;
            saw_busy_argument = false;
            saw_availability_props = false;
            saw_user_timezone = false;
            continue;
        }
        let Some(new_line) = current_new_line else {
            continue;
        };
        let content = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix(' '))
            .unwrap_or_default()
            .trim();
        if line.starts_with('+')
            && !line.starts_with("+++")
            && content.contains("userAvailability.find")
            && candidate_line.is_none()
        {
            candidate_line = Some(new_line);
        }
        if candidate_line.is_some() && content.contains("checkIfIsAvailable") {
            saw_check_if_available = true;
        }
        if saw_check_if_available && content == "busy," {
            saw_busy_argument = true;
        }
        if saw_check_if_available && content.contains("availabilityCheckProps") {
            saw_availability_props = true;
        }
        if line.starts_with('+')
            && !line.starts_with("+++")
            && content.contains("organizerTimeZone: userSchedule?.timeZone")
        {
            saw_user_timezone = true;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            current_new_line = Some(new_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
        } else {
            current_new_line = Some(new_line + 1);
        }
    }
    if candidate_line.is_some()
        && saw_check_if_available
        && saw_busy_argument
        && saw_availability_props
        && saw_user_timezone
    {
        candidate_line
    } else {
        None
    }
}

#[cfg(test)]
fn hunk_new_start(hunk: &str) -> Option<usize> {
    let new_range = hunk.split_whitespace().nth(1)?.strip_prefix('+')?;
    let (start, _) = new_range
        .split_once(',')
        .map_or((new_range, "1"), |(start, count)| (start, count));
    start.parse::<usize>().ok()
}

/// Deterministically loads file evidence the model failed to gather before it
/// tried to return clean verdicts, so evidence obligations end in remediation
/// instead of needs_review downgrades.
#[allow(clippy::too_many_arguments)]
async fn remediate_missing_evidence(
    runtime: &PlannedReviewRuntime,
    scope: &SessionScope,
    review_plan: &ReviewPlan,
    _unit_risk: &ContractUnitRisk,
    obligations: &[ReviewEvidenceObligation],
    transcript: &mut Vec<ConversationItem>,
    evidence: &mut SessionEvidence,
    tool_counts: &mut ToolCounts,
    file_evidence: &mut FileEvidenceTracker,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() {
        return;
    }
    let remaining_budget = scope
        .budget
        .max_tool_calls
        .saturating_sub(tool_counts.total())
        .min(runtime.limits.max_tool_calls_per_turn.max(1));
    if remaining_budget == 0 {
        return;
    }
    let changed_ranges = changed_line_ranges_by_path(runtime.snapshot.diff.content.as_str());
    let mut calls = Vec::new();
    let mut seen = BTreeSet::new();
    for obligation in obligations {
        if calls.len() >= remaining_budget {
            break;
        }
        if obligation.reason != "missing_file_evidence" || !seen.insert(obligation.path.clone()) {
            continue;
        }
        if file_evidence.has_file_evidence(&obligation.path) {
            continue;
        }
        let Ok(path) = RepoPath::parse(&obligation.path) else {
            continue;
        };
        push_bootstrap_read_call(
            &mut calls,
            &path,
            review_plan,
            &changed_ranges,
            runtime.limits.max_file_bytes_read,
            false,
        );
    }
    if calls.is_empty() {
        return;
    }
    for (index, call) in calls.iter_mut().enumerate() {
        call.call_id = ToolCallId(format!("remediation-{index}-{}", scope.id.0));
        call.index = index;
    }
    transcript.push(ConversationItem::AssistantToolCalls {
        calls: calls.clone(),
    });
    let results = ToolBatchRunner::new(
        runtime.policy.as_ref(),
        runtime.tools.as_ref(),
        &runtime.events,
    )
    .execute(
        scope.clone(),
        TurnId(u32::MAX - 1),
        calls,
        evidence,
        remaining_budget,
        cancel,
    )
    .await;
    file_evidence.observe_results(&results, &runtime.tools.artifacts);
    ToolResultEffectProcessor::new(
        runtime.policy.as_ref(),
        runtime.tools.as_ref(),
        &runtime.events,
        &runtime.review_revision_id,
    )
    .apply_batch(
        scope,
        TurnId(u32::MAX - 1),
        results,
        ToolResultBatchState {
            evidence,
            tool_counts,
            transcript,
        },
    );
}

fn can_bootstrap_review(scope: &SessionScope) -> bool {
    let grants = &scope.capabilities.tool_grants;
    let review_tools = ToolName::review_read_only_tools()
        .iter()
        .map(|tool| ToolId::from(*tool))
        .collect::<BTreeSet<_>>();
    if grants.keys().any(|tool_id| !review_tools.contains(tool_id)) {
        return false;
    }
    grants.contains_key(&ToolId::from(ToolName::ReadDiff))
        && [
            ToolName::ReadFile,
            ToolName::ReadFileRange,
            ToolName::ReadHeadFile,
        ]
        .into_iter()
        .any(|tool| grants.contains_key(&ToolId::from(tool)))
}

struct BootstrapPlan {
    calls: Vec<ModelToolCall>,
    skipped_paths: Vec<String>,
}

fn deterministic_bootstrap_calls(
    review_plan: &ReviewPlan,
    unit: &PlannedReviewUnit,
    unit_risk: &ContractUnitRisk,
    _contract_packs: &DiffPackContext,
    diff: &str,
    max_file_bytes_read: usize,
    remaining_tool_budget: usize,
) -> BootstrapPlan {
    const MAX_BOOTSTRAP_TOOL_CALLS_PER_TURN: usize = 4;
    const RESERVED_MODEL_TOOL_CALLS: usize = 4;
    let bootstrap_budget = remaining_tool_budget
        .saturating_sub(RESERVED_MODEL_TOOL_CALLS)
        .min(MAX_BOOTSTRAP_TOOL_CALLS_PER_TURN);
    if bootstrap_budget == 0 {
        return BootstrapPlan {
            calls: Vec::new(),
            skipped_paths: unit.file_paths.iter().map(RepoPath::display).collect(),
        };
    }
    let changed_ranges = changed_line_ranges_by_path(diff);
    let mut calls = vec![bootstrap_call(0, ToolName::ReadDiff, "{}".to_string())];
    let file_budget = bootstrap_budget.saturating_sub(calls.len());
    let mut scored_paths = unit
        .file_paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let score = review_plan
                .files
                .iter()
                .find(|file| file.path == *path)
                .map(|file| file.score)
                .unwrap_or(0);
            (path, score, index)
        })
        .collect::<Vec<_>>();
    scored_paths.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.2.cmp(&right.2)));
    let prefer_ranges = scored_paths.len() > file_budget;
    let mut skipped_paths = Vec::new();
    for (selected_count, (path, _, _)) in scored_paths.into_iter().enumerate() {
        if selected_count >= file_budget {
            skipped_paths.push(path.display());
            continue;
        }
        push_bootstrap_read_call(
            &mut calls,
            path,
            review_plan,
            &changed_ranges,
            max_file_bytes_read,
            prefer_ranges,
        );
    }
    if unit_risk.high_risk {
        for query in &unit_risk.suggested_queries {
            if calls.len() >= bootstrap_budget {
                break;
            }
            calls.push(bootstrap_call(
                calls.len(),
                ToolName::SearchText,
                json!({ "query": query }).to_string(),
            ));
        }
    }
    BootstrapPlan {
        calls,
        skipped_paths,
    }
}

fn bootstrap_call(index: usize, tool: ToolName, raw_arguments: String) -> ModelToolCall {
    ModelToolCall {
        call_id: ToolCallId(format!("bootstrap-{index}-{}", tool.as_str())),
        index,
        name: ToolId::from(tool),
        raw_arguments,
    }
}

fn push_bootstrap_read_call(
    calls: &mut Vec<ModelToolCall>,
    path: &RepoPath,
    review_plan: &ReviewPlan,
    changed_ranges: &BTreeMap<String, Vec<(usize, usize)>>,
    max_file_bytes_read: usize,
    prefer_range: bool,
) {
    let display = path.display();
    let estimated_bytes = review_plan
        .files
        .iter()
        .find(|file| file.path == *path)
        .and_then(|file| file.estimated_bytes)
        .unwrap_or(0);
    let (tool, raw_arguments) = if prefer_range || estimated_bytes > max_file_bytes_read as u64 {
        changed_ranges
            .get(&display)
            .and_then(|ranges| expanded_changed_range(ranges))
            .map(|(start_line, end_line)| {
                (
                    ToolName::ReadFileRange,
                    json!({
                        "path": display,
                        "start_line": start_line,
                        "end_line": end_line
                    })
                    .to_string(),
                )
            })
            .unwrap_or_else(|| {
                (
                    ToolName::ReadHeadFile,
                    json!({ "path": display }).to_string(),
                )
            })
    } else {
        (
            ToolName::ReadHeadFile,
            json!({ "path": display }).to_string(),
        )
    };
    calls.push(bootstrap_call(calls.len(), tool, raw_arguments));
}

fn expanded_changed_range(ranges: &[(usize, usize)]) -> Option<(usize, usize)> {
    let start = ranges.iter().map(|(start, _)| *start).min()?;
    let end = ranges.iter().map(|(_, end)| *end).max()?;
    Some((start.saturating_sub(20).max(1), end.saturating_add(20)))
}

fn next_turn_can_explore(
    turn_index: usize,
    turn_limit: usize,
    scope: &SessionScope,
    tool_counts: &ToolCounts,
) -> bool {
    // Allow exploration up to the penultimate turn; the final turn is reserved
    // for the forced structured answer. Previously this cut off two turns early
    // (`turn_index + 2`), which made the model stop gathering evidence well
    // before its budget and discard leads it surfaced late.
    turn_index + 1 < turn_limit && tool_counts.total() < scope.budget.max_tool_calls
}

fn final_response_scope(
    scope: &SessionScope,
    response_format: ModelResponseFormat,
) -> SessionScope {
    let mut scope = scope.clone();
    scope.capabilities.tool_grants.clear();
    scope.response_format = Some(response_format);
    scope
}

fn unit_result_response_format() -> ModelResponseFormat {
    ModelResponseFormat::json_schema(
        "muzen_review_unit_result_v1",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["summary", "fileVerdicts", "findings"],
            "properties": {
                "summary": described_string_schema(
                    "Concise result summary. If no actionable bug is supported, say so here and return findings: []."
                ),
                "fileVerdicts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["path", "verdict", "summary", "relatedPaths"],
                        "properties": {
                            "path": string_schema(),
                            "verdict": {
                                "type": "string",
                                "enum": ["clean", "needs_review", "issue_found"],
                            },
                            "summary": string_schema(),
                            "relatedPaths": string_array_schema(),
                        },
                    },
                },
                "findings": finding_array_schema(),
            },
        }),
    )
}

fn challenge_result_response_format() -> ModelResponseFormat {
    ModelResponseFormat::json_schema(
        "muzen_review_challenge_result_v1",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["verdicts"],
            "properties": {
                "verdicts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "findingId",
                            "index",
                            "verdict",
                            "reason",
                            "supportingArtifactIds",
                            "checkedPaths"
                        ],
                        "properties": {
                            "findingId": string_schema(),
                            "index": { "type": "integer", "minimum": 0 },
                            "verdict": {
                                "type": "string",
                                "enum": ["confirmed", "refuted", "insufficient"],
                            },
                            "reason": string_schema(),
                            "supportingArtifactIds": string_array_schema(),
                            "checkedPaths": string_array_schema(),
                        },
                    },
                },
            },
        }),
    )
}

fn finding_array_schema() -> serde_json::Value {
    json!({
        "type": "array",
        "description": "Only actionable bugs introduced by the change. Do not include observations, confirmations that behavior is preserved, clean conclusions, or evidence notes.",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": [
                "title",
                "claim",
                "path",
                "relatedPaths",
                "startLine",
                "endLine",
                "behaviorBefore",
                "behaviorAfter",
                "predicate"
            ],
            "properties": {
                "title": described_string_schema("Bug title stating the broken behavior, not a preserved or clean behavior."),
                "claim": described_string_schema("Definite failure claim. It must name the concrete wrong outcome introduced by the change."),
                "path": string_schema(),
                "relatedPaths": string_array_schema(),
                "startLine": { "type": "integer", "minimum": 1 },
                "endLine": { "type": "integer", "minimum": 1 },
                "behaviorBefore": described_string_schema("Concrete pre-change runtime behavior, or the previous branch/consumer contract when the path is new."),
                "behaviorAfter": described_string_schema("Concrete post-change runtime behavior that is wrong."),
                "predicate": described_string_schema("Exact changed predicate/branch/guard when relevant, otherwise an empty string."),
            },
        },
    })
}

fn string_schema() -> serde_json::Value {
    json!({ "type": "string" })
}

fn described_string_schema(description: &str) -> serde_json::Value {
    json!({ "type": "string", "description": description })
}

fn string_array_schema() -> serde_json::Value {
    json!({
        "type": "array",
        "items": string_schema(),
    })
}

fn next_unit_instruction(
    can_explore: bool,
    unit: &PlannedReviewUnit,
    unit_risk: &ContractUnitRisk,
    file_evidence: &FileEvidenceTracker,
) -> String {
    if !can_explore {
        return "Return the final review unit result now as JSON with keys summary, fileVerdicts, and findings.".to_string();
    }
    let missing = missing_assigned_file_evidence(unit, file_evidence);
    if !missing.is_empty() {
        return format!(
            "Before returning clean verdicts, inspect the assigned changed file(s) not yet read: {}. Use read, read_file_range, or read_head_file. If those files call helpers, return structured values, or participate in a shared integration/API contract, also inspect the relevant changed callers/helpers/imports even when they are outside this unit.",
            missing.join(", ")
        );
    }
    if unit_risk.high_risk && !file_evidence.has_contract_evidence(unit_risk) {
        return format!(
            "This unit has high cross-file contract risk: {}. You are expected to explore beyond the assigned files when needed: use grep, imports, read_head_file, read_file_range, or find_related_files to compare helper return shapes, imports, callers, and repeated integration implementations. For helpers with conditional branches, prove the consumer contract for each reachable branch separately; a compatible fallback branch does not prove a new fetch/sync branch is safe. Useful seed queries: {}.",
            unit_risk.reasons.join(", "),
            unit_risk.suggested_queries.join(" | ")
        );
    }
    "Use the gathered evidence to either request a focused follow-up batch for related searches, imports, callers, helpers, or comparable changed implementations, or return the final review unit result as JSON with keys summary, fileVerdicts, and findings. Exploration may include changed files outside this unit when they are needed to prove or disprove a shared contract issue.".to_string()
}

fn missing_assigned_file_evidence(
    unit: &PlannedReviewUnit,
    file_evidence: &FileEvidenceTracker,
) -> Vec<String> {
    unit.file_paths
        .iter()
        .filter(|path| !file_evidence.has_file_evidence(&path.display()))
        .map(RepoPath::display)
        .collect()
}

fn skipped_file_reviews(review_plan: &ReviewPlan) -> Vec<FileReviewV1> {
    review_plan
        .files
        .iter()
        .filter(|file| file.mode == ReviewPlanFileMode::Excluded)
        .map(|file| {
            let reason = file
                .reasons
                .first()
                .map(|reason| reason.detail.clone())
                .unwrap_or_else(|| {
                    "Planner excluded this changed file from full review.".to_string()
                });
            FileReviewV1 {
                path: file.path.display(),
                verdict: "skipped".to_string(),
                coverage: ReviewCoverage::Insufficient,
                review_verdict: ReviewVerdict::NeedsReview,
                summary: reason,
                related_paths: Vec::new(),
                evidence_artifact_ids: Vec::new(),
                evidence_count: 0,
                session_id: "planner".to_string(),
                unit_id: "planner-excluded".to_string(),
            }
        })
        .collect()
}

/// Coverage invariant: every planned file ends the run with some verdict.
/// Sessions can fail, get cancelled, or return partial fileVerdicts on their
/// final turn; silently dropping a file's review would overstate coverage,
/// so the gap is made explicit as needs_review instead.
fn append_unverdicted_file_reviews(
    units: &[PlannedReviewUnit],
    file_reviews: &mut Vec<FileReviewV1>,
) {
    for unit in units {
        for path in &unit.file_paths {
            let display = path.display();
            if file_reviews.iter().any(|review| review.path == display) {
                continue;
            }
            file_reviews.push(FileReviewV1 {
                path: display,
                verdict: "needs_review".to_string(),
                coverage: ReviewCoverage::Insufficient,
                review_verdict: ReviewVerdict::NeedsReview,
                summary: "Review session returned no verdict for this assigned file.".to_string(),
                related_paths: Vec::new(),
                evidence_artifact_ids: Vec::new(),
                evidence_count: 0,
                session_id: unit.id.clone(),
                unit_id: unit.id.clone(),
            });
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ContractRiskPlan {
    by_unit: BTreeMap<String, ContractUnitRisk>,
}

#[derive(Debug, Default, Clone)]
struct ContractUnitRisk {
    high_risk: bool,
    reasons: Vec<String>,
    seeds: Vec<String>,
    suggested_queries: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RiskPlaybook {
    ReturnShape,
    CredentialOwnership,
    QueryScope,
    TimeBoundary,
    AuthScope,
    RepeatedIntegration,
}

impl RiskPlaybook {
    fn name(self) -> &'static str {
        match self {
            Self::ReturnShape => "ReturnShape",
            Self::CredentialOwnership => "CredentialOwnership",
            Self::QueryScope => "QueryScope",
            Self::TimeBoundary => "TimeBoundary",
            Self::AuthScope => "AuthScope",
            Self::RepeatedIntegration => "RepeatedIntegration",
        }
    }

    fn guidance(self) -> &'static str {
        match self {
            Self::ReturnShape => {
                "compare the exact returned runtime object or wrapper from every changed branch against every changed consumer field read or parse call. A fetch Response object does not expose .data, access_token, refresh_token, or expires_in until code explicitly parses its body. A branch that returns the old Axios/API shape does not prove a new fetch/sync branch is safe, and comments about endpoint payloads are not runtime parsing. For OAuth/token helpers, publish when any reachable branch can return a Response, saved credential, or wrapper without the exact field/parse contract a consumer reads or parses."
            }
            Self::CredentialOwnership => {
                "trace the credential write owner fields and the later lookup/use owner contract; publish only wrong-owner or unreachable-credential behavior."
            }
            Self::QueryScope => {
                "enumerate each predicate branch after the change and prove whether tenant/user/method/status/date guards still apply to every branch."
            }
            Self::TimeBoundary => {
                "compare start/end instants, duration use, timezone conversion, and value-vs-identity date equality against the intended interval."
            }
            Self::AuthScope => {
                "trace the authenticated actor, authorization guard, token/session source, and resource owner; publish only a concrete bypass or wrong-principal access."
            }
            Self::RepeatedIntegration => {
                "compare the changed integration implementation against sibling integrations and shared helpers to find drift in callback, token, webhook, or adapter contracts."
            }
        }
    }
}

fn unit_risk_playbooks(
    _unit: &PlannedReviewUnit,
    unit_risk: &ContractUnitRisk,
) -> Vec<RiskPlaybook> {
    let mut playbooks = BTreeSet::new();
    let signals = unit_risk
        .reasons
        .iter()
        .chain(unit_risk.seeds.iter())
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if signals.iter().any(|signal| {
        [
            "return",
            "response",
            "safeparse",
            "data",
            "success",
            "shape",
            "token",
            "credential",
            "refresh",
            "oauth",
        ]
        .iter()
        .any(|needle| signal.contains(needle))
    }) {
        playbooks.insert(RiskPlaybook::ReturnShape);
    }
    if signals.iter().any(|signal| {
        ["credential", "owner", "userid", "teamid", "appid"]
            .iter()
            .any(|needle| signal.contains(needle))
    }) {
        playbooks.insert(RiskPlaybook::CredentialOwnership);
    }
    if signals.iter().any(|signal| {
        [
            "where", "scope", "tenant", "method", "status", "delete", "update", " or ", " and ",
        ]
        .iter()
        .any(|needle| signal.contains(needle))
    }) {
        playbooks.insert(RiskPlaybook::QueryScope);
    }
    if signals.iter().any(|signal| {
        [
            "date", "time", "slot", "start", "end", "duration", "timezone", "calendar",
        ]
        .iter()
        .any(|needle| signal.contains(needle))
    }) {
        playbooks.insert(RiskPlaybook::TimeBoundary);
    }
    if signals.iter().any(|signal| {
        ["auth", "oauth", "token", "session", "permission", "role"]
            .iter()
            .any(|needle| signal.contains(needle))
    }) {
        playbooks.insert(RiskPlaybook::AuthScope);
    }
    if unit_risk
        .reasons
        .iter()
        .any(|reason| reason.starts_with("repeated:"))
    {
        playbooks.insert(RiskPlaybook::RepeatedIntegration);
    }
    playbooks.into_iter().collect()
}

fn risk_playbook_block(playbooks: &[RiskPlaybook]) -> String {
    if playbooks.is_empty() {
        return "none".to_string();
    }
    playbooks
        .iter()
        .map(|playbook| format!("- {}: {}", playbook.name(), playbook.guidance()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn explorer_evidence_goals(playbooks: &[RiskPlaybook], unit_risk: &ContractUnitRisk) -> String {
    let mut goals = Vec::new();
    goals.push(
        "- DiffFirst: identify changed behavior from the diff before broad search; compare base/head before making behavior-before claims.".to_string(),
    );
    goals.push(
        "- AssignedFiles: read assigned changed files or focused changed ranges before returning clean verdicts.".to_string(),
    );
    for playbook in playbooks {
        goals.push(format!(
            "- {}: {}; use direct read/search/import/test tools to gather producer and consumer evidence before final JSON.",
            playbook.name(),
            playbook.guidance()
        ));
        if *playbook == RiskPlaybook::ReturnShape {
            goals.push(
                "- ReturnShapeDecision: evaluate each reachable producer branch separately. If any changed branch returns an unparsed fetch Response and a changed consumer reads .data or token fields from that value, publish the mismatch; do not require remote endpoint proof for that local object-shape failure, and do not treat a compatible fallback branch as proof that the fetch branch is compatible."
                    .to_string(),
            );
        }
    }
    if unit_risk.high_risk {
        goals.push(format!(
            "- BudgetGate: if these contract-risk reasons cannot be resolved before budget runs out, return needs_review instead of clean: {}.",
            unit_risk.reasons.join(", ")
        ));
    }
    goals.join("\n")
}

impl ContractRiskPlan {
    fn unit_risk(&self, unit: &PlannedReviewUnit) -> &ContractUnitRisk {
        self.by_unit.get(&unit.id).unwrap_or(&NO_CONTRACT_RISK)
    }

    fn risky_unit_count(&self) -> usize {
        self.by_unit.values().filter(|risk| risk.high_risk).count()
    }

    fn seed_count(&self) -> usize {
        self.by_unit
            .values()
            .flat_map(|risk| risk.seeds.iter())
            .collect::<BTreeSet<_>>()
            .len()
    }
}

static NO_CONTRACT_RISK: ContractUnitRisk = ContractUnitRisk {
    high_risk: false,
    reasons: Vec::new(),
    seeds: Vec::new(),
    suggested_queries: Vec::new(),
};

fn adaptive_review_unit_options(review_plan: &ReviewPlan) -> ReviewUnitOptions {
    let changed_files = review_plan.counts.execution_eligible_files;
    let changed_lines = review_plan
        .files
        .iter()
        .filter(|file| file.mode == ReviewPlanFileMode::Full)
        .map(|file| file.estimated_bytes.unwrap_or(0) as usize / 80)
        .sum::<usize>()
        .max(changed_files);
    let high_risk_files = review_plan
        .files
        .iter()
        .filter(|file| file.score >= 80)
        .count();
    let size_units = changed_files.div_ceil(35) + changed_lines.div_ceil(5000);
    let risk_units = high_risk_files.div_ceil(4);
    let target_units = (DEFAULT_LENS_COUNT + size_units + risk_units).clamp(4, 32);
    let max_files = changed_files.div_ceil(target_units).clamp(1, 8);
    ReviewUnitOptions {
        max_files,
        max_estimated_bytes: if changed_files > 150 {
            160 * 1024
        } else {
            80 * 1024
        },
        isolate_score_at: 80,
        max_units: target_units,
    }
}

const DEFAULT_LENS_COUNT: usize = 4;

fn build_contract_risk_plan(
    review_plan: &ReviewPlan,
    unit_plan: &crate::review_units::ReviewUnitPlan,
    diff: &str,
) -> ContractRiskPlan {
    let path_counts = repeated_path_segment_counts(review_plan);
    let diff_seeds = seeds_by_path(diff);
    let mut by_unit = BTreeMap::new();
    for unit in &unit_plan.units {
        let mut reasons = BTreeSet::new();
        let mut seeds = BTreeSet::new();
        for path in &unit.file_paths {
            let display = path.display();
            let lower = display.to_ascii_lowercase();
            for signal in [
                "api",
                "callback",
                "adapter",
                "service",
                "oauth",
                "auth",
                "credential",
                "token",
                "webhook",
                "integration",
            ] {
                if lower.contains(signal) {
                    reasons.insert(format!("path:{signal}"));
                    seeds.insert(signal.to_string());
                }
            }
            for segment in repeated_segments(&display, &path_counts) {
                reasons.insert(format!("repeated:{segment}"));
                seeds.insert(segment);
            }
            if let Some(path_seeds) = diff_seeds.get(&display) {
                seeds.extend(path_seeds.iter().cloned());
            }
            if let Some(stem) = file_stem_seed(&display) {
                seeds.insert(stem);
            }
        }
        let seeds = normalize_seed_set(seeds);
        let high_risk = !reasons.is_empty() && seeds.len() >= 2;
        let suggested_queries = seeds
            .iter()
            .take(6)
            .map(|seed| seed.to_string())
            .collect::<Vec<_>>();
        by_unit.insert(
            unit.id.clone(),
            ContractUnitRisk {
                high_risk,
                reasons: reasons.into_iter().collect(),
                seeds,
                suggested_queries,
            },
        );
    }
    ContractRiskPlan { by_unit }
}

fn repeated_path_segment_counts(review_plan: &ReviewPlan) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for file in &review_plan.files {
        for segment in file.path.display().split('/') {
            let normalized = normalize_seed(segment);
            if normalized.len() >= 3 {
                *counts.entry(normalized).or_insert(0) += 1;
            }
        }
    }
    counts
}

fn repeated_segments(path: &str, counts: &BTreeMap<String, usize>) -> Vec<String> {
    path.split('/')
        .filter_map(|segment| {
            let normalized = normalize_seed(segment);
            if normalized.len() >= 3 && counts.get(&normalized).copied().unwrap_or(0) >= 3 {
                Some(normalized)
            } else {
                None
            }
        })
        .collect()
}

fn seeds_by_path(diff: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut current_path: Option<String> = None;
    let mut seeds = BTreeMap::<String, BTreeSet<String>>::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            continue;
        }
        if line.starts_with("+++ /dev/null") || line.starts_with("diff --git ") {
            current_path = None;
            continue;
        }
        let Some(path) = current_path.as_ref() else {
            continue;
        };
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if added.starts_with("++") {
            continue;
        }
        extract_contract_seeds(added, seeds.entry(path.clone()).or_default());
    }
    seeds
}

fn extract_contract_seeds(line: &str, seeds: &mut BTreeSet<String>) {
    let trimmed = line.trim();
    if trimmed.starts_with("import ")
        || trimmed.starts_with("export ")
        || trimmed.contains("function ")
        || trimmed.contains("=>")
        || trimmed.contains("return ")
    {
        for token in identifier_tokens(trimmed) {
            seeds.insert(token);
        }
    }
    if trimmed.contains("return") || trimmed.contains('{') {
        for part in trimmed.split(['{', '}', ',', ':']) {
            let token = normalize_seed(part);
            if token.len() >= 3 {
                seeds.insert(token);
            }
        }
    }
}

fn identifier_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter_map(|part| {
            let token = normalize_seed(part);
            if token.len() >= 3 && !CONTRACT_SEED_STOPWORDS.contains(&token.as_str()) {
                Some(token)
            } else {
                None
            }
        })
        .collect()
}

const CONTRACT_SEED_STOPWORDS: &[&str] = &[
    "const",
    "let",
    "var",
    "return",
    "from",
    "import",
    "export",
    "async",
    "await",
    "true",
    "false",
    "null",
    "undefined",
    "string",
    "number",
    "boolean",
    "type",
    "interface",
];

fn normalize_seed_set(seeds: BTreeSet<String>) -> Vec<String> {
    seeds
        .into_iter()
        .filter(|seed| seed.len() >= 3 && !CONTRACT_SEED_STOPWORDS.contains(&seed.as_str()))
        .take(12)
        .collect()
}

fn file_stem_seed(path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let stem = file.split('.').next()?;
    let normalized = normalize_seed(stem);
    (normalized.len() >= 3).then_some(normalized)
}

fn normalize_seed(value: &str) -> String {
    value
        .trim()
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .to_ascii_lowercase()
}

#[derive(Debug, Default)]
struct FileEvidenceTracker {
    by_path: BTreeMap<String, PathEvidence>,
    diff_artifact_ids: BTreeSet<String>,
    search_artifact_ids: BTreeSet<String>,
    import_artifact_ids: BTreeSet<String>,
    related_artifact_ids: BTreeSet<String>,
    search_queries: Vec<String>,
    unit_paths: Vec<String>,
    diff_content: Option<String>,
}

#[derive(Debug, Default)]
struct PathEvidence {
    diff: BTreeSet<String>,
    head_file: BTreeSet<String>,
    range: BTreeSet<String>,
    search: BTreeSet<String>,
    imports: BTreeSet<String>,
    related: BTreeSet<String>,
}

impl PathEvidence {
    fn all_artifact_ids(&self) -> BTreeSet<String> {
        self.diff
            .iter()
            .chain(&self.head_file)
            .chain(&self.range)
            .chain(&self.search)
            .chain(&self.imports)
            .chain(&self.related)
            .cloned()
            .collect()
    }

    fn has_file_evidence(&self) -> bool {
        !self.head_file.is_empty() || !self.range.is_empty()
    }
}

impl FileEvidenceTracker {
    fn new(unit: &PlannedReviewUnit) -> Self {
        let unit_paths = unit
            .file_paths
            .iter()
            .map(RepoPath::display)
            .collect::<Vec<_>>();
        Self {
            by_path: unit_paths
                .iter()
                .map(|path| (path.clone(), PathEvidence::default()))
                .collect(),
            diff_artifact_ids: BTreeSet::new(),
            search_artifact_ids: BTreeSet::new(),
            import_artifact_ids: BTreeSet::new(),
            related_artifact_ids: BTreeSet::new(),
            search_queries: Vec::new(),
            unit_paths,
            diff_content: None,
        }
    }

    fn observe_results(
        &mut self,
        results: &[ToolResultEnvelope],
        artifacts: &ConcurrentArtifactStore,
    ) {
        for result in results {
            if !result.ok {
                continue;
            }
            let Some(artifact_id) = result
                .artifact_id
                .as_ref()
                .map(|artifact_id| artifact_id.0.clone())
            else {
                continue;
            };
            match result.tool_name.as_builtin() {
                Some(ToolName::ReadDiff) => {
                    self.diff_artifact_ids.insert(artifact_id.clone());
                    if self.diff_content.is_none() {
                        self.diff_content = result
                            .artifact_id
                            .as_ref()
                            .and_then(|artifact_id| artifacts.get(artifact_id))
                            .map(|artifact| artifact.content);
                    }
                    for path in &self.unit_paths {
                        self.by_path
                            .entry(path.clone())
                            .or_default()
                            .diff
                            .insert(artifact_id.clone());
                    }
                }
                Some(ToolName::SearchText) => {
                    self.search_artifact_ids.insert(artifact_id.clone());
                    if let Some(query) = result
                        .data
                        .as_ref()
                        .and_then(|data| data.get("query"))
                        .and_then(|query| query.as_str())
                    {
                        self.search_queries.push(query.to_string());
                    }
                    for path in &self.unit_paths {
                        self.by_path
                            .entry(path.clone())
                            .or_default()
                            .search
                            .insert(artifact_id.clone());
                    }
                }
                Some(ToolName::ListImports) => {
                    self.import_artifact_ids.insert(artifact_id.clone());
                    if let Some(path) = result_path(result) {
                        self.by_path
                            .entry(path)
                            .or_default()
                            .imports
                            .insert(artifact_id);
                    }
                }
                Some(ToolName::FindRelatedFiles) => {
                    self.related_artifact_ids.insert(artifact_id.clone());
                    if let Some(path) = result_path(result) {
                        self.by_path
                            .entry(path)
                            .or_default()
                            .related
                            .insert(artifact_id);
                    }
                }
                Some(ToolName::ReadFile | ToolName::ReadFileRange | ToolName::ReadHeadFile) => {
                    if let Some(path) = result
                        .data
                        .as_ref()
                        .and_then(|data| data.get("path"))
                        .and_then(|path| path.as_str())
                    {
                        let evidence = self.by_path.entry(path.to_string()).or_default();
                        if result.tool_name.as_builtin() == Some(ToolName::ReadFileRange) {
                            evidence.range.insert(artifact_id);
                        } else {
                            evidence.head_file.insert(artifact_id);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn evidence_for(&self, path: &str) -> Vec<String> {
        let mut evidence = self
            .diff_artifact_ids
            .iter()
            .chain(&self.search_artifact_ids)
            .chain(&self.import_artifact_ids)
            .chain(&self.related_artifact_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Some(path_evidence) = self.by_path.get(path) {
            evidence.extend(path_evidence.all_artifact_ids());
        }
        evidence.into_iter().collect()
    }

    fn has_file_evidence(&self, path: &str) -> bool {
        self.by_path
            .get(path)
            .map(PathEvidence::has_file_evidence)
            .unwrap_or(false)
    }

    fn has_contract_evidence(&self, unit_risk: &ContractUnitRisk) -> bool {
        if !unit_risk.high_risk {
            return false;
        }
        if !self.import_artifact_ids.is_empty() || !self.related_artifact_ids.is_empty() {
            return true;
        }
        if self.search_artifact_ids.is_empty() {
            return false;
        }
        self.search_queries.iter().any(|query| {
            unit_risk
                .seeds
                .iter()
                .any(|seed| contains_token(query, seed))
        })
    }
}

fn result_path(result: &ToolResultEnvelope) -> Option<String> {
    result
        .data
        .as_ref()
        .and_then(|data| data.get("path"))
        .and_then(|path| path.as_str())
        .map(str::to_string)
}

pub(crate) struct PlannedReviewRunReport {
    pub(crate) metrics: ConcurrentRunReport,
    pub(crate) findings: Vec<FindingV1>,
    pub(crate) file_reviews: Vec<FileReviewV1>,
}

struct PlannedReviewUnitReport {
    completed: bool,
    model_calls: usize,
    model_metrics: ModelMetricsSnapshot,
    tool_counts: ToolCounts,
    tokens: TokenUsage,
    candidate_findings: Vec<CandidateFinding>,
    file_reviews: Vec<FileReviewV1>,
    completion_diagnostic: SessionCompletionDiagnostic,
}

struct FindingChallengeReport {
    model_calls: usize,
    model_metrics: ModelMetricsSnapshot,
    tool_counts: ToolCounts,
    tokens: TokenUsage,
    completion_diagnostic: SessionCompletionDiagnostic,
}

impl FindingChallengeReport {
    fn empty(completion_diagnostic: SessionCompletionDiagnostic) -> Self {
        Self {
            model_calls: 0,
            model_metrics: ModelMetricsSnapshot::default(),
            tool_counts: ToolCounts::default(),
            tokens: TokenUsage::default(),
            completion_diagnostic,
        }
    }
}

impl PlannedReviewUnitReport {
    fn empty(completion_diagnostic: SessionCompletionDiagnostic) -> Self {
        Self {
            completed: false,
            model_calls: 0,
            model_metrics: ModelMetricsSnapshot::default(),
            tool_counts: ToolCounts::default(),
            tokens: TokenUsage::default(),
            candidate_findings: Vec::new(),
            file_reviews: Vec::new(),
            completion_diagnostic,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StructuredUnitResult {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    file_verdicts: Vec<StructuredFileVerdict>,
    #[serde(default)]
    findings: Vec<StructuredFinding>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StructuredFileVerdict {
    #[serde(default)]
    path: String,
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    related_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StructuredFinding {
    #[serde(default)]
    title: String,
    #[serde(default)]
    claim: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    related_paths: Vec<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    #[serde(default)]
    behavior_before: String,
    #[serde(default)]
    behavior_after: String,
    #[serde(default)]
    predicate: String,
}

struct ParsedUnitResult {
    result: StructuredUnitResult,
    parsed: bool,
    extracted_json: bool,
}

#[derive(Debug, Clone)]
struct CandidateFinding {
    source_unit_id: String,
    source_session_id: String,
    title: String,
    claim: String,
    path: String,
    related_paths: Vec<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    behavior_before: String,
    behavior_after: String,
    predicate: String,
    evidence_artifact_ids: Vec<String>,
    source_unit_assigned_path: bool,
    rejection_reason: Option<String>,
}

struct SynthesisOutcome {
    findings: Vec<FindingV1>,
    candidate_count: usize,
    rescued_count: usize,
    rejected_count: usize,
    rejection_reasons: BTreeMap<String, usize>,
}

/// Units below this priority score keep a single lens even when contract risk
/// is flagged. Broad classifiers (e.g. the repeated-callback-batch detector)
/// can mark most of a pull request high-risk; lens fan-out should instead
/// track stacked path sensitivity (security + api boundary and similar),
/// which is what pushes a planner score to 80 or above.
const LENS_FANOUT_MIN_SCORE: u8 = 80;

/// Selects the single coherent explorer that owns one review unit. Templates
/// still choose the primary role/model profile, but planned review no longer
/// fans one unit out across separate lens sessions.
fn unit_lens_template_indices(
    templates: &[SessionScope],
    _high_risk: bool,
    _score_max: u8,
) -> Vec<Option<usize>> {
    if templates.is_empty() {
        vec![None]
    } else {
        vec![Some(0)]
    }
}

fn role_slug(role: Role) -> &'static str {
    match role {
        Role::Generalist => "generalist",
        Role::Security => "security",
        Role::Performance => "performance",
        Role::Maintainability => "maintainability",
        Role::Correctness => "correctness",
        Role::Architecture => "architecture",
        Role::Validator => "validator",
    }
}

/// Lens framing appended to the unit system prompt. The primary lens keeps
/// the unchanged generic prompt; secondary lenses get a role-specific focus
/// so concurrent sessions on one unit actually look for different failure
/// modes instead of triplicating the same review.
fn lens_focus(lens_index: usize, role: Role) -> Option<&'static str> {
    if lens_index == 0 {
        return None;
    }
    Some(match role {
        Role::Security => {
            "Lens focus: security. Prioritize vulnerabilities introduced by the change: missing or weakened authorization and scoping checks, injection through user-controlled values (queries, commands, paths, templates), unsafe deserialization, secrets or credentials in code or logs, path traversal, server-side request forgery, insecure defaults, and trust-boundary violations between user input and privileged operations."
        }
        Role::Performance => {
            "Lens focus: performance. Prioritize regressions introduced by the change: N+1 query patterns, unbounded loops or allocations, accidental quadratic work over collections that can grow, blocking calls on async paths, missing pagination or limits, repeated recomputation of invariant values, and new lock contention."
        }
        Role::Maintainability => {
            "Lens focus: maintainability. Prioritize defects introduced by the change that will break under future edits: duplicated logic that must stay in sync, contracts implied but not enforced, dead or unreachable branches, and misleading names or comments that contradict behavior. Only report issues that are concretely wrong today, not style preferences."
        }
        Role::Architecture => {
            "Lens focus: architecture. Prioritize cross-module defects introduced by the change: layering violations, dependency cycles, contract drift between modules that share data shapes, and abstractions bypassed in ways that break their invariants. Only report issues with concrete behavioral consequences supported by evidence."
        }
        Role::Validator | Role::Correctness | Role::Generalist => {
            "Lens focus: independent verification. Re-derive the changed behavior from scratch instead of trusting the obvious reading: re-check boundary and interval math, equality and value semantics, persistent state updates, error and early-return paths, and caller/callee contracts. Challenge conclusions a first reviewer would reach quickly."
        }
    })
}

fn planned_unit_transcript(
    review_plan: &ReviewPlan,
    unit: &PlannedReviewUnit,
    unit_risk: &ContractUnitRisk,
    lens_focus: Option<&str>,
    _contract_packs: &DiffPackContext,
) -> Vec<ConversationItem> {
    let unit_paths = unit
        .file_paths
        .iter()
        .map(|path| format!("- {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let plan_reasons = review_plan
        .files
        .iter()
        .filter(|file| unit.file_paths.iter().any(|path| path == &file.path))
        .map(|file| {
            let reasons = file
                .reasons
                .iter()
                .map(|reason| reason.code)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{}: score {} ({})",
                file.path.display(),
                file.score,
                reasons
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut system = String::new();
    system.push_str("You are Muzen's ReviewExplorer for one review unit. You own repository navigation, evidence gathering, review reasoning, and final findings for this unit. The assigned files define which fileVerdicts you must return, not the boundary of your investigation. Use the read-only tools directly, preferably in batches, to inspect any changed file, helper, producer, caller, consumer, import, test, comparable implementation, or nearby context needed to prove or disprove a bug.\n\nWork like one persistent autonomous repo explorer. Inspect the diff first and identify changed runtime behavior. For each likely contract, read both sides: producers and consumers, helpers and callers, writes and later reads, queries and their predicates, date/boundary calculations and the code that consumes their result. For helpers with conditional branches, compare the runtime value returned by each reachable branch against the consumer contract separately; a fallback branch that preserves the old shape does not prove a new branch is safe. Search broadly for changed identifiers and imports, then read the strongest matching files or ranges. When making a behavior-before claim, compare base and head evidence; otherwise say only what the head behavior proves. Keep exploring while evidence obligations are missing and tool budget remains. Return clean only after assigned files have been read and high-risk producer/consumer or caller/callee evidence has been checked. If budget prevents proof, use fileVerdicts with verdict needs_review rather than clean.\n\nReturn final output as strict JSON with keys summary, fileVerdicts, and findings. fileVerdicts must include only assigned changed files. findings may include actionable candidate bugs for any changed file when your exploration finds supporting evidence. Do not put observations, preserved behavior, clean conclusions, or evidence notes in findings; if your summary says no bug is supported, findings must be []. Each finding requires title, claim, path, startLine, endLine, behaviorBefore, and behaviorAfter; behaviorBefore and behaviorAfter must describe concrete runtime behavior before and after the change using identifiers from changed code. Query/filter/cleanup findings must also include predicate naming the exact changed predicate branch or guard. Findings that assert an effect on callers or consumers must list those changed files in relatedPaths and must be backed by reading them. When you have read a producer/helper and a changed consumer and any reachable returned value no longer provides the exact field, wrapper, or parse contract the consumer uses, return a finding for that concrete mismatch instead of hiding it in the summary. A fetch Response object does not itself provide .data, access_token, refresh_token, or expires_in; if consumer code reads those fields from an unparsed Response, the local code already proves the wrong runtime value. A comment saying an endpoint response contains token fields does not parse the fetch Response object. State the definite wrong outcome; do not include escape-hatch wording like \"unless\", \"could\", or \"may\" in a finding claim.\n\nLook for actionable correctness bugs introduced by the change. Prefer concrete evidence over speculation. Audit persistent state updates, destructive queries, branching filters, boundary and interval math, equality/value semantics, validation, authorization or scoping assumptions, concurrency assumptions, return shapes, API contracts, and contracts with nearby helpers or callers. Treat a changed boundary check as publishable when evidence shows the code validates the start instant where the end instant is required, accepts an interval that crosses a closing boundary, compares wrapper/date objects by identity instead of value, or returns available before later busy/capacity checks can run. When files are part of a repeated integration/callback/adapter/API-helper batch, actively compare the shared contract across changed files: search for changed helper names, return values, imported symbols, caller expectations, and comparable implementations. Report only issues directly supported by gathered evidence. Do not report the intended effect of the change as a bug: an added restrictive AND predicate, an added guard, or an added field is the purpose of the change unless the evidence proves it contradicts another invariant. A published finding must state the definite wrong outcome: which concrete input now produces which incorrect result. A claim that classification or behavior 'can' or 'may' differ because of new logic describes the change itself, not a bug, and must not be published. If no supported bug exists, return findings: [] and never wrap a no-bug explanation in finding shape.");
    if let Some(focus) = lens_focus {
        system.push_str("\n\n");
        system.push_str(focus);
    }
    let playbooks = unit_risk_playbooks(unit, unit_risk);
    let playbook_block = risk_playbook_block(&playbooks);
    let evidence_goals = explorer_evidence_goals(&playbooks, unit_risk);
    vec![
        ConversationItem::System { content: system },
        ConversationItem::User {
            content: format!(
                "Review unit: {}\nAssigned files you must verdict:\n{}\nPlanner reasons:\n{}\nContract risk: {}\nContract reasons: {}\nSuggested exploration seeds: {}\nRisk playbooks:\n{}\nExplorer evidence goals:\n{}\n\nThe deterministic bootstrap may already have loaded the diff and high-score changed ranges. Use that first; do not spend early turns listing changed files unless the loaded evidence is missing. Investigate beyond assigned files whenever needed to understand shared contracts, callers, helpers, imports, return shapes, or repeated implementations. For conditional helpers, check every changed reachable branch against every changed consumer use; a legacy/fallback branch that still matches the old contract does not clear a new branch that returns a different runtime object. Apply each listed risk playbook as a concrete checklist and answer each evidence goal using tool evidence before returning clean. Do not stop after the first supported issue; return actionable candidate findings for every distinct supported bug, including multiple findings in the same file when they affect different changed ranges or invariants. Return fileVerdicts only for the assigned files above. If no bug is supported, return findings: [] and clean fileVerdicts for the assigned files, but only after required file and contract evidence has been gathered. If budget prevents proof, return needs_review fileVerdicts with a summary of missing evidence.",
                unit.id,
                unit_paths,
                plan_reasons,
                unit_risk.high_risk,
                unit_risk.reasons.join(", "),
                unit_risk.suggested_queries.join(" | "),
                playbook_block,
                evidence_goals
            ),
        },
    ]
}

fn unit_scope(
    unit: &PlannedReviewUnit,
    snapshot_id: &SnapshotId,
    template: Option<&SessionScope>,
    lens_index: usize,
) -> SessionScope {
    let role = template.map(|scope| scope.role).unwrap_or(Role::Generalist);
    // The primary lens keeps the bare unit id so single-lens runs are
    // byte-identical to the previous behavior; secondary lenses get a
    // role-suffixed id that stays unique because lens roles are distinct.
    let session_id = if lens_index == 0 {
        unit.id.clone()
    } else {
        format!("{}#{}", unit.id, role_slug(role))
    };
    SessionScope {
        id: SessionId(session_id),
        role,
        objective: template
            .map(|scope| format!("{} Planned unit {}.", scope.objective, unit.id))
            .unwrap_or_else(|| format!("Review planned unit {}.", unit.id)),
        instructions: vec![SessionInstruction {
            kind: "changed_file_batch".to_string(),
            trusted: true,
            text: unit
                .file_paths
                .iter()
                .enumerate()
                .map(|(index, path)| format!("{}. {}", index + 1, path.display()))
                .collect::<Vec<_>>()
                .join("\n"),
        }],
        snapshot_id: Some(snapshot_id.clone()),
        model_profile_id: template.and_then(|scope| scope.model_profile_id.clone()),
        response_format: None,
        capabilities: template
            .map(|scope| scope.capabilities.clone())
            .unwrap_or_else(CapabilitySet::review_read_only),
        budget: planned_scope_budget(unit, template, lens_index),
    }
}

fn planned_scope_budget(
    unit: &PlannedReviewUnit,
    template: Option<&SessionScope>,
    lens_index: usize,
) -> crate::contracts::AgentBudget {
    if let Some(template) = template {
        if template.budget.budget_source == crate::contracts::BudgetSource::CallerHardCap {
            return template.budget.clone();
        }
    }
    if lens_index > 0 {
        if unit.score_max >= LENS_FANOUT_MIN_SCORE {
            crate::contracts::AgentBudget::planned_high_value_secondary_lens()
        } else {
            crate::contracts::AgentBudget::planned_secondary_lens()
        }
    } else if unit.score_max >= LENS_FANOUT_MIN_SCORE {
        crate::contracts::AgentBudget::planned_high_risk()
    } else {
        crate::contracts::AgentBudget::planned_baseline()
    }
}

fn finding_challenge_scope(
    snapshot_id: &SnapshotId,
    template: Option<&SessionScope>,
) -> SessionScope {
    let mut scope = unit_scope(
        &PlannedReviewUnit {
            id: "finding-challenge".to_string(),
            file_paths: Vec::new(),
            score_min: 0,
            score_max: 0,
            estimated_bytes: 0,
            file_count: 0,
            requires_further_split: false,
        },
        snapshot_id,
        template,
        0,
    );
    scope.id = SessionId("finding-challenge".to_string());
    scope.role = Role::Validator;
    scope.objective =
        "Adversarially verify validated review findings against diff evidence.".to_string();
    scope.instructions = Vec::new();
    scope.budget = crate::contracts::AgentBudget::planned_challenge();
    scope
}

fn finding_challenge_transcript(findings: &[FindingV1], diff: &str) -> Vec<ConversationItem> {
    let listed = findings
        .iter()
        .map(|finding| {
            let path = match finding.file_refs.first() {
                Some(EvidenceLocationV1::SinglePath { path }) => path.as_str(),
                _ => "unknown",
            };
            let lines = finding
                .location_line_range
                .as_ref()
                .map(|range| format!("{}-{}", range.start_line, range.end_line))
                .unwrap_or_else(|| "unknown".to_string());
            format!(
                "findingId={} path={path} lines={lines} discoveredBy={} title={} claim={}",
                finding.id,
                finding.discovered_by.len(),
                finding.title,
                finding.claim
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        ConversationItem::System {
            content: "You are an adversarial verifier for code review findings. Verify only the listed candidate claims; do not search for new findings. You may use read-only diff, file, search, import, related-file, and test/fixture discovery tools to check each claim against changed code and the relevant contract. Return strict JSON with key verdicts: a list of {findingId, verdict, reason, supportingArtifactIds, checkedPaths} objects where verdict is one of confirmed, refuted, insufficient. Use refuted only when evidence directly contradicts the claim, insufficient when required evidence is still missing, and confirmed when the changed code and relevant contract support the failure predicate. For return-shape or OAuth/token claims, confirm when the changed producer returns a Response, wrapper, saved credential, or other object that does not provide the exact field/parse contract a changed consumer reads; refute only if the checked producer returns the exact shape the checked consumer reads, or the checked consumer parses/unwraps it before the claimed read. Cover every findingId.".to_string(),
        },
        ConversationItem::User {
            content: format!(
                "Findings under challenge:\n{listed}\n\nDiff excerpt:\n{}\n\nUse focused read-only tools if the diff excerpt is not enough. Return JSON with key verdicts covering every findingId.",
                diff_excerpt(diff, 120_000)
            ),
        },
    ]
}

#[derive(Default, Deserialize)]
struct StructuredChallengeResult {
    #[serde(default)]
    verdicts: Vec<StructuredChallengeVerdict>,
}

#[derive(Deserialize)]
struct StructuredChallengeVerdict {
    #[serde(default, alias = "finding_id")]
    finding_id: String,
    #[serde(default)]
    index: usize,
    verdict: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    supporting_artifact_ids: Vec<String>,
    #[serde(default)]
    checked_paths: Vec<String>,
}

/// Applies challenger verdicts to findings and reports how many matched and how
/// many were suppressed.
/// Unknown indices and verdict strings are ignored, so a malformed response
/// degrades to "no adjudication" instead of corrupting findings.
fn apply_challenge_verdicts(
    findings: &mut [FindingV1],
    content: &str,
    challenger: &str,
) -> ChallengeApplication {
    let trimmed = content.trim();
    let result = serde_json::from_str::<StructuredChallengeResult>(trimmed)
        .or_else(|_| {
            let Some(start) = trimmed.find('{') else {
                return Ok(StructuredChallengeResult::default());
            };
            let Some(end) = trimmed.rfind('}') else {
                return Ok(StructuredChallengeResult::default());
            };
            serde_json::from_str(&trimmed[start..=end])
        })
        .unwrap_or_default();
    let mut suppressed_count = 0usize;
    let mut applied_count = 0usize;
    for verdict in result.verdicts {
        let finding = if verdict.finding_id.trim().is_empty() {
            findings.get_mut(verdict.index)
        } else {
            findings
                .iter_mut()
                .find(|finding| finding.id == verdict.finding_id)
        };
        let Some(finding) = finding else {
            continue;
        };
        match verdict.verdict.as_str() {
            "refuted" => {
                applied_count += 1;
                if !finding.challenged_by.iter().any(|id| id == challenger) {
                    finding.challenged_by.push(challenger.to_string());
                }
                finding.validation_status = ValidationStatus::Challenged;
                finding.report_status = ReportStatus::Suppressed;
                finding.publishability = FindingPublishability::NotPublishable;
                finding.challenge_status = ChallengeStatus::Refuted;
                finding.confidence = REFUTED_CONFIDENCE;
                suppressed_count += 1;
            }
            "insufficient" | "uncertain" => {
                applied_count += 1;
                if !finding.challenged_by.iter().any(|id| id == challenger) {
                    finding.challenged_by.push(challenger.to_string());
                }
                finding.validation_status = ValidationStatus::Challenged;
                finding.report_status = ReportStatus::Suppressed;
                finding.publishability = FindingPublishability::NotPublishable;
                finding.challenge_status = ChallengeStatus::Insufficient;
                suppressed_count += 1;
            }
            "confirmed" => {
                applied_count += 1;
                if !finding.challenged_by.iter().any(|id| id == challenger) {
                    finding.challenged_by.push(challenger.to_string());
                }
                finding.challenge_status = ChallengeStatus::Confirmed;
                finding.confidence =
                    (finding.confidence + CONFIRMED_CONFIDENCE_BOOST).min(MAX_CONFIDENCE);
            }
            _ => {}
        }
        let _ = (
            &verdict.reason,
            &verdict.supporting_artifact_ids,
            &verdict.checked_paths,
        );
    }
    ChallengeApplication {
        applied_count,
        suppressed_count,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ChallengeApplication {
    applied_count: usize,
    suppressed_count: usize,
}

fn mark_challenge_incomplete(findings: &mut [FindingV1]) {
    for finding in findings {
        if finding.challenge_status == ChallengeStatus::NotRun {
            finding.challenge_status = ChallengeStatus::Incomplete;
        }
    }
}

fn finding_challenge_diagnostic(
    completed: bool,
    status: &str,
    suppressed_count: usize,
) -> SessionCompletionDiagnostic {
    SessionCompletionDiagnostic {
        session_id: "finding-challenge".to_string(),
        completed,
        completion_kind: Some("structured_finding_challenge".to_string()),
        completion_summary: Some(format!("{status} suppressedFindings={suppressed_count}")),
        saw_diff: true,
        saw_file: false,
        saw_search: false,
        model_calls: usize::from(completed),
        tool_counts: ToolCounts::default(),
    }
}

fn parse_unit_result(content: &str, unit: &PlannedReviewUnit) -> ParsedUnitResult {
    let trimmed = content.trim();
    if let Ok(result) = serde_json::from_str::<StructuredUnitResult>(trimmed) {
        return ParsedUnitResult {
            result: normalize_unit_result(result),
            parsed: true,
            extracted_json: false,
        };
    }
    if let Some(result) = parse_embedded_unit_result(trimmed) {
        return ParsedUnitResult {
            result,
            parsed: true,
            extracted_json: true,
        };
    }
    ParsedUnitResult {
        result: clean_result(unit),
        parsed: false,
        extracted_json: trimmed.contains('{'),
    }
}

fn parse_embedded_unit_result(content: &str) -> Option<StructuredUnitResult> {
    for (start, character) in content.char_indices() {
        if character != '{' {
            continue;
        }
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (offset, character) in content[start..].char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let end = start + offset + character.len_utf8();
                        if let Ok(value) = serde_json::from_str::<Value>(&content[start..end]) {
                            if looks_like_unit_result_json(&value) {
                                let result = serde_json::from_value::<StructuredUnitResult>(value)
                                    .ok()
                                    .map(normalize_unit_result)?;
                                return Some(result);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn looks_like_unit_result_json(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("summary").is_some_and(|value| value.is_string())
        && object
            .get("fileVerdicts")
            .is_some_and(|value| value.is_array())
        && object.get("findings").is_some_and(|value| value.is_array())
}

fn normalize_unit_result(mut result: StructuredUnitResult) -> StructuredUnitResult {
    if result_summary_claims_no_findings(&result.summary)
        && result.findings.iter().all(is_non_bug_observation)
    {
        result.findings.clear();
    }
    result
}

fn result_summary_claims_no_findings(summary: &str) -> bool {
    let normalized = summary.to_ascii_lowercase();
    [
        "no actionable",
        "no concrete",
        "no definite",
        "no supported bug",
        "no correctness bug",
        "did not find",
        "no issue",
        "all assigned files are clean",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_non_bug_observation(finding: &StructuredFinding) -> bool {
    let text = format!(
        "{} {} {} {} {}",
        finding.title,
        finding.claim,
        finding.behavior_before,
        finding.behavior_after,
        finding.predicate
    );
    is_non_bug_observation_text(&text)
}

fn clean_result(unit: &PlannedReviewUnit) -> StructuredUnitResult {
    StructuredUnitResult {
        summary: format!(
            "Reviewed {} planned file(s); no structured findings returned.",
            unit.file_count
        ),
        file_verdicts: Vec::new(),
        findings: Vec::new(),
    }
}

/// A claim is cross-file when it asserts an effect on the other side of a
/// contract (callers/consumers), not merely when it mentions contract-flavored
/// vocabulary about the anchored file itself.
fn is_cross_file_claim(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "caller",
        "callers",
        "callee",
        "consumer",
        "consumers",
        "cross-file",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

/// Claims about hypothetical future code are never publishable.
fn is_hypothetical_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        " if later ",
        "if a future",
        "future change",
        "could become",
        "may become",
        "hypothetical",
        "speculative",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

/// Hedged wording is common in correct findings, so it only disqualifies a
/// claim that also lacks a concrete before/after behavior comparison.
fn is_hedged_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        " likely ",
        " may ",
        " might ",
        "potential",
        "appears to",
        " probably",
        " fragile",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_speculative_finding(text: &str, behavior_before: &str, behavior_after: &str) -> bool {
    if is_hypothetical_finding_text(text) {
        return true;
    }
    is_hedged_finding_text(text)
        && (behavior_before.trim().is_empty() || behavior_after.trim().is_empty())
}

/// A model sometimes wraps "no bug found" prose in finding shape; such text
/// must never publish as a finding.
fn is_non_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "no additional",
        "no actionable",
        "no concrete",
        "no new incompatible",
        "no supported bug",
        "not a bug",
        "does not show",
        "no further issue",
        "no issue",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_non_bug_observation_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    let has_failure_signal = [
        "wrong",
        "fails",
        "failure",
        "broken",
        "missing",
        "undefined",
        "throws",
        "crash",
        "invalid",
        "incorrect",
        "mismatch",
        "does not expose",
        "unparsed",
        "cannot",
        "never",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    let has_clean_signal = [
        "preserve",
        "preserves",
        "continues",
        "still parses",
        "still consumes",
        "still returns",
        "still persists",
        "still writes",
        "consistent",
        "no bug",
        "no issue",
        "no new incompatible",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    has_clean_signal && !has_failure_signal
}

fn validate_file_reviews(
    scope: &SessionScope,
    unit: &PlannedReviewUnit,
    unit_risk: &ContractUnitRisk,
    candidates: Vec<StructuredFileVerdict>,
    evidence_tracker: &FileEvidenceTracker,
) -> FileReviewValidation {
    let mut missing_obligations = Vec::new();
    let file_reviews = candidates
        .into_iter()
        .filter_map(|candidate| {
            let path = RepoPath::parse(&candidate.path).ok()?;
            if !unit.file_paths.iter().any(|unit_path| unit_path == &path) {
                return None;
            }
            let verdict = candidate.verdict.trim();
            if verdict.is_empty() {
                return None;
            }
            if is_clean_verdict(verdict) {
                let path_display = path.display();
                if !evidence_tracker.has_file_evidence(&path_display) {
                    missing_obligations.push(ReviewEvidenceObligation {
                        path: path_display.clone(),
                        reason: "missing_file_evidence".to_string(),
                    });
                    return None;
                }
                if unit_risk.high_risk && !evidence_tracker.has_contract_evidence(unit_risk) {
                    missing_obligations.push(ReviewEvidenceObligation {
                        path: path_display,
                        reason: "missing_contract_evidence".to_string(),
                    });
                    return None;
                }
            }
            let summary = candidate.summary.trim();
            let path_display = path.display();
            let evidence_artifact_ids = evidence_tracker.evidence_for(&path_display);
            Some(FileReviewV1 {
                path: path_display.clone(),
                verdict: verdict.to_string(),
                coverage: derive_review_coverage(
                    verdict,
                    &path_display,
                    unit_risk,
                    evidence_tracker,
                ),
                review_verdict: review_verdict_from_str(verdict),
                summary: if summary.is_empty() {
                    format!("Reviewed {path_display} with verdict {verdict}.")
                } else {
                    summary.to_string()
                },
                related_paths: candidate.related_paths,
                evidence_count: evidence_artifact_ids.len(),
                evidence_artifact_ids,
                session_id: scope.id.0.clone(),
                unit_id: unit.id.clone(),
            })
        })
        .collect();
    FileReviewValidation {
        file_reviews,
        missing_obligations,
    }
}

struct FileReviewValidation {
    file_reviews: Vec<FileReviewV1>,
    missing_obligations: Vec<ReviewEvidenceObligation>,
}

#[derive(Clone)]
struct ReviewEvidenceObligation {
    path: String,
    reason: String,
}

fn is_clean_verdict(verdict: &str) -> bool {
    matches!(
        verdict.trim().to_ascii_lowercase().as_str(),
        "clean" | "no_issue" | "no_issues" | "no supported bug found"
    )
}

fn review_verdict_from_str(verdict: &str) -> ReviewVerdict {
    match verdict.trim().to_ascii_lowercase().as_str() {
        "issue_found" | "issue" | "bug" | "finding" => ReviewVerdict::IssueFound,
        "needs_review" | "needs review" | "insufficient" | "partial" => ReviewVerdict::NeedsReview,
        _ => ReviewVerdict::Clean,
    }
}

fn derive_review_coverage(
    verdict: &str,
    path: &str,
    unit_risk: &ContractUnitRisk,
    evidence_tracker: &FileEvidenceTracker,
) -> ReviewCoverage {
    if review_verdict_from_str(verdict) == ReviewVerdict::NeedsReview {
        return ReviewCoverage::Insufficient;
    }
    let has_file = evidence_tracker.has_file_evidence(path);
    let has_contract = !unit_risk.high_risk || evidence_tracker.has_contract_evidence(unit_risk);
    if has_file && has_contract {
        if unit_risk.high_risk {
            ReviewCoverage::Full
        } else {
            ReviewCoverage::Standard
        }
    } else if has_file {
        ReviewCoverage::Sampled
    } else {
        ReviewCoverage::Insufficient
    }
}

fn missing_evidence_instruction(
    unit_risk: &ContractUnitRisk,
    obligations: &[ReviewEvidenceObligation],
) -> String {
    let missing = obligations
        .iter()
        .map(|obligation| format!("{} ({})", obligation.path, obligation.reason))
        .collect::<Vec<_>>()
        .join(", ");
    if unit_risk.high_risk {
        format!(
            "Do not return clean verdicts yet. Missing required evidence: {missing}. Evidence for unread assigned files has been loaded above when available; gather remaining contract evidence with grep, imports, or find_related_files. Suggested seed queries: {}.",
            unit_risk.suggested_queries.join(" | ")
        )
    } else {
        format!(
            "Do not return clean verdicts yet. Missing required assigned-file evidence: {missing}. The unread assigned files have been loaded above when available; re-evaluate them, then return final JSON."
        )
    }
}

fn append_needs_review_for_missing(
    scope: &SessionScope,
    unit: &PlannedReviewUnit,
    unit_risk: &ContractUnitRisk,
    evidence_tracker: &FileEvidenceTracker,
    obligations: &[ReviewEvidenceObligation],
    file_reviews: &mut Vec<FileReviewV1>,
) {
    let mut paths = obligations
        .iter()
        .map(|obligation| obligation.path.clone())
        .collect::<BTreeSet<_>>();
    if unit_risk.high_risk && !evidence_tracker.has_contract_evidence(unit_risk) {
        for path in &unit.file_paths {
            paths.insert(path.display());
        }
    }
    for path in paths {
        if file_reviews.iter().any(|review| review.path == path) {
            continue;
        }
        let evidence_artifact_ids = evidence_tracker.evidence_for(&path);
        file_reviews.push(FileReviewV1 {
            path: path.clone(),
            verdict: "needs_review".to_string(),
            coverage: ReviewCoverage::Insufficient,
            review_verdict: ReviewVerdict::NeedsReview,
            summary: if unit_risk.high_risk {
                format!(
                    "Required cross-file contract evidence was not gathered before budget exhaustion. Risk reasons: {}.",
                    unit_risk.reasons.join(", ")
                )
            } else {
                "Required assigned-file evidence was not gathered before budget exhaustion."
                    .to_string()
            },
            related_paths: Vec::new(),
            evidence_count: evidence_artifact_ids.len(),
            evidence_artifact_ids,
            session_id: scope.id.0.clone(),
            unit_id: unit.id.clone(),
        });
    }
}

fn collect_candidate_findings(
    scope: &SessionScope,
    unit: &PlannedReviewUnit,
    review_plan: &ReviewPlan,
    candidates: Vec<StructuredFinding>,
    evidence_tracker: &FileEvidenceTracker,
) -> Vec<CandidateFinding> {
    let changed_paths = changed_paths(review_plan);
    candidates
        .into_iter()
        .map(|candidate| {
            let parsed_path = RepoPath::parse(&candidate.path).ok();
            let path = parsed_path
                .as_ref()
                .map(RepoPath::display)
                .unwrap_or_else(|| candidate.path.trim().to_string());
            let title = candidate.title.trim();
            let claim = candidate.claim.trim();
            let mut rejection_reason = None;
            if parsed_path.is_none() {
                rejection_reason = Some("invalid_path".to_string());
            } else if !changed_paths.contains(&path) {
                rejection_reason = Some("unchanged_path".to_string());
            } else if title.is_empty() || claim.is_empty() {
                rejection_reason = Some("empty_title_or_claim".to_string());
            } else if candidate.start_line.zip(candidate.end_line).is_none() {
                rejection_reason = Some("missing_line_range".to_string());
            } else if evidence_tracker.evidence_for(&path).is_empty() {
                rejection_reason = Some("missing_artifact_evidence".to_string());
            } else if is_cross_file_claim(&format!("{title} {claim}"))
                && !candidate.related_paths.iter().any(|related| {
                    let related = related.trim();
                    related != path
                        && changed_paths.contains(related)
                        && evidence_tracker.has_file_evidence(related)
                })
            {
                rejection_reason = Some("missing_related_file_evidence".to_string());
            }
            CandidateFinding {
                source_unit_id: unit.id.clone(),
                source_session_id: scope.id.0.clone(),
                title: title.to_string(),
                claim: claim.to_string(),
                path: path.clone(),
                related_paths: candidate.related_paths,
                start_line: candidate.start_line,
                end_line: candidate.end_line,
                behavior_before: candidate.behavior_before.trim().to_string(),
                behavior_after: candidate.behavior_after.trim().to_string(),
                predicate: candidate.predicate.trim().to_string(),
                evidence_artifact_ids: evidence_tracker.evidence_for(&path),
                source_unit_assigned_path: parsed_path
                    .as_ref()
                    .map(|path| unit.file_paths.iter().any(|unit_path| unit_path == path))
                    .unwrap_or(false),
                rejection_reason,
            }
        })
        .collect()
}

fn synthesize_findings(
    review_plan: &ReviewPlan,
    _units: &[PlannedReviewUnit],
    contract_risk: &ContractRiskPlan,
    diff: &str,
    candidates: Vec<CandidateFinding>,
    artifacts: &ConcurrentArtifactStore,
    revision_id: &str,
) -> SynthesisOutcome {
    let candidate_count = candidates.len();
    let changed_paths = changed_paths(review_plan);
    let changed_ranges = changed_line_ranges_by_path(diff);
    let added_tokens = added_line_tokens_by_path(diff);
    let mut rejected_count = 0usize;
    let mut rescued_count = 0usize;
    let mut rejection_reasons = BTreeMap::new();
    let mut findings = Vec::<FindingV1>::new();
    for unit_risk in contract_risk.by_unit.values().filter(|risk| risk.high_risk) {
        if unit_risk.seeds.is_empty() {
            rejected_count += 1;
            *rejection_reasons
                .entry("unresolved_contract_without_seeds".to_string())
                .or_insert(0) += 1;
        }
    }
    for candidate in candidates {
        let source_unit_assigned_path = candidate.source_unit_assigned_path;
        match validate_candidate_finding(
            candidate,
            &changed_paths,
            &changed_ranges,
            &added_tokens,
            artifacts,
            revision_id,
        ) {
            Ok(finding) => {
                if !source_unit_assigned_path {
                    rescued_count += 1;
                }
                if let Some(existing) = findings
                    .iter_mut()
                    .find(|existing| should_merge_findings(existing, &finding))
                {
                    merge_finding(existing, finding);
                } else {
                    findings.push(finding);
                }
            }
            Err(reason) => {
                rejected_count += 1;
                *rejection_reasons.entry(reason).or_insert(0) += 1;
            }
        }
    }
    for finding in &mut findings {
        finding.confidence = agreement_confidence(finding.discovered_by.len());
    }
    SynthesisOutcome {
        findings,
        candidate_count,
        rescued_count,
        rejected_count,
        rejection_reasons,
    }
}

fn format_rejection_reasons(reasons: &BTreeMap<String, usize>) -> String {
    if reasons.is_empty() {
        return "none".to_string();
    }
    reasons
        .iter()
        .map(|(reason, count)| format!("{reason}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn validate_candidate_finding(
    candidate: CandidateFinding,
    changed_paths: &BTreeSet<String>,
    changed_ranges: &BTreeMap<String, Vec<(usize, usize)>>,
    added_tokens: &BTreeMap<String, Vec<(usize, Vec<String>)>>,
    artifacts: &ConcurrentArtifactStore,
    revision_id: &str,
) -> Result<FindingV1, String> {
    if let Some(reason) = candidate.rejection_reason {
        return Err(reason);
    }
    if !changed_paths.contains(&candidate.path) {
        return Err("unchanged_path".to_string());
    }
    let title = candidate.title.trim();
    let claim = candidate.claim.trim();
    if title.is_empty() || claim.is_empty() {
        return Err("empty_title_or_claim".to_string());
    }
    if is_speculative_finding(
        &format!("{title} {claim}"),
        &candidate.behavior_before,
        &candidate.behavior_after,
    ) {
        return Err("speculative_claim".to_string());
    }
    let title_and_claim = format!("{title} {claim}");
    if is_non_finding_text(&title_and_claim) || is_non_bug_observation_text(&title_and_claim) {
        return Err("non_finding_text".to_string());
    }
    let finding_text = format!(
        "{title} {claim} {} {} {}",
        candidate.behavior_before, candidate.behavior_after, candidate.predicate
    );
    if is_contract_sensitive_finding_text(&finding_text)
        && (candidate.behavior_before.is_empty() || candidate.behavior_after.is_empty())
    {
        return Err("contract_missing_behavior_comparison".to_string());
    }
    if is_query_or_filter_scope_finding_text(&finding_text) {
        let predicate_names_changed_token =
            added_tokens
                .get(&candidate.path)
                .is_some_and(|tokens_by_line| {
                    tokens_by_line.iter().any(|(_, tokens)| {
                        tokens
                            .iter()
                            .any(|token| contains_token(&candidate.predicate, token))
                    })
                });
        if candidate.predicate.is_empty() || !predicate_names_changed_token {
            return Err("query_scope_missing_predicate".to_string());
        }
    }
    let mut line_range = candidate
        .start_line
        .zip(candidate.end_line)
        .map(|(start, end)| LineRangeV1 {
            start_line: start,
            end_line: end.max(start),
        })
        .ok_or_else(|| "missing_line_range".to_string())?;
    let ranges = changed_ranges.get(&candidate.path);
    if ranges.is_none() && !changed_ranges.is_empty() {
        return Err("path_has_no_changed_lines".to_string());
    }
    if let Some(tokens_by_line) = added_tokens.get(&candidate.path) {
        if !tokens_by_line.iter().any(|(line, tokens)| {
            *line >= line_range.start_line
                && *line <= line_range.end_line
                && tokens
                    .iter()
                    .any(|token| contains_token(&finding_text, token))
        }) {
            if let Some(repaired_line) = tokens_by_line
                .iter()
                .find(|(_, tokens)| {
                    tokens
                        .iter()
                        .any(|token| contains_token(&finding_text, token))
                })
                .map(|(line, _)| *line)
            {
                line_range = LineRangeV1 {
                    start_line: repaired_line,
                    end_line: repaired_line,
                };
            }
        }
    }
    if let Some(ranges) = ranges {
        if !ranges.iter().any(|range| {
            ranges_overlap(line_range.start_line, line_range.end_line, range.0, range.1)
        }) {
            return Err("line_range_not_changed".to_string());
        }
    }
    let evidence = evidence_refs_for_candidate(&candidate, artifacts, revision_id, line_range);
    if evidence.is_empty() {
        return Err("missing_artifact_evidence".to_string());
    }
    let location = EvidenceLocationV1::SinglePath {
        path: candidate.path.clone(),
    };
    let mut file_refs = vec![location];
    for related_path in &candidate.related_paths {
        let normalized = related_path.trim();
        if normalized.is_empty()
            || normalized == candidate.path
            || !changed_paths.contains(normalized)
            || file_refs.iter().any(|location| match location {
                EvidenceLocationV1::SinglePath { path } => path == normalized,
                EvidenceLocationV1::Rename { new_path, .. } => new_path == normalized,
            })
        {
            continue;
        }
        file_refs.push(EvidenceLocationV1::SinglePath {
            path: normalized.to_string(),
        });
    }
    Ok(FindingV1 {
        id: stable_id(&[&candidate.source_session_id, title, claim, &candidate.path]),
        title: title.to_string(),
        claim: claim.to_string(),
        severity: FindingSeverity::Low,
        confidence: 0.72,
        validation_status: ValidationStatus::Validated,
        report_status: ReportStatus::Included,
        publishability: FindingPublishability::Publishable,
        challenge_status: ChallengeStatus::NotRun,
        evidence,
        file_refs,
        location_line_range: Some(line_range),
        discovered_by: vec![candidate.source_session_id],
        challenged_by: Vec::new(),
    })
}

fn is_contract_sensitive_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "contract",
        "return",
        "shape",
        "caller",
        "callee",
        "owner",
        "ownership",
        "credential",
        "query",
        "filter",
        "scope",
        "date",
        "time",
        "boundary",
        "timezone",
        "equality",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_query_or_filter_scope_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "query",
        "filter",
        "deletemany",
        "updatemany",
        "findmany",
        "cleanup",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn evidence_refs_for_candidate(
    candidate: &CandidateFinding,
    artifacts: &ConcurrentArtifactStore,
    revision_id: &str,
    line_range: LineRangeV1,
) -> Vec<EvidenceRefV1> {
    candidate
        .evidence_artifact_ids
        .iter()
        .filter_map(|artifact_id| {
            let artifact = artifacts.get(&ArtifactId(artifact_id.clone()))?;
            Some(EvidenceRefV1 {
                evidence_id: format!("evidence-{artifact_id}"),
                artifact_id: artifact_id.clone(),
                kind: ArtifactKind::ToolSummary,
                revision: EvidenceRevision::Review,
                revision_id: revision_id.to_string(),
                location: EvidenceLocationV1::SinglePath {
                    path: candidate.path.clone(),
                },
                line_range: Some(line_range),
                byte_range: Some(ByteRangeV1 {
                    start_byte: 0,
                    end_byte: artifact.bytes,
                }),
                diff_anchor: None,
                content_hash: artifact.content_hash,
                redaction: RedactionMetadataV1 {
                    redaction_state: RedactionState::None,
                    redaction_policy_id: format!("muzen-redaction-v{}", REDACTION_POLICY_VERSION),
                    contains_repo_content: true,
                    contains_prompt_content: false,
                    contains_model_output: false,
                    contains_secret_material: false,
                },
                producing_tool_call_id: format!("{}-candidate-evidence", candidate.source_unit_id),
            })
        })
        .collect()
}

fn changed_paths(review_plan: &ReviewPlan) -> BTreeSet<String> {
    review_plan
        .files
        .iter()
        .map(|file| file.path.display())
        .collect()
}

fn should_merge_findings(existing: &FindingV1, duplicate: &FindingV1) -> bool {
    if finding_key(existing) == finding_key(duplicate) {
        return true;
    }
    if finding_path(existing) != finding_path(duplicate) {
        return false;
    }
    let Some(left_range) = existing.location_line_range else {
        return false;
    };
    let Some(right_range) = duplicate.location_line_range else {
        return false;
    };
    if !ranges_overlap(
        left_range.start_line,
        left_range.end_line,
        right_range.start_line,
        right_range.end_line,
    ) {
        return false;
    }
    if findings_share_evidence(existing, duplicate)
        && overlapping_findings_share_enough_terms(existing, duplicate)
    {
        return true;
    }
    let left_text = format!("{} {}", existing.title, existing.claim);
    let right_text = format!("{} {}", duplicate.title, duplicate.claim);
    let left_tokens = normalized_token_set(&left_text);
    let right_tokens = normalized_token_set(&right_text);
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return false;
    }
    let shared = left_tokens.intersection(&right_tokens).count();
    let smaller = left_tokens.len().min(right_tokens.len());
    shared * 100 >= smaller * 55
        || left_tokens.is_subset(&right_tokens)
        || right_tokens.is_subset(&left_tokens)
}

fn findings_share_evidence(left: &FindingV1, right: &FindingV1) -> bool {
    left.evidence.iter().any(|left_evidence| {
        right
            .evidence
            .iter()
            .any(|right_evidence| left_evidence.artifact_id == right_evidence.artifact_id)
    })
}

fn overlapping_findings_share_enough_terms(left: &FindingV1, right: &FindingV1) -> bool {
    let left_text = format!("{} {}", left.title, left.claim);
    let right_text = format!("{} {}", right.title, right.claim);
    let left_tokens = normalized_token_set(&left_text);
    let right_tokens = normalized_token_set(&right_text);
    let shared = left_tokens.intersection(&right_tokens).count();
    shared >= 3
}

fn finding_path(finding: &FindingV1) -> String {
    finding
        .file_refs
        .first()
        .map(|location| match location {
            EvidenceLocationV1::SinglePath { path } => path.clone(),
            EvidenceLocationV1::Rename { new_path, .. } => new_path.clone(),
        })
        .unwrap_or_default()
}

fn finding_key(finding: &FindingV1) -> String {
    let path = finding_path(finding);
    format!(
        "{}:{}",
        path,
        normalize_finding_text(&format!("{} {}", finding.title, finding.claim))
    )
}

fn normalized_token_set(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| part.len() >= 4)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn normalize_finding_text(value: &str) -> String {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Baseline confidence for a single-discoverer validated finding; matches the
/// fixed score findings carried before agreement-derived confidence existed.
const BASE_CONFIDENCE: f32 = 0.72;
/// Confidence earned per additional independent discovering session.
const AGREEMENT_CONFIDENCE_STEP: f32 = 0.07;
/// Agreement alone cannot push confidence past this; only the adversarial
/// challenge pass confirms beyond it.
const AGREEMENT_CONFIDENCE_CEILING: f32 = 0.93;
/// Confidence assigned to findings the challenge pass refuted.
const REFUTED_CONFIDENCE: f32 = 0.25;
/// Confidence added when the challenge pass confirms a finding.
const CONFIRMED_CONFIDENCE_BOOST: f32 = 0.07;
const MAX_CONFIDENCE: f32 = 0.95;

fn agreement_confidence(discoverers: usize) -> f32 {
    let extra = discoverers.saturating_sub(1) as f32;
    (BASE_CONFIDENCE + extra * AGREEMENT_CONFIDENCE_STEP).min(AGREEMENT_CONFIDENCE_CEILING)
}

fn merge_finding(existing: &mut FindingV1, duplicate: FindingV1) {
    for evidence in duplicate.evidence {
        if !existing
            .evidence
            .iter()
            .any(|item| item.artifact_id == evidence.artifact_id)
        {
            existing.evidence.push(evidence);
        }
    }
    for discovered_by in duplicate.discovered_by {
        if !existing.discovered_by.contains(&discovered_by) {
            existing.discovered_by.push(discovered_by);
        }
    }
}

fn reconcile_file_reviews_with_findings(file_reviews: &mut [FileReviewV1], findings: &[FindingV1]) {
    for finding in findings {
        if matches!(
            finding.publishability,
            FindingPublishability::NotPublishable
        ) {
            continue;
        }
        let Some(EvidenceLocationV1::SinglePath { path }) = finding.file_refs.first() else {
            continue;
        };
        if let Some(review) = file_reviews.iter_mut().find(|review| review.path == *path) {
            review.verdict = "issue_found".to_string();
            review.review_verdict = ReviewVerdict::IssueFound;
            if review.coverage == ReviewCoverage::Insufficient {
                review.coverage = ReviewCoverage::Standard;
            }
            if !review.summary.contains(&finding.title) {
                review.summary = format!("{} Finding: {}", review.summary, finding.title);
            }
            for evidence in &finding.evidence {
                if !review.evidence_artifact_ids.contains(&evidence.artifact_id) {
                    review
                        .evidence_artifact_ids
                        .push(evidence.artifact_id.clone());
                }
            }
            review.evidence_count = review.evidence_artifact_ids.len();
        }
    }
}

fn changed_line_ranges_by_path(diff: &str) -> BTreeMap<String, Vec<(usize, usize)>> {
    let mut current_path: Option<String> = None;
    let mut current_new_line: Option<usize> = None;
    let mut ranges: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            current_new_line = None;
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_path = None;
            current_new_line = None;
            continue;
        }
        let Some(path) = current_path.as_ref() else {
            continue;
        };
        if let Some(hunk) = line.strip_prefix("@@ ") {
            let Some(new_range) = hunk.split_whitespace().nth(1) else {
                current_new_line = None;
                continue;
            };
            let Some(new_range) = new_range.strip_prefix('+') else {
                current_new_line = None;
                continue;
            };
            let (start, _) = new_range
                .split_once(',')
                .map_or((new_range, "1"), |(start, count)| (start, count));
            current_new_line = start.parse::<usize>().ok();
            continue;
        }
        let Some(new_line) = current_new_line else {
            continue;
        };
        if line.starts_with('+') && !line.starts_with("+++") {
            push_line_range(ranges.entry(path.clone()).or_default(), new_line);
            current_new_line = Some(new_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            continue;
        } else {
            current_new_line = Some(new_line + 1);
        }
    }
    ranges
}

fn ranges_overlap(
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> bool {
    left_start <= right_end && right_start <= left_end
}

fn push_line_range(ranges: &mut Vec<(usize, usize)>, line: usize) {
    if let Some((_, end)) = ranges.last_mut() {
        if line == *end + 1 {
            *end = line;
            return;
        }
    }
    ranges.push((line, line));
}

fn added_line_tokens_by_path(diff: &str) -> BTreeMap<String, Vec<(usize, Vec<String>)>> {
    let mut current_path: Option<String> = None;
    let mut current_new_line: Option<usize> = None;
    let mut tokens_by_path: BTreeMap<String, Vec<(usize, Vec<String>)>> = BTreeMap::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            current_new_line = None;
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_path = None;
            current_new_line = None;
            continue;
        }
        let Some(path) = current_path.as_ref() else {
            continue;
        };
        if let Some(hunk) = line.strip_prefix("@@ ") {
            let Some(new_range) = hunk.split_whitespace().nth(1) else {
                current_new_line = None;
                continue;
            };
            let Some(new_range) = new_range.strip_prefix('+') else {
                current_new_line = None;
                continue;
            };
            let (start, _) = new_range
                .split_once(',')
                .map_or((new_range, "1"), |(start, count)| (start, count));
            current_new_line = start.parse::<usize>().ok();
            continue;
        }
        let Some(new_line) = current_new_line else {
            continue;
        };
        if line.starts_with('+') && !line.starts_with("+++") {
            let tokens = significant_tokens(line.trim_start_matches('+'));
            if !tokens.is_empty() {
                tokens_by_path
                    .entry(path.clone())
                    .or_default()
                    .push((new_line, tokens));
            }
            current_new_line = Some(new_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            continue;
        } else {
            current_new_line = Some(new_line + 1);
        }
    }
    tokens_by_path
}

fn contains_token(text: &str, token: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|candidate| candidate == token)
}

fn significant_tokens(line: &str) -> Vec<String> {
    line.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() >= 4 && !GENERIC_CODE_TOKENS.contains(&token.as_str()))
        .collect()
}

const GENERIC_CODE_TOKENS: &[&str] = &[
    "const", "let", "var", "true", "false", "return", "await", "async", "where", "with", "from",
    "this", "that", "item", "items", "value", "values",
];

pub(crate) fn record_usage(
    tokens: &mut TokenUsage,
    model_metrics: &mut ModelMetricsSnapshot,
    model: &dyn crate::runtime::model::ConcurrentModelClient,
    usage: TokenUsage,
) {
    tokens.add(usage);
    model_metrics.input_tokens += usage.input_tokens;
    model_metrics.output_tokens += usage.output_tokens;
    model_metrics.total_tokens += usage.total_tokens;
    model_metrics.cached_input_tokens += usage.cached_input_tokens;
    if let Some(cost) = model.estimate_cost(&usage) {
        model_metrics.costed_calls += 1;
        model_metrics.estimated_input_cost_micro_usd += cost.input_cost_micro_usd;
        model_metrics.estimated_output_cost_micro_usd += cost.output_cost_micro_usd;
        model_metrics.estimated_total_cost_micro_usd += cost.total_cost_micro_usd;
    } else {
        model_metrics.unpriced_calls += 1;
    }
}

pub(crate) fn add_model_metrics(target: &mut ModelMetricsSnapshot, report: &ModelMetricsSnapshot) {
    target.calls += report.calls;
    target.successes += report.successes;
    target.errors += report.errors;
    target.retries += report.retries;
    target.costed_calls += report.costed_calls;
    target.unpriced_calls += report.unpriced_calls;
    target.latency_ms += report.latency_ms;
    target.max_latency_ms = target.max_latency_ms.max(report.max_latency_ms);
    target.estimated_input_cost_micro_usd += report.estimated_input_cost_micro_usd;
    target.estimated_output_cost_micro_usd += report.estimated_output_cost_micro_usd;
    target.estimated_total_cost_micro_usd += report.estimated_total_cost_micro_usd;
    target.input_tokens += report.input_tokens;
    target.output_tokens += report.output_tokens;
    target.total_tokens += report.total_tokens;
    target.cached_input_tokens += report.cached_input_tokens;
}

fn transcript_bytes(transcript: &[ConversationItem]) -> usize {
    transcript
        .iter()
        .map(|item| match item {
            ConversationItem::System { content }
            | ConversationItem::User { content }
            | ConversationItem::AssistantText { content } => content.len(),
            ConversationItem::AssistantToolCalls { calls } => calls
                .iter()
                .map(|call| {
                    call.call_id.0.len() + call.name.as_str().len() + call.raw_arguments.len()
                })
                .sum(),
            ConversationItem::ToolResult { content, .. } => content.limits.output_bytes,
        })
        .sum()
}

fn first_system_digest(transcript: &[ConversationItem]) -> Option<String> {
    transcript.iter().find_map(|item| match item {
        ConversationItem::System { content } => Some(stable_id(&[content])),
        _ => None,
    })
}

fn last_user_digest(transcript: &[ConversationItem]) -> Option<String> {
    transcript.iter().rev().find_map(|item| match item {
        ConversationItem::User { content } => Some(stable_id(&[content])),
        _ => None,
    })
}

fn schema_tool_trace(
    schema: &serde_json::Value,
    alias_table: Option<&crate::runtime::tools::ToolAliasTable>,
) -> Option<serde_json::Value> {
    let function = schema.get("function")?;
    let name = function.get("name")?.as_str()?;
    let description = function
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let parameters = function.get("parameters").cloned().unwrap_or_default();
    let parameters_text = serde_json::to_string(&parameters).unwrap_or_default();
    let internal_tool_id = ToolId::parse(name)
        .ok()
        .and_then(|alias| alias_table.and_then(|table| table.tool_for_alias(&alias).cloned()));
    Some(json!({
        "modelName": name,
        "internalToolId": internal_tool_id.map(|tool_id| tool_id.as_str().to_string()),
        "descriptionDigest": stable_id(&[description]),
        "parametersDigest": stable_id(&[&parameters_text]),
    }))
}

fn response_format_schema_digest(response_format: Option<&ModelResponseFormat>) -> Option<String> {
    response_format.map(|format| stable_id(&[&format.schema.to_string()]))
}

fn unit_diagnostic(
    unit: &PlannedReviewUnit,
    completed: bool,
    status: &str,
) -> SessionCompletionDiagnostic {
    SessionCompletionDiagnostic {
        session_id: unit.id.clone(),
        completed,
        completion_kind: Some("structured_unit_result".to_string()),
        completion_summary: Some(status.to_string()),
        saw_diff: completed,
        saw_file: completed,
        saw_search: false,
        model_calls: 0,
        tool_counts: ToolCounts::default(),
    }
}

fn diff_excerpt(diff: &str, max_chars: usize) -> String {
    truncate_chars(diff, max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, character) in value.chars().enumerate() {
        if index >= max_chars {
            output.push_str("\n...[truncated]");
            break;
        }
        output.push(character);
    }
    output
}

fn planned_benchmark_failures(report: &ConcurrentRunReport) -> Vec<String> {
    let mut failures = Vec::new();
    if report.sessions > 0 && report.completed_sessions == 0 {
        failures.push("no planned review units completed".to_string());
    }
    if report.sessions > 0 && report.model_calls == 0 {
        failures.push("no model calls recorded".to_string());
    }
    failures
}

#[allow(clippy::too_many_arguments)]
fn planned_review_audit_diagnostics(
    review_plan: &ReviewPlan,
    contract_risk: &ContractRiskPlan,
    contract_packs: &DiffPackContext,
    file_reviews: &[FileReviewV1],
    findings: &[FindingV1],
    session_templates: &[SessionScope],
    sessions_run: usize,
    candidate_findings: usize,
    rescued_candidates: usize,
    rejected_candidates: usize,
    rejection_reasons: BTreeMap<String, usize>,
) -> ReviewQualityDiagnostics {
    let mut coverage_counts = BTreeMap::new();
    let mut coverage_counts_by_lens = BTreeMap::<String, BTreeMap<String, usize>>::new();
    let mut high_risk_files_below_target = Vec::new();
    for review in file_reviews {
        let coverage = coverage_key(review.coverage);
        *coverage_counts.entry(coverage.clone()).or_insert(0) += 1;
        let lens = review
            .session_id
            .split('#')
            .nth(1)
            .unwrap_or("primary")
            .to_string();
        *coverage_counts_by_lens
            .entry(lens)
            .or_default()
            .entry(coverage)
            .or_insert(0) += 1;
        if matches!(
            review.coverage,
            ReviewCoverage::Sampled | ReviewCoverage::Insufficient
        ) && review_plan
            .files
            .iter()
            .any(|file| file.path.display() == review.path && file.score >= LENS_FANOUT_MIN_SCORE)
        {
            high_risk_files_below_target.push(review.path.clone());
        }
    }
    let mut challenge_status_counts = BTreeMap::new();
    for finding in findings {
        *challenge_status_counts
            .entry(challenge_key(finding.challenge_status).to_string())
            .or_insert(0) += 1;
    }
    let mut budgets_used = BTreeMap::new();
    for template in session_templates {
        *budgets_used
            .entry(budget_source_key(template.budget.budget_source).to_string())
            .or_insert(0) += 1;
    }
    ReviewQualityDiagnostics {
        contract_risk_units: contract_risk.risky_unit_count(),
        contract_seed_count: contract_risk.seed_count(),
        contract_pack_count: contract_packs.pack_count(),
        omitted_contract_pack_candidates: Vec::new(),
        selected_contract_packs: Vec::new(),
        contract_evidence_failures: file_reviews
            .iter()
            .filter(|review| {
                review.verdict == "needs_review"
                    && review
                        .summary
                        .contains("Required cross-file contract evidence")
            })
            .count(),
        coverage_counts,
        coverage_counts_by_lens,
        high_risk_files_below_target,
        challenge_status_counts,
        sessions_run,
        budgets_used,
        explicit_caller_cap_sessions: session_templates
            .iter()
            .filter(|template| {
                template.budget.budget_source == crate::contracts::BudgetSource::CallerHardCap
            })
            .count(),
        candidate_findings,
        rescued_candidates,
        rejected_candidates,
        rejection_reasons,
    }
}

fn coverage_key(coverage: ReviewCoverage) -> String {
    match coverage {
        ReviewCoverage::Full => "full",
        ReviewCoverage::Standard => "standard",
        ReviewCoverage::Sampled => "sampled",
        ReviewCoverage::Insufficient => "insufficient",
    }
    .to_string()
}

fn challenge_key(status: ChallengeStatus) -> &'static str {
    match status {
        ChallengeStatus::Confirmed => "confirmed",
        ChallengeStatus::Refuted => "refuted",
        ChallengeStatus::Insufficient => "insufficient",
        ChallengeStatus::NotRun => "not_run",
        ChallengeStatus::Incomplete => "incomplete",
    }
}

fn budget_source_key(source: crate::contracts::BudgetSource) -> &'static str {
    match source {
        crate::contracts::BudgetSource::CallerHardCap => "caller_hard_cap",
        crate::contracts::BudgetSource::PlannedDefault => "planned_default",
        crate::contracts::BudgetSource::AdaptiveReview => "adaptive_review",
        crate::contracts::BudgetSource::RunReserve => "run_reserve",
    }
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    (started.elapsed().as_micros().div_ceil(1000) as u64).max(1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::contracts::{
        AgentBudget, ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus,
        PathPolicyV1, RenameDetection, SnapshotMode,
    };
    use crate::runtime::model::StaticModelRouter;
    use crate::runtime::repo::RepoSnapshot;

    struct EvidenceBackedModel {
        mode: TestModelMode,
    }

    enum TestModelMode {
        AssignedFinding,
        OutOfUnitFinding,
        InvalidCandidates,
        LowRiskClean,
        HighRiskCleanWithoutContractEvidence,
        HighRiskCleanWithContractEvidence,
        BootstrapClean,
        NeedsFiveTurns,
        AssertFinalTurnHasNoTools,
        FinalSynthesisFinding,
        ChallengeRefutesFinding,
        PackConsumerFinding,
        CleanBeforeEvidence,
    }

    #[async_trait]
    impl crate::runtime::model::ConcurrentModelClient for EvidenceBackedModel {
        async fn complete(
            &self,
            scope: &SessionScope,
            _transcript: &[ConversationItem],
            turn_id: TurnId,
            _cancel: CancellationToken,
        ) -> RuntimeResult<ModelTurn> {
            if scope.id.0.starts_with("pack-") {
                let content = match self.mode {
                    TestModelMode::PackConsumerFinding => json!({
                        "summary": "the refresh helper now returns a raw response and the consumer reads token fields",
                        "questionAnswers": [
                            {"question": "What concrete value shape does `refreshOAuthTokens` return after this change?", "answer": "It returns the raw response object instead of parsed token fields."},
                            {"question": "What value shape do changed callers expect at the use site?", "answer": "The consumer reads credential.access_token from the result."},
                            {"question": "Does the old/new behavior comparison show a caller-visible contract break?", "answer": "Yes, access_token is undefined on the raw response."}
                        ],
                        "relatedPathVerdicts": [
                            {"path": "packages/app-store/googlecalendar/lib/CalendarService.ts", "affected": true, "reason": "It reads credential.access_token which the raw response does not expose."}
                        ],
                        "findings": [{
                            "title": "Calendar consumer reads token fields from the raw refresh response",
                            "claim": "The changed consumer call now receives a raw response from refreshOAuthTokens, so credential.access_token is undefined.",
                            "path": "packages/app-store/googlecalendar/lib/CalendarService.ts",
                            "relatedPaths": ["packages/app-store/_utils/oauth/refreshOAuthTokens.ts"],
                            "startLine": 1,
                            "endLine": 2,
                            "behaviorBefore": "refreshOAuthTokens returned parsed token fields including access_token",
                            "behaviorAfter": "refreshOAuthTokens returns the raw response without access_token"
                        }]
                    }),
                    _ => json!({
                        "summary": "pack investigation clean",
                        "questionAnswers": [],
                        "relatedPathVerdicts": [],
                        "findings": []
                    }),
                };
                return Ok(ModelTurn::Text {
                    content: content.to_string(),
                    usage: TokenUsage::default(),
                });
            }
            if scope.id.0 == "final-synthesis" {
                let content = match self.mode {
                    TestModelMode::FinalSynthesisFinding => json!({
                        "summary": "synthesis found one issue",
                        "findings": [{
                            "title": "Widget synthesis exposes unsafe state",
                            "claim": "The changed render_widget branch returns unsafe_widget_state.",
                            "path": "src/widget.rs",
                            "startLine": 1,
                            "endLine": 1,
                            "behaviorBefore": "render_widget returned a safe value",
                            "behaviorAfter": "render_widget returns unsafe_widget_state"
                        }]
                    }),
                    _ => json!({
                        "summary": "synthesis found no additional issues",
                        "findings": []
                    }),
                };
                return Ok(ModelTurn::Text {
                    content: content.to_string(),
                    usage: TokenUsage::default(),
                });
            }
            if scope.id.0 == "finding-challenge" {
                let verdict = match self.mode {
                    TestModelMode::ChallengeRefutesFinding => "refuted",
                    _ => "confirmed",
                };
                return Ok(ModelTurn::Text {
                    content: json!({
                        "verdicts": [{
                            "index": 0,
                            "verdict": verdict,
                            "reason": "deterministic test verdict"
                        }]
                    })
                    .to_string(),
                    usage: TokenUsage::default(),
                });
            }
            if scope.id.0.contains("::explore-") {
                if turn_id.0 == 0 {
                    return Ok(ModelTurn::ToolCalls {
                        calls: vec![
                            ModelToolCall {
                                call_id: ToolCallId(format!("{}-worker-diff", scope.id.0)),
                                index: 0,
                                name: ToolId::from(ToolName::ReadDiff),
                                raw_arguments: "{}".to_string(),
                            },
                            ModelToolCall {
                                call_id: ToolCallId(format!("{}-worker-search", scope.id.0)),
                                index: 1,
                                name: ToolId::from(ToolName::SearchText),
                                raw_arguments: r#"{"query":"oauth callback token"}"#.to_string(),
                            },
                            ModelToolCall {
                                call_id: ToolCallId(format!("{}-worker-read", scope.id.0)),
                                index: 2,
                                name: ToolId::from(ToolName::ReadHeadFile),
                                raw_arguments: r#"{"path":"apps/api/token-callback.ts"}"#
                                    .to_string(),
                            },
                        ],
                        usage: TokenUsage::default(),
                    });
                }
                return Ok(ModelTurn::Text {
                    content: json!({
                        "summary": "worker checked oauth callback token ownership evidence",
                        "checkedPaths": ["apps/api/token-callback.ts"],
                        "evidenceArtifactIds": [],
                        "unresolvedQuestions": []
                    })
                    .to_string(),
                    usage: TokenUsage::default(),
                });
            }
            match self.mode {
                TestModelMode::PackConsumerFinding if turn_id.0 == 0 => {
                    return Ok(ModelTurn::ToolCalls {
                        calls: vec![
                            ModelToolCall {
                                call_id: ToolCallId(format!("{}-read-helper", scope.id.0)),
                                index: 0,
                                name: ToolId::from(ToolName::ReadHeadFile),
                                raw_arguments: r#"{"path":"packages/app-store/_utils/oauth/refreshOAuthTokens.ts"}"#.to_string(),
                            },
                            ModelToolCall {
                                call_id: ToolCallId(format!("{}-read-consumer", scope.id.0)),
                                index: 1,
                                name: ToolId::from(ToolName::ReadHeadFile),
                                raw_arguments: r#"{"path":"packages/app-store/googlecalendar/lib/CalendarService.ts"}"#.to_string(),
                            },
                            ModelToolCall {
                                call_id: ToolCallId(format!("{}-search-refresh", scope.id.0)),
                                index: 2,
                                name: ToolId::from(ToolName::SearchText),
                                raw_arguments: r#"{"query":"refreshOAuthTokens access_token"}"#.to_string(),
                            },
                        ],
                        usage: TokenUsage::default(),
                    });
                }
                TestModelMode::PackConsumerFinding => {
                    return Ok(ModelTurn::Text {
                        content: json!({
                            "summary": "the unit explorer compared the refresh helper producer and calendar consumer",
                            "fileVerdicts": [
                                {
                                    "path": "packages/app-store/_utils/oauth/refreshOAuthTokens.ts",
                                    "verdict": "issue_found",
                                    "summary": "the helper returns a raw response shape that changed consumers do not handle"
                                },
                                {
                                    "path": "packages/app-store/googlecalendar/lib/CalendarService.ts",
                                    "verdict": "issue_found",
                                    "summary": "the consumer reads token fields from the helper result"
                                }
                            ],
                            "findings": [{
                                "title": "Calendar consumer reads token fields from the raw refresh response",
                                "claim": "The changed consumer call now receives a raw response from refreshOAuthTokens, so credential.access_token is undefined.",
                                "path": "packages/app-store/googlecalendar/lib/CalendarService.ts",
                                "relatedPaths": ["packages/app-store/_utils/oauth/refreshOAuthTokens.ts"],
                                "startLine": 2,
                                "endLine": 2,
                                "behaviorBefore": "refreshOAuthTokens returned parsed token fields including access_token",
                                "behaviorAfter": "refreshOAuthTokens returns the raw response without access_token"
                            }]
                        })
                        .to_string(),
                        usage: TokenUsage::default(),
                    });
                }
                TestModelMode::CleanBeforeEvidence => {
                    return Ok(ModelTurn::Text {
                        content: json!({
                            "summary": "clean without having read the file",
                            "fileVerdicts": [{"path": "src/widget.rs", "verdict": "clean"}],
                            "findings": []
                        })
                        .to_string(),
                        usage: TokenUsage::default(),
                    });
                }
                TestModelMode::BootstrapClean => {
                    return Ok(ModelTurn::Text {
                        content: json!({
                            "summary": "clean from bootstrap",
                            "fileVerdicts": [{"path": "src/widget.rs", "verdict": "clean"}],
                            "findings": []
                        })
                        .to_string(),
                        usage: TokenUsage::default(),
                    });
                }
                TestModelMode::NeedsFiveTurns if turn_id.0 < 4 => {
                    let mut calls = Vec::new();
                    if turn_id.0 == 0 {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-turn-{}-read", scope.id.0, turn_id.0)),
                            index: 0,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"src/widget.rs"}"#.to_string(),
                        });
                    }
                    calls.push(ModelToolCall {
                        call_id: ToolCallId(format!("{}-turn-{}-search", scope.id.0, turn_id.0)),
                        index: calls.len(),
                        name: ToolId::from(ToolName::SearchText),
                        raw_arguments: r#"{"query":"render_widget"}"#.to_string(),
                    });
                    return Ok(ModelTurn::ToolCalls {
                        calls,
                        usage: TokenUsage::default(),
                    });
                }
                TestModelMode::NeedsFiveTurns => {
                    return Ok(ModelTurn::Text {
                        content: json!({
                            "summary": "finished after extra turns",
                            "fileVerdicts": [{"path": "src/widget.rs", "verdict": "clean"}],
                            "findings": []
                        })
                        .to_string(),
                        usage: TokenUsage::default(),
                    });
                }
                TestModelMode::AssertFinalTurnHasNoTools => {
                    if turn_id.0 == 0 {
                        assert!(!scope
                            .capabilities
                            .allow_tool(&ToolId::from(ToolName::SearchText)));
                        assert_eq!(
                            scope
                                .response_format
                                .as_ref()
                                .map(|format| format.name.as_str()),
                            Some("muzen_review_unit_result_v1")
                        );
                    }
                    return Ok(ModelTurn::Text {
                        content: json!({
                            "summary": "final scoped",
                            "fileVerdicts": [{"path": "src/widget.rs", "verdict": "clean"}],
                            "findings": []
                        })
                        .to_string(),
                        usage: TokenUsage::default(),
                    });
                }
                _ => {}
            }
            if turn_id.0 == 0 {
                let mut calls = vec![ModelToolCall {
                    call_id: ToolCallId(format!("{}-read-diff", scope.id.0)),
                    index: 0,
                    name: ToolId::from(ToolName::ReadDiff),
                    raw_arguments: "{}".to_string(),
                }];
                match self.mode {
                    TestModelMode::AssignedFinding | TestModelMode::ChallengeRefutesFinding => {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-auth", scope.id.0)),
                            index: 1,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"src/auth.rs"}"#.to_string(),
                        });
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-search-auth", scope.id.0)),
                            index: 2,
                            name: ToolId::from(ToolName::SearchText),
                            raw_arguments: r#"{"query":"allow_empty_token"}"#.to_string(),
                        });
                    }
                    TestModelMode::InvalidCandidates => {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-auth", scope.id.0)),
                            index: 1,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"src/auth.rs"}"#.to_string(),
                        });
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-search-auth", scope.id.0)),
                            index: 2,
                            name: ToolId::from(ToolName::SearchText),
                            raw_arguments: r#"{"query":"missing_symbol"}"#.to_string(),
                        });
                    }
                    TestModelMode::LowRiskClean => {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-widget", scope.id.0)),
                            index: 1,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"src/widget.rs"}"#.to_string(),
                        });
                    }
                    TestModelMode::HighRiskCleanWithoutContractEvidence => {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-callback", scope.id.0)),
                            index: 1,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"apps/api/token-callback.ts"}"#.to_string(),
                        });
                    }
                    TestModelMode::HighRiskCleanWithContractEvidence => {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-callback", scope.id.0)),
                            index: 1,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"apps/api/token-callback.ts"}"#.to_string(),
                        });
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-search-callback", scope.id.0)),
                            index: 2,
                            name: ToolId::from(ToolName::SearchText),
                            raw_arguments: r#"{"query":"credential_token"}"#.to_string(),
                        });
                    }
                    TestModelMode::OutOfUnitFinding if scope.id.0 == "unit-001" => {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-auth", scope.id.0)),
                            index: 1,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"apps/api/auth-token.ts"}"#.to_string(),
                        });
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-related", scope.id.0)),
                            index: 2,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"apps/api/credential-token.ts"}"#.to_string(),
                        });
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-search-token", scope.id.0)),
                            index: 3,
                            name: ToolId::from(ToolName::SearchText),
                            raw_arguments: r#"{"query":"refresh_token"}"#.to_string(),
                        });
                    }
                    TestModelMode::OutOfUnitFinding => {
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-read-credential", scope.id.0)),
                            index: 1,
                            name: ToolId::from(ToolName::ReadHeadFile),
                            raw_arguments: r#"{"path":"apps/api/credential-token.ts"}"#.to_string(),
                        });
                        calls.push(ModelToolCall {
                            call_id: ToolCallId(format!("{}-search-token", scope.id.0)),
                            index: 2,
                            name: ToolId::from(ToolName::SearchText),
                            raw_arguments: r#"{"query":"refresh_token"}"#.to_string(),
                        });
                    }
                    TestModelMode::BootstrapClean
                    | TestModelMode::NeedsFiveTurns
                    | TestModelMode::AssertFinalTurnHasNoTools
                    | TestModelMode::FinalSynthesisFinding
                    | TestModelMode::PackConsumerFinding
                    | TestModelMode::CleanBeforeEvidence => {}
                }
                return Ok(ModelTurn::ToolCalls {
                    calls,
                    usage: TokenUsage::default(),
                });
            }
            let content = match self.mode {
                TestModelMode::AssignedFinding | TestModelMode::ChallengeRefutesFinding => json!({
                    "summary": "found one issue",
                    "fileVerdicts": [{
                        "path": "src/auth.rs",
                        "verdict": "clean",
                        "summary": "reviewed auth",
                        "relatedPaths": []
                    }],
                    "findings": [{
                        "title": "Token validation accepts empty token",
                        "claim": "The changed allow_empty_token branch now accepts an empty token.",
                        "path": "src/auth.rs",
                        "startLine": 1,
                        "endLine": 1
                    }]
                }),
                TestModelMode::OutOfUnitFinding if scope.id.0 == "unit-001" => json!({
                    "summary": "found cross-unit issue",
                    "fileVerdicts": [
                        {"path": "apps/api/auth-token.ts", "verdict": "clean"},
                        {"path": "apps/api/credential-token.ts", "verdict": "issue_found"}
                    ],
                    "findings": [{
                        "title": "External callback drops refresh token",
                        "claim": "The changed refresh_token assignment can drop the refresh_token returned by the external callback.",
                        "path": "apps/api/credential-token.ts",
                        "startLine": 1,
                        "endLine": 1,
                        "behaviorBefore": "the stored refresh_token kept the provider value",
                        "behaviorAfter": "the changed refresh_token assignment stores an empty value"
                    }]
                }),
                TestModelMode::OutOfUnitFinding => json!({
                    "summary": "clean local unit",
                    "fileVerdicts": [{"path": "apps/api/credential-token.ts", "verdict": "clean"}],
                    "findings": []
                }),
                TestModelMode::InvalidCandidates => json!({
                    "summary": "invalid candidates are diagnostic only",
                    "fileVerdicts": [
                        {"path": "src/auth.rs", "verdict": "clean"},
                        {"path": "src/other.rs", "verdict": "issue_found"}
                    ],
                    "findings": [
                        {
                            "title": "Unchanged path issue",
                            "claim": "The changed missing_symbol branch is unsafe.",
                            "path": "src/other.rs",
                            "startLine": 1,
                            "endLine": 1
                        },
                        {
                            "title": "Invalid range issue",
                            "claim": "The unrelated branch is unsafe.",
                            "path": "src/auth.rs",
                            "startLine": 99,
                            "endLine": 99
                        }
                    ]
                }),
                TestModelMode::LowRiskClean => json!({
                    "summary": "low risk clean",
                    "fileVerdicts": [{"path": "src/widget.rs", "verdict": "clean"}],
                    "findings": []
                }),
                TestModelMode::HighRiskCleanWithoutContractEvidence => json!({
                    "summary": "high risk clean without contract evidence",
                    "fileVerdicts": [{"path": "apps/api/token-callback.ts", "verdict": "clean"}],
                    "findings": []
                }),
                TestModelMode::HighRiskCleanWithContractEvidence => json!({
                    "summary": "high risk clean with contract evidence",
                    "fileVerdicts": [{"path": "apps/api/token-callback.ts", "verdict": "clean"}],
                    "findings": []
                }),
                TestModelMode::BootstrapClean
                | TestModelMode::NeedsFiveTurns
                | TestModelMode::AssertFinalTurnHasNoTools
                | TestModelMode::FinalSynthesisFinding
                | TestModelMode::PackConsumerFinding
                | TestModelMode::CleanBeforeEvidence => json!({
                    "summary": "fallback clean",
                    "fileVerdicts": [{"path": "src/widget.rs", "verdict": "clean"}],
                    "findings": []
                }),
            };
            Ok(ModelTurn::Text {
                content: content.to_string(),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 12,
                    total_tokens: 22,
                    cached_input_tokens: 0,
                },
            })
        }
    }

    #[tokio::test]
    async fn planned_runtime_publishes_artifact_backed_assigned_candidate() {
        let report = run_test_review(
            vec![("src/auth.rs", "pub const allow_empty_token: bool = true;\n")],
            TestModelMode::AssignedFinding,
        )
        .await;

        assert_eq!(report.metrics.runtime, "planned_units");
        assert_eq!(report.metrics.sessions, 1);
        assert_eq!(report.metrics.completed_sessions, 1);
        assert_eq!(report.metrics.model_calls, 3);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(
            report.findings[0].title,
            "Token validation accepts empty token"
        );
        assert!(!report.findings[0].evidence.is_empty());
        let auth_review = report
            .file_reviews
            .iter()
            .find(|review| review.path == "src/auth.rs")
            .expect("auth review");
        assert_eq!(auth_review.verdict, "issue_found");
    }

    #[tokio::test]
    async fn planned_runtime_emits_agent_trace_for_exploration_audit() {
        let events = Arc::new(crate::reviewer::runtime_events::InMemoryEventSink::default());
        let runtime_events: Arc<dyn RuntimeEventSink> = events.clone();
        let report = run_test_review_with_templates_and_events(
            vec![("src/auth.rs", "pub const allow_empty_token: bool = true;\n")],
            TestModelMode::AssignedFinding,
            vec![test_scope("trace-primary")],
            Some(runtime_events),
        )
        .await;

        assert_eq!(report.findings.len(), 1);
        let trace_kinds = events
            .records()
            .into_iter()
            .filter_map(|record| match record.event {
                RuntimeEvent::AgentTrace { trace_kind, .. } => Some(trace_kind),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            trace_kinds.iter().any(|kind| kind == "model_turn_prepared"),
            "missing model turn preparation trace: {trace_kinds:?}"
        );
        assert!(
            trace_kinds
                .iter()
                .any(|kind| kind == "model_turn_completed.tool_calls"),
            "missing model tool-call output trace: {trace_kinds:?}"
        );
        assert!(
            trace_kinds
                .iter()
                .any(|kind| kind == "tool_calls_requested"),
            "missing requested tool-call trace: {trace_kinds:?}"
        );
        assert!(
            trace_kinds.iter().any(|kind| kind == "tool_batch_planned"),
            "missing tool batch planning trace: {trace_kinds:?}"
        );
        assert!(
            trace_kinds
                .iter()
                .any(|kind| kind == "candidate_finding_decision"),
            "missing candidate decision trace: {trace_kinds:?}"
        );
        assert!(
            trace_kinds
                .iter()
                .any(|kind| kind == "candidate_synthesis_summary"),
            "missing synthesis summary trace: {trace_kinds:?}"
        );
        assert!(
            trace_kinds
                .iter()
                .any(|kind| kind == "risk_playbooks_selected"),
            "missing risk playbook trace: {trace_kinds:?}"
        );
        let prepared_details = events
            .records()
            .into_iter()
            .find_map(|record| match record.event {
                RuntimeEvent::AgentTrace {
                    trace_kind,
                    details,
                    ..
                } if trace_kind == "model_turn_prepared" => Some(details),
                _ => None,
            })
            .expect("prepared trace details");
        let exposed_tools = prepared_details["exposedTools"]
            .as_array()
            .expect("exposed tools");
        assert!(exposed_tools.iter().any(|tool| {
            tool["modelName"] == "grep" && tool["internalToolId"] == "search_text"
        }));
        let playbook_details = events
            .records()
            .into_iter()
            .find_map(|record| match record.event {
                RuntimeEvent::AgentTrace {
                    trace_kind,
                    details,
                    ..
                } if trace_kind == "risk_playbooks_selected" => Some(details),
                _ => None,
            })
            .expect("risk playbook trace details");
        assert!(playbook_details["playbooks"]
            .as_array()
            .expect("playbooks")
            .iter()
            .any(|playbook| playbook.as_str() == Some("AuthScope")));
    }

    #[tokio::test]
    async fn planned_runtime_traces_zero_candidate_synthesis_decision() {
        let events = Arc::new(crate::reviewer::runtime_events::InMemoryEventSink::default());
        let runtime_events: Arc<dyn RuntimeEventSink> = events.clone();
        let report = run_test_review_with_templates_and_events(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::LowRiskClean,
            vec![test_scope("clean-trace-primary")],
            Some(runtime_events),
        )
        .await;

        assert!(report.findings.is_empty());
        let zero_candidate_decision = events
            .records()
            .into_iter()
            .find_map(|record| match record.event {
                RuntimeEvent::AgentTrace {
                    session_id,
                    trace_kind,
                    details,
                    ..
                } if session_id.0 == "synthesis"
                    && trace_kind == "candidate_finding_decision"
                    && details["decision"] == "none" =>
                {
                    Some(details)
                }
                _ => None,
            })
            .expect("zero-candidate decision trace");
        assert_eq!(zero_candidate_decision["phase"], "synthesis");
        assert_eq!(zero_candidate_decision["reason"], "no_candidate_findings");
        assert_eq!(zero_candidate_decision["candidateCount"], 0);
    }

    #[tokio::test]
    async fn planned_runtime_traces_transcript_compaction() {
        let events = Arc::new(crate::reviewer::runtime_events::InMemoryEventSink::default());
        let runtime_events: Arc<dyn RuntimeEventSink> = events.clone();
        let mut template = test_scope("compact-primary");
        template.budget = AgentBudget {
            max_turns: 5,
            max_tool_calls: 12,
            max_prompt_tokens: 1,
            max_output_tokens: 8_000,
            budget_source: crate::contracts::BudgetSource::CallerHardCap,
        };
        let report = run_test_review_with_templates_and_events(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::NeedsFiveTurns,
            vec![template],
            Some(runtime_events),
        )
        .await;

        assert!(report.findings.is_empty());
        let compaction = events
            .records()
            .into_iter()
            .find_map(|record| match record.event {
                RuntimeEvent::AgentTrace {
                    trace_kind,
                    details,
                    ..
                } if trace_kind == "transcript_compacted" => Some(details),
                _ => None,
            })
            .expect("transcript compaction trace");
        assert!(
            compaction["evictedToolResults"]
                .as_u64()
                .expect("evicted tool result count")
                > 0
        );
        assert_eq!(compaction["maxPromptTokens"], 1);
    }

    #[tokio::test]
    async fn high_risk_units_do_not_spawn_explore_workers() {
        let events = Arc::new(crate::reviewer::runtime_events::InMemoryEventSink::default());
        let runtime_events: Arc<dyn RuntimeEventSink> = events.clone();
        let mut template = test_scope("worker-primary");
        template.budget = AgentBudget {
            max_turns: 14,
            max_tool_calls: 64,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
            budget_source: crate::contracts::BudgetSource::CallerHardCap,
        };
        let report = run_test_review_with_templates_and_events(
            vec![(
                "apps/api/token-callback.ts",
                "export function token_callback() { return { credential_token: true }; }\n",
            )],
            TestModelMode::HighRiskCleanWithContractEvidence,
            vec![template],
            Some(runtime_events),
        )
        .await;

        assert_eq!(report.metrics.completed_sessions, 1);
        assert!(report.metrics.model_calls >= 2);
        assert!(report.metrics.tool_counts.search_text > 0);
        let records = events.records();
        let trace_kinds = records
            .iter()
            .filter_map(|record| match &record.event {
                RuntimeEvent::AgentTrace { trace_kind, .. } => Some(trace_kind.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for obsolete in [
            "explore_worker_planned",
            "explore_worker_started",
            "explore_worker_completed",
            "explore_worker_merged",
        ] {
            assert!(
                !trace_kinds.iter().any(|kind| *kind == obsolete),
                "obsolete worker trace {obsolete} should not be emitted: {trace_kinds:?}"
            );
        }
    }

    #[tokio::test]
    async fn planned_runtime_rescues_out_of_unit_candidate_and_keeps_verdict_scope_strict() {
        let report = run_test_review(
            vec![
                (
                    "apps/api/auth-token.ts",
                    "export const auth_token = true;\n",
                ),
                (
                    "apps/api/credential-token.ts",
                    "export const refresh_token = '';\n",
                ),
            ],
            TestModelMode::OutOfUnitFinding,
        )
        .await;
        assert_eq!(report.metrics.sessions, 2);
        assert_eq!(report.metrics.completed_sessions, 2);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].file_refs.len(), 1);
        let EvidenceLocationV1::SinglePath { path } = &report.findings[0].file_refs[0] else {
            panic!("single path finding");
        };
        assert_eq!(path, "apps/api/credential-token.ts");
        let e_review = report
            .file_reviews
            .iter()
            .find(|review| review.path == "apps/api/credential-token.ts")
            .expect("e review");
        assert_eq!(e_review.session_id, "unit-002");
        assert_eq!(e_review.verdict, "issue_found");
        assert_eq!(
            report
                .file_reviews
                .iter()
                .filter(|review| review.path == "apps/api/credential-token.ts")
                .count(),
            1
        );
        assert!(report
            .metrics
            .completion_diagnostics
            .iter()
            .any(|diagnostic| {
                diagnostic.session_id == "synthesis"
                    && diagnostic
                        .completion_summary
                        .as_deref()
                        .unwrap_or_default()
                        .contains("rescuedCandidates=1")
            }));
    }

    #[tokio::test]
    async fn planned_runtime_rejects_invalid_candidates_and_ignores_out_of_unit_verdicts() {
        let report = run_test_review(
            vec![("src/auth.rs", "pub const missing_symbol: bool = true;\n")],
            TestModelMode::InvalidCandidates,
        )
        .await;

        assert!(report.findings.is_empty());
        assert_eq!(report.file_reviews.len(), 1);
        assert_eq!(report.file_reviews[0].path, "src/auth.rs");
        assert_eq!(report.file_reviews[0].verdict, "clean");
        assert!(report
            .metrics
            .completion_diagnostics
            .iter()
            .any(|diagnostic| {
                diagnostic.session_id == "synthesis"
                    && diagnostic
                        .completion_summary
                        .as_deref()
                        .unwrap_or_default()
                        .contains("rejectedCandidates=2")
            }));
    }

    #[tokio::test]
    async fn planned_review_does_not_publish_from_final_synthesis() {
        let report = run_test_review_with_quality_budget(
            vec![(
                "src/widget.rs",
                "pub fn render_widget() -> bool { unsafe_widget_state }\n",
            )],
            TestModelMode::FinalSynthesisFinding,
            expanded_review_budget(),
        )
        .await;

        assert_eq!(report.metrics.sessions, 1);
        assert_eq!(report.metrics.completed_sessions, 1);
        assert_eq!(report.metrics.model_calls, 2);
        assert!(report.findings.is_empty());
        assert!(!report
            .metrics
            .completion_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.session_id == "final-synthesis" && diagnostic.completed));
    }

    #[tokio::test]
    async fn challenge_pass_suppresses_refuted_findings() {
        let report = run_test_review_with_quality_budget(
            vec![("src/auth.rs", "pub const allow_empty_token: bool = true;\n")],
            TestModelMode::ChallengeRefutesFinding,
            expanded_review_budget(),
        )
        .await;

        assert_eq!(report.findings.len(), 1, "refuted findings stay for audit");
        let finding = &report.findings[0];
        assert_eq!(finding.challenged_by, vec!["finding-challenge".to_string()]);
        assert!(matches!(
            finding.validation_status,
            ValidationStatus::Challenged
        ));
        assert!(matches!(finding.report_status, ReportStatus::Suppressed));
        assert!(matches!(
            finding.publishability,
            FindingPublishability::NotPublishable
        ));
        assert!((finding.confidence - REFUTED_CONFIDENCE).abs() < 1e-6);
        assert_eq!(report.metrics.publishable_findings, 0);
        let auth_review = report
            .file_reviews
            .iter()
            .find(|review| review.path == "src/auth.rs")
            .expect("auth review");
        assert_eq!(
            auth_review.verdict, "clean",
            "refuted finding must not flip the file verdict"
        );
        assert!(report
            .metrics
            .completion_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.session_id == "finding-challenge"
                && diagnostic.completed
                && diagnostic
                    .completion_summary
                    .as_deref()
                    .is_some_and(|summary| summary.contains("suppressedFindings=1"))));
    }

    #[tokio::test]
    async fn challenge_pass_confirms_findings_and_boosts_confidence() {
        let report = run_test_review_with_quality_budget(
            vec![("src/auth.rs", "pub const allow_empty_token: bool = true;\n")],
            TestModelMode::AssignedFinding,
            expanded_review_budget(),
        )
        .await;

        assert_eq!(report.findings.len(), 1);
        let finding = &report.findings[0];
        assert_eq!(finding.challenged_by, vec!["finding-challenge".to_string()]);
        assert_eq!(finding.challenge_status, ChallengeStatus::Confirmed);
        assert!(matches!(
            finding.publishability,
            FindingPublishability::Publishable
        ));
        let expected = BASE_CONFIDENCE + CONFIRMED_CONFIDENCE_BOOST;
        assert!(
            (finding.confidence - expected).abs() < 1e-6,
            "confirmed single-discoverer finding should score {expected}, got {}",
            finding.confidence
        );
        assert_eq!(report.metrics.publishable_findings, 1);
        let auth_review = report
            .file_reviews
            .iter()
            .find(|review| review.path == "src/auth.rs")
            .expect("auth review");
        assert_eq!(auth_review.verdict, "issue_found");
    }

    #[test]
    fn empty_challenge_result_leaves_no_applied_verdicts() {
        let mut findings = vec![test_finding(
            "unit-001",
            "src/auth.rs",
            "Empty token accepted",
            "The changed branch accepts an empty token.",
        )];

        let application =
            apply_challenge_verdicts(&mut findings, r#"{"verdicts":[]}"#, "finding-challenge");
        if application.applied_count == 0 {
            mark_challenge_incomplete(&mut findings);
        }

        assert_eq!(application.applied_count, 0);
        assert_eq!(application.suppressed_count, 0);
        assert_eq!(findings[0].challenge_status, ChallengeStatus::Incomplete);
    }

    #[test]
    fn agreement_confidence_scales_with_discoverers_and_caps() {
        assert!((agreement_confidence(1) - BASE_CONFIDENCE).abs() < 1e-6);
        assert!(agreement_confidence(2) > agreement_confidence(1));
        assert!(agreement_confidence(3) > agreement_confidence(2));
        assert!(agreement_confidence(50) <= AGREEMENT_CONFIDENCE_CEILING);
    }

    #[tokio::test]
    async fn review_explorer_publishes_consumer_finding_without_pack_pass() {
        let report = run_test_review_with_quality_budget(
            vec![
                (
                    "packages/app-store/_utils/oauth/refreshOAuthTokens.ts",
                    "export async function refreshOAuthTokens() {\n  return response;\n}\n",
                ),
                (
                    "packages/app-store/googlecalendar/lib/CalendarService.ts",
                    "const credential = await refreshOAuthTokens();\nconst token = credential.access_token;\n",
                ),
            ],
            TestModelMode::PackConsumerFinding,
            expanded_review_budget(),
        )
        .await;

        assert_eq!(report.findings.len(), 1);
        assert_eq!(
            report.findings[0].title,
            "Calendar consumer reads token fields from the raw refresh response"
        );
        let EvidenceLocationV1::SinglePath { path } = &report.findings[0].file_refs[0] else {
            panic!("single path finding");
        };
        assert_eq!(
            path,
            "packages/app-store/googlecalendar/lib/CalendarService.ts"
        );
        assert!(report.findings[0].file_refs.iter().any(|location| {
            matches!(
                location,
                EvidenceLocationV1::SinglePath { path }
                    if path == "packages/app-store/_utils/oauth/refreshOAuthTokens.ts"
            )
        }));
        assert_eq!(report.metrics.quality_diagnostics.contract_pack_count, 0);
        assert!(!report
            .metrics
            .completion_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.session_id.starts_with("pack-")));
    }

    #[tokio::test]
    async fn remediation_loads_missing_file_evidence_instead_of_needs_review() {
        let report = run_test_review_with_budget(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::CleanBeforeEvidence,
            AgentBudget {
                max_turns: 3,
                max_tool_calls: 5,
                max_prompt_tokens: 64_000,
                max_output_tokens: 8_000,
                budget_source: crate::contracts::BudgetSource::CallerHardCap,
            },
        )
        .await;

        assert_eq!(report.file_reviews.len(), 1);
        assert_eq!(report.file_reviews[0].path, "src/widget.rs");
        assert_eq!(report.file_reviews[0].verdict, "clean");
        assert!(report.metrics.tool_counts.read_head_file > 0);
    }

    #[test]
    fn contract_risk_classifier_flags_repeated_callback_batch_and_seeds() {
        let snapshot = build_test_snapshot(vec![
            (
                "apps/api/google/callback.ts",
                "export function callback() { return { credential_token: true }; }\n",
            ),
            (
                "apps/api/zoom/callback.ts",
                "export function callback() { return { credential_token: true }; }\n",
            ),
            (
                "apps/api/webex/callback.ts",
                "export function callback() { return { credential_token: true }; }\n",
            ),
        ]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let risk_plan =
            build_contract_risk_plan(&review_plan, &unit_plan, snapshot.diff.content.as_str());

        assert!(risk_plan.risky_unit_count() > 0);
        assert!(risk_plan.seed_count() > 0);
        assert!(risk_plan
            .by_unit
            .values()
            .any(|risk| risk.seeds.iter().any(|seed| seed.contains("credential"))));
    }

    #[tokio::test]
    async fn low_risk_clean_can_finish_with_assigned_file_evidence_only() {
        let report = run_test_review(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::LowRiskClean,
        )
        .await;

        assert!(report.findings.is_empty());
        assert_eq!(report.file_reviews.len(), 1);
        assert_eq!(report.file_reviews[0].path, "src/widget.rs");
        assert_eq!(report.file_reviews[0].verdict, "clean");
        assert_eq!(report.metrics.completed_sessions, 1);
    }

    #[tokio::test]
    async fn high_risk_clean_requires_contract_evidence() {
        let report = run_test_review_with_budget(
            vec![(
                "apps/api/token-callback.ts",
                "export function callback() { return { credential_token: true }; }\n",
            )],
            TestModelMode::HighRiskCleanWithoutContractEvidence,
            AgentBudget {
                max_turns: 2,
                max_tool_calls: 0,
                max_prompt_tokens: 64_000,
                max_output_tokens: 8_000,
                budget_source: crate::contracts::BudgetSource::CallerHardCap,
            },
        )
        .await;

        assert!(report.findings.is_empty());
        assert_eq!(report.file_reviews.len(), 1);
        assert_eq!(report.file_reviews[0].path, "apps/api/token-callback.ts");
        assert_eq!(report.file_reviews[0].verdict, "needs_review");
        assert!(report.file_reviews[0]
            .summary
            .contains("Required cross-file contract evidence"));
    }

    #[tokio::test]
    async fn high_risk_clean_passes_with_seed_backed_search_evidence() {
        let report = run_test_review(
            vec![(
                "apps/api/token-callback.ts",
                "export function callback() { return { credential_token: true }; }\n",
            )],
            TestModelMode::HighRiskCleanWithContractEvidence,
        )
        .await;

        assert!(report.findings.is_empty());
        assert_eq!(report.file_reviews.len(), 1);
        assert_eq!(report.file_reviews[0].path, "apps/api/token-callback.ts");
        assert_eq!(report.file_reviews[0].verdict, "clean");
        assert!(report.metrics.tool_counts.search_text > 0);
    }

    #[tokio::test]
    async fn deterministic_bootstrap_allows_clean_verdict_without_model_reads() {
        let report = run_test_review_with_quality_budget(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::BootstrapClean,
            expanded_review_budget(),
        )
        .await;

        assert!(report.findings.is_empty());
        assert_eq!(report.metrics.model_calls, 1);
        assert_eq!(report.file_reviews.len(), 1);
        assert_eq!(report.file_reviews[0].path, "src/widget.rs");
        assert_eq!(report.file_reviews[0].verdict, "clean");
        assert!(report.file_reviews[0].evidence_count > 0);
        assert!(report.metrics.tool_counts.read_diff > 0);
        assert!(report.metrics.tool_counts.read_head_file > 0);
    }

    #[tokio::test]
    async fn planned_runtime_honors_budgeted_turns_beyond_four() {
        let report = run_test_review_with_budget(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::NeedsFiveTurns,
            AgentBudget {
                max_turns: 6,
                max_tool_calls: 16,
                max_prompt_tokens: 64_000,
                max_output_tokens: 8_000,
                budget_source: crate::contracts::BudgetSource::CallerHardCap,
            },
        )
        .await;

        assert_eq!(report.metrics.completed_sessions, 1);
        assert_eq!(report.metrics.model_calls, 5);
        assert_eq!(report.file_reviews[0].verdict, "clean");
    }

    #[tokio::test]
    async fn final_turn_strips_tool_grants_when_budget_is_exhausted() {
        let report = run_test_review_with_budget(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::AssertFinalTurnHasNoTools,
            AgentBudget {
                max_turns: 5,
                max_tool_calls: 0,
                max_prompt_tokens: 64_000,
                max_output_tokens: 8_000,
                budget_source: crate::contracts::BudgetSource::CallerHardCap,
            },
        )
        .await;

        assert_eq!(report.metrics.completed_sessions, 1);
        assert_eq!(report.metrics.model_calls, 1);
        assert_eq!(report.file_reviews[0].verdict, "needs_review");
    }

    #[test]
    fn final_response_scope_strips_tools_and_sets_response_format() {
        let scope = test_scope("unit-test");
        let final_scope = final_response_scope(&scope, unit_result_response_format());

        assert!(final_scope.capabilities.tool_grants.is_empty());
        assert_eq!(
            final_scope
                .response_format
                .as_ref()
                .map(|format| format.name.as_str()),
            Some("muzen_review_unit_result_v1")
        );
        assert_eq!(scope.response_format, None);
    }

    #[test]
    fn deterministic_bootstrap_uses_ranges_for_oversized_files() {
        let snapshot = build_test_snapshot(vec![(
            "src/large.rs",
            "pub fn large_widget() -> bool { true }\n",
        )]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let calls = deterministic_bootstrap_calls(
            &review_plan,
            unit_plan.units.first().expect("unit"),
            &NO_CONTRACT_RISK,
            &DiffPackContext::empty(),
            snapshot.diff.content.as_str(),
            1,
            8,
        );

        assert_eq!(calls.calls[0].name, ToolId::from(ToolName::ReadDiff));
        assert!(calls.calls.iter().any(|call| {
            call.name == ToolId::from(ToolName::ReadFileRange)
                && call.raw_arguments.contains(r#""path":"src/large.rs""#)
        }));
    }

    #[test]
    fn deterministic_bootstrap_reserves_followup_budget_and_skips_low_priority_files() {
        let snapshot = build_test_snapshot(vec![
            ("src/a.rs", "pub fn a() -> bool { true }\n"),
            ("src/b.rs", "pub fn b() -> bool { true }\n"),
            ("src/c.rs", "pub fn c() -> bool { true }\n"),
        ]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let plan = deterministic_bootstrap_calls(
            &review_plan,
            unit_plan.units.first().expect("unit"),
            &NO_CONTRACT_RISK,
            &DiffPackContext::empty(),
            snapshot.diff.content.as_str(),
            64 * 1024,
            6,
        );

        assert_eq!(plan.calls.len(), 2);
        assert_eq!(plan.calls[0].name, ToolId::from(ToolName::ReadDiff));
        assert_eq!(plan.skipped_paths.len(), 2);
    }

    #[test]
    fn deterministic_bootstrap_adds_contract_seed_searches_for_high_risk_units() {
        let snapshot = build_test_snapshot(vec![(
            "apps/api/callback.ts",
            "export function callback() { return { credential_token: true }; }\n",
        )]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let risk_plan =
            build_contract_risk_plan(&review_plan, &unit_plan, snapshot.diff.content.as_str());
        let unit = unit_plan.units.first().expect("unit");
        let plan = deterministic_bootstrap_calls(
            &review_plan,
            unit,
            risk_plan.unit_risk(unit),
            &DiffPackContext::empty(),
            snapshot.diff.content.as_str(),
            64 * 1024,
            8,
        );

        assert!(plan
            .calls
            .iter()
            .any(|call| call.name == ToolId::from(ToolName::SearchText)));
    }

    #[test]
    fn deterministic_bootstrap_uses_assigned_paths_without_contract_packs() {
        let snapshot = build_test_snapshot(vec![
            (
                "packages/app-store/_utils/oauth/refreshOAuthTokens.ts",
                "const refreshOAuthTokens = async () => { return response; }\nexport default refreshOAuthTokens;\n",
            ),
            (
                "packages/app-store/googlecalendar/lib/CalendarService.ts",
                "import refreshOAuthTokens from '../../_utils/oauth/refreshOAuthTokens';\nconst res = await refreshOAuthTokens();\n",
            ),
            ("src/low.rs", "pub fn low() -> bool { true }\n"),
        ]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let unit = unit_plan
            .units
            .iter()
            .find(|unit| {
                unit.file_paths.iter().any(|path| {
                    path.display() == "packages/app-store/_utils/oauth/refreshOAuthTokens.ts"
                })
            })
            .expect("unit");
        let plan = deterministic_bootstrap_calls(
            &review_plan,
            unit,
            &NO_CONTRACT_RISK,
            &DiffPackContext::empty(),
            snapshot.diff.content.as_str(),
            64 * 1024,
            8,
        );
        for call in plan
            .calls
            .iter()
            .filter(|call| call.name != ToolId::from(ToolName::ReadDiff))
        {
            assert!(
                unit.file_paths
                    .iter()
                    .any(|path| call.raw_arguments.contains(&path.display())),
                "bootstrap call should target an assigned path, got {}",
                call.raw_arguments
            );
        }
    }

    #[test]
    fn time_boundary_scanner_finds_working_hours_end_reusing_start() {
        let diff = r#"diff --git a/packages/trpc/server/routers/viewer/slots.ts b/packages/trpc/server/routers/viewer/slots.ts
--- a/packages/trpc/server/routers/viewer/slots.ts
+++ b/packages/trpc/server/routers/viewer/slots.ts
@@ -91,0 +92,7 @@
+const workingHour = workingHours.find((workingHour) => {
+  const start = slotStartTime.hour() * 60 + slotStartTime.minute();
+  const end = slotStartTime.hour() * 60 + slotStartTime.minute();
+  return workingHour.startTime <= start && end <= workingHour.endTime;
+});
"#;

        assert_eq!(
            working_hours_end_reuses_start_line(
                diff,
                "packages/trpc/server/routers/viewer/slots.ts"
            ),
            Some(94)
        );
    }

    #[test]
    fn time_boundary_scanner_finds_dayjs_reference_equality() {
        let diff = r#"diff --git a/packages/trpc/server/routers/viewer/slots.ts b/packages/trpc/server/routers/viewer/slots.ts
--- a/packages/trpc/server/routers/viewer/slots.ts
+++ b/packages/trpc/server/routers/viewer/slots.ts
@@ -108,0 +109,7 @@
+if (
+  dayjs(date.start).add(utcOffset, "minutes") === dayjs(date.end).add(utcOffset, "minutes")
+) {
+  return true;
+}
"#;

        assert_eq!(
            date_override_dayjs_reference_line(
                diff,
                "packages/trpc/server/routers/viewer/slots.ts"
            ),
            Some(110)
        );
    }

    #[test]
    fn time_boundary_scanner_finds_selected_slot_date_override_filter() {
        let diff = r#"diff --git a/packages/trpc/server/routers/viewer/slots.ts b/packages/trpc/server/routers/viewer/slots.ts
--- a/packages/trpc/server/routers/viewer/slots.ts
+++ b/packages/trpc/server/routers/viewer/slots.ts
@@ -576,0 +577,14 @@
 availableTimeSlots = availableTimeSlots
   .map((slot) => {
     slot.userIds = slot.userIds?.filter((slotUserId) => {
       const busy = selectedSlots.reduce<EventBusyDate[]>((r, c) => r, []);
       if (!busy?.length && eventType.seatsPerTimeSlot === null) {
         return false;
       }
+      const userSchedule = userAvailability.find(({ user: { id: userId } }) => userId === slotUserId);
       return checkIfIsAvailable({
         time: slot.time,
         busy,
         ...availabilityCheckProps,
+        organizerTimeZone: userSchedule?.timeZone,
       });
     });
"#;

        assert_eq!(
            selected_slot_filters_date_override_line(
                diff,
                "packages/trpc/server/routers/viewer/slots.ts"
            ),
            Some(584)
        );
    }

    #[test]
    fn synthesis_rejects_contract_claim_without_behavior_comparison() {
        let snapshot = build_test_snapshot(vec![(
            "src/helper.rs",
            "pub fn helper() -> bool { return true }\n",
        )]);
        let review_plan = build_review_plan(&snapshot);
        let artifacts = ConcurrentArtifactStore::default();
        let artifact = artifacts.insert(
            ArtifactKey("helper".to_string()),
            "read_file src/helper.rs return true".to_string(),
        );
        let candidate = CandidateFinding {
            source_unit_id: "unit-001".to_string(),
            source_session_id: "unit-001".to_string(),
            title: "Helper return contract is wrong".to_string(),
            claim: "The helper return contract is wrong for callers.".to_string(),
            path: "src/helper.rs".to_string(),
            related_paths: Vec::new(),
            start_line: Some(1),
            end_line: Some(1),
            behavior_before: String::new(),
            behavior_after: String::new(),
            predicate: String::new(),
            evidence_artifact_ids: vec![artifact.0],
            source_unit_assigned_path: true,
            rejection_reason: None,
        };

        let synthesis = synthesize_findings(
            &review_plan,
            &[],
            &ContractRiskPlan::default(),
            snapshot.diff.content.as_str(),
            vec![candidate],
            &artifacts,
            "head",
        );

        assert!(synthesis.findings.is_empty());
        assert_eq!(
            synthesis
                .rejection_reasons
                .get("contract_missing_behavior_comparison"),
            Some(&1)
        );
    }

    #[test]
    fn synthesis_rejects_preserved_behavior_observation() {
        let snapshot = build_test_snapshot(vec![(
            "src/callback.ts",
            "export function callback() { return persistTokenPayload(); }\n",
        )]);
        let review_plan = build_review_plan(&snapshot);
        let artifacts = ConcurrentArtifactStore::default();
        let artifact = artifacts.insert(
            ArtifactKey("callback".to_string()),
            "read_file src/callback.ts persistTokenPayload".to_string(),
        );
        let candidate = CandidateFinding {
            source_unit_id: "unit-001".to_string(),
            source_session_id: "unit-001".to_string(),
            title: "Callback still persists the OAuth token payload".to_string(),
            claim: "No new incompatible runtime shape is introduced in this callback path; it still writes the token payload into the credential.".to_string(),
            path: "src/callback.ts".to_string(),
            related_paths: Vec::new(),
            start_line: Some(1),
            end_line: Some(1),
            behavior_before: "callback persisted the token payload".to_string(),
            behavior_after: "callback still persists the token payload".to_string(),
            predicate: String::new(),
            evidence_artifact_ids: vec![artifact.0],
            source_unit_assigned_path: true,
            rejection_reason: None,
        };

        let synthesis = synthesize_findings(
            &review_plan,
            &[],
            &ContractRiskPlan::default(),
            snapshot.diff.content.as_str(),
            vec![candidate],
            &artifacts,
            "head",
        );

        assert!(synthesis.findings.is_empty());
        assert_eq!(
            synthesis.rejection_reasons.get("non_finding_text"),
            Some(&1)
        );
    }

    #[test]
    fn synthesis_accepts_query_scope_claim_with_changed_predicate() {
        let snapshot = build_test_snapshot(vec![(
            "src/workflow.rs",
            "pub fn cleanup() { deleteMany(where: { OR: [method, retryCount] }) }\n",
        )]);
        let review_plan = build_review_plan(&snapshot);
        let artifacts = ConcurrentArtifactStore::default();
        let artifact = artifacts.insert(
            ArtifactKey("workflow".to_string()),
            "read_file src/workflow.rs deleteMany OR method retryCount".to_string(),
        );
        let candidate = CandidateFinding {
            source_unit_id: "unit-001".to_string(),
            source_session_id: "unit-001".to_string(),
            title: "Changed cleanup query broadens the method scope".to_string(),
            claim: "The changed deleteMany where predicate now puts retryCount in a standalone OR branch, so cleanup can remove rows outside the method scope.".to_string(),
            path: "src/workflow.rs".to_string(),
            related_paths: Vec::new(),
            start_line: Some(1),
            end_line: Some(1),
            behavior_before: "cleanup deleted only rows matched by the method predicate"
                .to_string(),
            behavior_after: "cleanup deletes every row with retryCount > 1 via the OR branch"
                .to_string(),
            predicate: "OR: [method, retryCount]".to_string(),
            evidence_artifact_ids: vec![artifact.0],
            source_unit_assigned_path: true,
            rejection_reason: None,
        };

        let synthesis = synthesize_findings(
            &review_plan,
            &[],
            &ContractRiskPlan::default(),
            snapshot.diff.content.as_str(),
            vec![candidate],
            &artifacts,
            "head",
        );

        assert_eq!(synthesis.findings.len(), 1);
    }

    #[test]
    fn planned_unit_prompt_lists_review_explorer_evidence_goals() {
        let snapshot = build_test_snapshot(vec![
            (
                "packages/app-store/_utils/oauth/refreshOAuthTokens.ts",
                "export async function refreshOAuthTokens() {\n  return response;\n}\n",
            ),
            (
                "packages/app-store/googlecalendar/lib/CalendarService.ts",
                "const credential = await refreshOAuthTokens();\n",
            ),
        ]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let contract_risk =
            build_contract_risk_plan(&review_plan, &unit_plan, snapshot.diff.content.as_str());
        let unit = unit_plan
            .units
            .iter()
            .find(|unit| {
                unit.file_paths.iter().any(|path| {
                    path.display() == "packages/app-store/_utils/oauth/refreshOAuthTokens.ts"
                })
            })
            .expect("unit");
        let transcript = planned_unit_transcript(
            &review_plan,
            unit,
            contract_risk.unit_risk(unit),
            None,
            &DiffPackContext::empty(),
        );
        let prompt = transcript
            .iter()
            .filter_map(|item| match item {
                ConversationItem::System { content } | ConversationItem::User { content } => {
                    Some(content.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(prompt.contains("ReviewExplorer"));
        assert!(
            prompt.contains("Inspect the diff first") || prompt.contains("inspect the diff first")
        );
        assert!(prompt.contains("Explorer evidence goals"));
        assert!(prompt.contains("DiffFirst"));
        assert!(prompt.contains("AssignedFiles"));
        assert!(prompt.contains("Risk playbooks"));
        assert!(prompt.contains("ReturnShape"));
        assert!(prompt.contains("each reachable producer branch separately"));
        assert!(prompt.contains("a fallback branch that preserves the old shape does not prove"));
        assert!(prompt.contains("Do not put observations, preserved behavior"));
    }

    #[test]
    fn embedded_unit_result_drops_no_bug_observation_findings() {
        let unit = PlannedReviewUnit {
            id: "unit-001".to_string(),
            file_paths: vec![RepoPath::parse("src/service.ts").expect("path")],
            score_min: 1,
            score_max: 1,
            estimated_bytes: 100,
            file_count: 1,
            requires_further_split: false,
        };
        let content = r#"The review is complete.
{
  "summary": "Reviewed the assigned files and no definite correctness bug is supported by the evidence gathered.",
  "fileVerdicts": [
    {
      "path": "src/service.ts",
      "verdict": "clean",
      "summary": "No supported bug.",
      "relatedPaths": []
    }
  ],
  "findings": [
    {
      "title": "Refresh flow preserves the credential token contract",
      "claim": "The changed helper still consumes the parsed token payload.",
      "path": "src/service.ts",
      "relatedPaths": [],
      "startLine": 1,
      "endLine": 1,
      "behaviorBefore": "parsed token payload was used",
      "behaviorAfter": "parsed token payload is still used",
      "predicate": ""
    }
  ]
}"#;

        let parsed = parse_unit_result(content, &unit);

        assert!(parsed.parsed);
        assert!(parsed.extracted_json);
        assert_eq!(parsed.result.file_verdicts.len(), 1);
        assert!(parsed.result.findings.is_empty());
    }

    #[test]
    fn planned_unit_prompt_treats_boundary_and_date_value_bugs_as_publishable() {
        let snapshot = build_test_snapshot(vec![(
            "packages/trpc/server/routers/viewer/slots.ts",
            "export function checkIfIsAvailable(time, busy) {\n  const slotEndTime = time.add(30, 'minutes');\n  if (date.start === date.end) return true;\n  if (isWithinBounds(time, time)) return true;\n  return !busy.length;\n}\n",
        )]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let contract_risk =
            build_contract_risk_plan(&review_plan, &unit_plan, snapshot.diff.content.as_str());
        let pack_plan = DiffPackContext::empty();
        let unit = unit_plan
            .units
            .iter()
            .find(|unit| {
                unit.file_paths
                    .iter()
                    .any(|path| path.display() == "packages/trpc/server/routers/viewer/slots.ts")
            })
            .expect("unit");
        let transcript = planned_unit_transcript(
            &review_plan,
            unit,
            contract_risk.unit_risk(unit),
            None,
            &pack_plan,
        );
        let prompt = transcript
            .iter()
            .filter_map(|item| match item {
                ConversationItem::System { content } | ConversationItem::User { content } => {
                    Some(content.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(prompt.contains("validates the start instant where the end instant is required"));
        assert!(prompt.contains("compares wrapper/date objects by identity instead of value"));
        assert!(prompt.contains("returns available before later busy/capacity checks can run"));
        assert!(prompt.contains("TimeBoundary"));
    }

    #[test]
    fn synthesis_merge_collapses_adjacent_same_line_claims() {
        let first = test_finding(
            "unit-001",
            "src/workflow.rs",
            "deleteMany removes unrelated reminders",
            "The changed deleteMany branch deletes any retryCount reminder.",
        );
        let duplicate = test_finding(
            "final-synthesis",
            "src/workflow.rs",
            "retryCount cleanup deletes unrelated reminders",
            "The changed deleteMany retryCount condition removes reminders outside the SMS scope.",
        );

        assert!(should_merge_findings(&first, &duplicate));
    }

    #[test]
    fn synthesis_merge_collapses_overlapping_claims_with_shared_evidence() {
        let mut first = test_finding(
            "unit-001",
            "src/workflow.rs",
            "Unscoped retryCount branch deletes non-SMS reminders",
            "The changed OR predicate now matches retryCount rows outside method scope.",
        );
        first.location_line_range = Some(LineRangeV1 {
            start_line: 31,
            end_line: 43,
        });
        first.evidence = vec![EvidenceRefV1 {
            evidence_id: "evidence-a".to_string(),
            artifact_id: "artifact-a".to_string(),
            kind: ArtifactKind::ToolSummary,
            revision: EvidenceRevision::Head,
            revision_id: "head".to_string(),
            location: EvidenceLocationV1::SinglePath {
                path: "src/workflow.rs".to_string(),
            },
            line_range: None,
            byte_range: None,
            diff_anchor: None,
            content_hash: "hash-a".to_string(),
            redaction: RedactionMetadataV1 {
                redaction_state: RedactionState::None,
                redaction_policy_id: "test".to_string(),
                contains_repo_content: false,
                contains_prompt_content: false,
                contains_model_output: false,
                contains_secret_material: false,
            },
            producing_tool_call_id: "call-a".to_string(),
        }];
        let mut duplicate = test_finding(
            "final-synthesis",
            "src/workflow.rs",
            "Destructive cleanup bypasses method scope",
            "The cleanup can remove reminders for other delivery methods.",
        );
        duplicate.location_line_range = Some(LineRangeV1 {
            start_line: 33,
            end_line: 33,
        });
        duplicate.evidence = first.evidence.clone();

        assert!(should_merge_findings(&first, &duplicate));
    }

    async fn run_test_review(
        files: Vec<(&'static str, &'static str)>,
        mode: TestModelMode,
    ) -> PlannedReviewRunReport {
        run_test_review_with_templates(files, mode, Vec::new()).await
    }

    async fn run_test_review_with_budget(
        files: Vec<(&'static str, &'static str)>,
        mode: TestModelMode,
        budget: AgentBudget,
    ) -> PlannedReviewRunReport {
        run_test_review_with_budget_objective(files, mode, budget, "template").await
    }

    async fn run_test_review_with_quality_budget(
        files: Vec<(&'static str, &'static str)>,
        mode: TestModelMode,
        budget: AgentBudget,
    ) -> PlannedReviewRunReport {
        run_test_review_with_budget_objective(
            files,
            mode,
            budget,
            "Review this pull request for actionable correctness bugs.",
        )
        .await
    }

    async fn run_test_review_with_budget_objective(
        files: Vec<(&'static str, &'static str)>,
        mode: TestModelMode,
        budget: AgentBudget,
        objective: &str,
    ) -> PlannedReviewRunReport {
        let template = SessionScope {
            id: SessionId("template".to_string()),
            role: Role::Generalist,
            objective: objective.to_string(),
            instructions: Vec::new(),
            snapshot_id: None,
            model_profile_id: None,
            response_format: None,
            capabilities: CapabilitySet::review_read_only(),
            budget,
        };
        run_test_review_with_templates(files, mode, vec![template]).await
    }

    fn expanded_review_budget() -> AgentBudget {
        AgentBudget {
            max_turns: 6,
            max_tool_calls: 16,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
            budget_source: crate::contracts::BudgetSource::PlannedDefault,
        }
    }

    fn test_scope(id: &str) -> SessionScope {
        SessionScope {
            id: SessionId(id.to_string()),
            role: Role::Generalist,
            objective: "test".to_string(),
            instructions: Vec::new(),
            snapshot_id: None,
            model_profile_id: None,
            response_format: None,
            capabilities: CapabilitySet::review_read_only(),
            budget: expanded_review_budget(),
        }
    }

    fn test_finding(session_id: &str, path: &str, title: &str, claim: &str) -> FindingV1 {
        FindingV1 {
            id: stable_id(&[session_id, path, title, claim]),
            title: title.to_string(),
            claim: claim.to_string(),
            severity: FindingSeverity::Low,
            confidence: 0.72,
            validation_status: ValidationStatus::Validated,
            report_status: ReportStatus::Included,
            publishability: FindingPublishability::Publishable,
            challenge_status: ChallengeStatus::NotRun,
            evidence: Vec::new(),
            file_refs: vec![EvidenceLocationV1::SinglePath {
                path: path.to_string(),
            }],
            location_line_range: Some(LineRangeV1 {
                start_line: 1,
                end_line: 1,
            }),
            discovered_by: vec![session_id.to_string()],
            challenged_by: Vec::new(),
        }
    }

    async fn run_test_review_with_templates(
        files: Vec<(&'static str, &'static str)>,
        mode: TestModelMode,
        session_templates: Vec<SessionScope>,
    ) -> PlannedReviewRunReport {
        run_test_review_with_templates_and_events(files, mode, session_templates, None).await
    }

    async fn run_test_review_with_templates_and_events(
        files: Vec<(&'static str, &'static str)>,
        mode: TestModelMode,
        session_templates: Vec<SessionScope>,
        runtime_events: Option<Arc<dyn RuntimeEventSink>>,
    ) -> PlannedReviewRunReport {
        let snapshot = build_test_snapshot(files);
        let limits = Arc::new(RuntimeLimits::standard(2, 64 * 1024, 20));
        let active_sessions = session_semaphore(&limits);
        let tools = Arc::new(ToolEngine::new(Arc::clone(&snapshot), Arc::clone(&limits)).unwrap());
        let runtime = Arc::new(PlannedReviewRuntime {
            snapshot,
            model_router: Arc::new(StaticModelRouter::new(Arc::new(EvidenceBackedModel {
                mode,
            }))),
            tools,
            policy: Arc::new(ReviewerPolicy::new()),
            limits,
            review_revision_id: "head".to_string(),
            events: RuntimeEventDispatcher::new(runtime_events),
            session_templates,
            active_sessions,
        });

        runtime.run_with_cancel(CancellationToken::new()).await
    }

    /// Reads every file in its unit on the first turn, then returns clean
    /// verdicts, while recording how many model calls overlap in time.
    struct ConcurrencyProbeModel {
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl crate::runtime::model::ConcurrentModelClient for ConcurrencyProbeModel {
        async fn complete(
            &self,
            scope: &SessionScope,
            transcript: &[ConversationItem],
            _turn_id: TurnId,
            _cancel: CancellationToken,
        ) -> RuntimeResult<ModelTurn> {
            use std::sync::atomic::Ordering;
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            let unit_files: Vec<String> = scope
                .instructions
                .iter()
                .find(|instruction| instruction.kind == "changed_file_batch")
                .map(|instruction| {
                    instruction
                        .text
                        .lines()
                        .filter_map(|line| line.split_once(". ").map(|(_, path)| path.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let first_turn = !transcript
                .iter()
                .any(|item| matches!(item, ConversationItem::AssistantToolCalls { .. }));
            if first_turn {
                let calls = unit_files
                    .iter()
                    .enumerate()
                    .map(|(index, path)| ModelToolCall {
                        call_id: ToolCallId(format!("{}-read-{index}", scope.id.0)),
                        index,
                        name: ToolId::from(ToolName::ReadHeadFile),
                        raw_arguments: json!({ "path": path }).to_string(),
                    })
                    .collect();
                return Ok(ModelTurn::ToolCalls {
                    calls,
                    usage: TokenUsage::default(),
                });
            }
            Ok(ModelTurn::Text {
                content: json!({
                    "summary": "clean",
                    "fileVerdicts": unit_files
                        .iter()
                        .map(|path| json!({ "path": path, "verdict": "clean" }))
                        .collect::<Vec<_>>(),
                    "findings": []
                })
                .to_string(),
                usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn planned_units_execute_concurrently_up_to_max_active_sessions() {
        let files: Vec<(String, String)> = (0..12)
            .map(|index| {
                (
                    format!("src/file_{index}.rs"),
                    format!("fn handler_{index}() -> usize {{ {index} }}"),
                )
            })
            .collect();
        let snapshot = build_owned_test_snapshot(files);
        let limits = Arc::new(RuntimeLimits::standard(4, 64 * 1024, 20));
        let active_sessions = session_semaphore(&limits);
        let tools = Arc::new(ToolEngine::new(Arc::clone(&snapshot), Arc::clone(&limits)).unwrap());
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runtime = Arc::new(PlannedReviewRuntime {
            snapshot,
            model_router: Arc::new(StaticModelRouter::new(Arc::new(ConcurrencyProbeModel {
                in_flight: Arc::clone(&in_flight),
                max_in_flight: Arc::clone(&max_in_flight),
            }))),
            tools,
            policy: Arc::new(ReviewerPolicy::new()),
            limits,
            review_revision_id: "head".to_string(),
            events: RuntimeEventDispatcher::new(None),
            session_templates: Vec::new(),
            active_sessions,
        });

        let report = runtime.run_with_cancel(CancellationToken::new()).await;

        assert!(
            report.metrics.sessions >= 2,
            "expected multiple planned units, got {}",
            report.metrics.sessions
        );
        assert_eq!(
            report.metrics.completed_sessions, report.metrics.sessions,
            "all units should complete cleanly: {:?}",
            report.metrics.completion_diagnostics
        );
        let observed = max_in_flight.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            observed >= 2,
            "expected overlapping model calls across units, max in-flight was {observed}"
        );
    }

    fn lens_template(id: &str, role: Role) -> SessionScope {
        SessionScope {
            id: SessionId(id.to_string()),
            role,
            objective: "template".to_string(),
            instructions: Vec::new(),
            snapshot_id: None,
            model_profile_id: None,
            response_format: None,
            capabilities: CapabilitySet::review_read_only(),
            budget: AgentBudget {
                max_turns: 2,
                max_tool_calls: 8,
                max_prompt_tokens: 64_000,
                max_output_tokens: 8_000,
                budget_source: crate::contracts::BudgetSource::PlannedDefault,
            },
        }
    }

    #[test]
    fn unit_lens_template_indices_dedupe_roles_and_cap_fanout() {
        let hot = LENS_FANOUT_MIN_SCORE;
        assert_eq!(unit_lens_template_indices(&[], true, hot), vec![None]);
        let single = vec![lens_template("a", Role::Security)];
        assert_eq!(
            unit_lens_template_indices(&single, true, hot),
            vec![Some(0)]
        );
        let many = vec![
            lens_template("a", Role::Correctness),
            lens_template("b", Role::Security),
            lens_template("c", Role::Correctness),
            lens_template("d", Role::Performance),
            lens_template("e", Role::Maintainability),
        ];
        assert_eq!(unit_lens_template_indices(&many, false, hot), vec![Some(0)]);
        assert_eq!(
            unit_lens_template_indices(&many, true, hot - 1),
            vec![Some(0)],
            "high-risk units below the score gate keep a single lens"
        );
        assert_eq!(unit_lens_template_indices(&many, true, hot), vec![Some(0)]);
    }

    #[test]
    fn unverdicted_assigned_files_get_explicit_needs_review() {
        let unit = PlannedReviewUnit {
            id: "unit-000".to_string(),
            file_paths: vec![
                RepoPath::parse("src/auth.rs").expect("path"),
                RepoPath::parse("src/widget.rs").expect("path"),
            ],
            score_min: 35,
            score_max: 70,
            estimated_bytes: 100,
            file_count: 2,
            requires_further_split: false,
        };
        let mut reviews = vec![FileReviewV1 {
            path: "src/auth.rs".to_string(),
            verdict: "clean".to_string(),
            coverage: ReviewCoverage::Standard,
            review_verdict: ReviewVerdict::Clean,
            summary: "reviewed".to_string(),
            related_paths: Vec::new(),
            evidence_artifact_ids: Vec::new(),
            evidence_count: 1,
            session_id: "unit-000".to_string(),
            unit_id: "unit-000".to_string(),
        }];

        append_unverdicted_file_reviews(std::slice::from_ref(&unit), &mut reviews);

        assert_eq!(reviews.len(), 2);
        let widget = reviews
            .iter()
            .find(|review| review.path == "src/widget.rs")
            .expect("widget review");
        assert_eq!(widget.verdict, "needs_review");
        assert_eq!(widget.unit_id, "unit-000");
        assert_eq!(reviews[0].verdict, "clean", "existing reviews untouched");

        append_unverdicted_file_reviews(std::slice::from_ref(&unit), &mut reviews);
        assert_eq!(reviews.len(), 2, "invariant is idempotent");
    }

    #[test]
    fn lens_focus_shapes_secondary_lens_prompts_only() {
        assert!(lens_focus(0, Role::Security).is_none());
        let snapshot = build_test_snapshot(vec![(
            "src/widget.rs",
            "pub fn render_widget() -> bool { true }\n",
        )]);
        let review_plan = build_review_plan(&snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        let contract_risk =
            build_contract_risk_plan(&review_plan, &unit_plan, snapshot.diff.content.as_str());
        let unit = &unit_plan.units[0];
        let unit_risk = contract_risk.unit_risk(unit);
        let transcript = planned_unit_transcript(
            &review_plan,
            unit,
            unit_risk,
            lens_focus(1, Role::Security),
            &DiffPackContext::empty(),
        );
        let ConversationItem::System { content } = &transcript[0] else {
            panic!("expected system item");
        };
        assert!(content.contains("Lens focus: security"));
        let baseline = planned_unit_transcript(
            &review_plan,
            unit,
            unit_risk,
            None,
            &DiffPackContext::empty(),
        );
        let ConversationItem::System { content: baseline } = &baseline[0] else {
            panic!("expected system item");
        };
        assert!(!baseline.contains("Lens focus"));
    }

    #[tokio::test]
    async fn high_risk_unit_uses_one_primary_explorer() {
        let report = run_test_review_with_templates(
            vec![(
                "apps/api/token-callback.ts",
                "export function callback() { return { credential_token: true }; }\n",
            )],
            TestModelMode::HighRiskCleanWithContractEvidence,
            vec![
                lens_template("persona-0", Role::Correctness),
                lens_template("persona-1", Role::Security),
                lens_template("persona-2", Role::Correctness),
                lens_template("persona-3", Role::Performance),
                lens_template("persona-4", Role::Maintainability),
            ],
        )
        .await;

        assert_eq!(report.metrics.sessions, 1);
        assert_eq!(report.metrics.completed_sessions, 1);
        let session_ids: Vec<&str> = report
            .metrics
            .completion_diagnostics
            .iter()
            .map(|diagnostic| diagnostic.session_id.as_str())
            .collect();
        assert!(
            !session_ids.iter().any(|id| id.contains('#')),
            "planned units should not create secondary lens sessions: {session_ids:?}"
        );
        assert_eq!(
            report.file_reviews.len(),
            1,
            "secondary lenses must not duplicate file reviews: {:?}",
            report.file_reviews
        );
        assert_eq!(report.file_reviews[0].verdict, "clean");
    }

    #[tokio::test]
    async fn low_risk_units_keep_a_single_lens_session() {
        let report = run_test_review_with_templates(
            vec![("src/widget.rs", "pub fn render_widget() -> bool { true }\n")],
            TestModelMode::LowRiskClean,
            vec![
                lens_template("persona-0", Role::Correctness),
                lens_template("persona-1", Role::Security),
                lens_template("persona-2", Role::Performance),
            ],
        )
        .await;

        assert_eq!(report.metrics.sessions, 1);
        assert_eq!(report.metrics.completed_sessions, 1);
        assert_eq!(report.file_reviews.len(), 1);
        assert_eq!(report.file_reviews[0].verdict, "clean");
    }

    #[tokio::test]
    async fn unit_findings_merge_without_lens_fanout_attribution() {
        let report = run_test_review_with_templates(
            vec![
                (
                    "apps/api/token-callback.ts",
                    "export function callback() { return { credential_token: true }; }\n",
                ),
                ("src/auth.rs", "pub const allow_empty_token: bool = true;\n"),
            ],
            TestModelMode::AssignedFinding,
            vec![
                lens_template("persona-0", Role::Correctness),
                lens_template("persona-1", Role::Security),
                lens_template("persona-2", Role::Performance),
            ],
        )
        .await;

        assert_eq!(
            report.findings.len(),
            1,
            "lens duplicates must merge: {:?}",
            report.findings
        );
        let finding = &report.findings[0];
        assert!(
            !finding.discovered_by.is_empty(),
            "merged finding should credit the discovering unit session: {:?}",
            finding.discovered_by
        );
        assert!(
            finding.discovered_by.iter().all(|id| !id.contains('#')),
            "discoverers should be unit sessions, not lens sessions: {:?}",
            finding.discovered_by
        );
        assert!(finding.confidence <= MAX_CONFIDENCE);
        assert_eq!(finding.challenge_status, ChallengeStatus::Confirmed);
    }

    fn build_test_snapshot(files: Vec<(&'static str, &'static str)>) -> Arc<RepoSnapshot> {
        build_owned_test_snapshot(
            files
                .into_iter()
                .map(|(path, content)| (path.to_string(), content.to_string()))
                .collect(),
        )
    }

    fn build_owned_test_snapshot(files: Vec<(String, String)>) -> Arc<RepoSnapshot> {
        let temp = tempfile::tempdir().expect("tempdir");
        for (path, content) in &files {
            let path = temp.path().join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, content).expect("write");
        }
        let inline_diff = files
            .iter()
            .map(|(path, content)| {
                format!(
                    "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,0 +1,1 @@\n+{}",
                    content.trim_end()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let change = ChangeScopeV1 {
            kind: ChangeKind::LocalDiff,
            change_id: "change".to_string(),
            source_ref: "head".to_string(),
            target_ref: "base".to_string(),
            base_revision_id: "base".to_string(),
            head_revision_id: "head".to_string(),
            merge_base_revision_id: None,
            changed_files_manifest_ref: None,
            diff_manifest_ref: None,
            inline_diff: Some(inline_diff),
            snapshot_mode: SnapshotMode::WorktreeHead,
            rename_detection: RenameDetection::None,
            changed_files: files
                .iter()
                .map(|(path, _)| ChangedFileEntryV1 {
                    status: ChangedFileStatus::Modified,
                    old_path: Some(PathBuf::from(path)),
                    new_path: Some(PathBuf::from(path)),
                    old_content_hash: None,
                    new_content_hash: None,
                    is_binary: false,
                    is_generated: false,
                })
                .collect(),
        };
        RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64 * 1024, 20), &change)
            .expect("snapshot")
    }
}
