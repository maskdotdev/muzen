use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::contracts::{
    ArtifactKind, ByteRangeV1, EventLevel, EventType, EvidenceLocationV1, EvidenceRefV1,
    EvidenceRevision, FileReviewV1, FindingPublishability, FindingSeverity, FindingV1, LineRangeV1,
    RedactionMetadataV1, RedactionState, ReportStatus, Role, TokenUsage, ToolCounts, ToolName,
    ValidationStatus,
};
use crate::events::EventRecord;
use crate::review_plan::ReviewPlanFileMode;
use crate::review_plan::{build_review_plan, ReviewPlan};
use crate::review_units::{build_review_unit_plan, PlannedReviewUnit, ReviewUnitOptions};
use crate::runtime::contracts::*;
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::effects::{ToolResultBatchState, ToolResultEffectProcessor};
use crate::runtime::model::ConcurrentModelRouter;
use crate::runtime::policy::{ReviewerPolicy, SessionEvidence, SessionTerminal};
use crate::runtime::repo::RepoSnapshot;
use crate::runtime::tool_batch::ToolBatchRunner;
use crate::runtime::tools::{ConcurrentArtifactStore, ToolEngine};

pub(crate) struct PlannedReviewRuntime {
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) model_router: Arc<dyn ConcurrentModelRouter>,
    pub(crate) tools: Arc<ToolEngine>,
    pub(crate) policy: Arc<ReviewerPolicy>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) review_revision_id: String,
    pub(crate) events: RuntimeEventDispatcher,
    pub(crate) session_templates: Vec<SessionScope>,
}

impl PlannedReviewRuntime {
    pub(crate) async fn run_with_cancel(
        &self,
        cancel: CancellationToken,
    ) -> PlannedReviewRunReport {
        let started = Instant::now();
        let review_plan = build_review_plan(&self.snapshot);
        let unit_plan = build_review_unit_plan(&review_plan, ReviewUnitOptions::default());
        self.events.emit_legacy(EventRecord::new(
            EventLevel::Info,
            EventType::ToolCallCompleted,
            json!({
                "plannedReview": {
                    "totalFiles": review_plan.counts.total_files,
                    "excludedFiles": review_plan.counts.excluded_files,
                    "fullFiles": review_plan.counts.full_files,
                    "units": unit_plan.counts.total_units,
                }
            }),
        ));
        let mut completed_sessions = 0usize;
        let mut model_calls = 0usize;
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut tool_counts = ToolCounts::default();
        let mut tokens = TokenUsage::default();
        let mut terminal_diagnostics = Vec::new();
        let mut candidate_findings = Vec::new();
        let mut file_reviews = skipped_file_reviews(&review_plan);

        for unit in &unit_plan.units {
            if cancel.is_cancelled() {
                terminal_diagnostics.push(unit_diagnostic(unit, false, "cancelled"));
                continue;
            }
            let report = self
                .run_unit(&review_plan, unit.clone(), cancel.child_token())
                .await;
            if report.completed {
                completed_sessions += 1;
            }
            model_calls += report.model_calls;
            add_model_metrics(&mut model_metrics, &report.model_metrics);
            tool_counts.add(report.tool_counts);
            tokens.add(report.tokens);
            candidate_findings.extend(report.candidate_findings);
            file_reviews.extend(report.file_reviews);
            terminal_diagnostics.push(report.terminal_diagnostic);
        }
        let synthesis = synthesize_findings(
            &review_plan,
            &unit_plan.units,
            self.snapshot.diff.content.as_str(),
            candidate_findings,
            &self.tools.artifacts,
            &self.review_revision_id,
        );
        let findings = synthesis.findings;
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
        reconcile_file_reviews_with_findings(&mut file_reviews, &findings);
        terminal_diagnostics.push(SessionTerminalDiagnostic {
            session_id: "synthesis".to_string(),
            completed: true,
            terminal_tool: Some("structured_synthesis".to_string()),
            terminal_summary: Some(format!(
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
        let review_sessions = terminal_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.session_id != "synthesis")
            .count();
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
            terminal_diagnostics,
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

    async fn run_unit(
        &self,
        review_plan: &ReviewPlan,
        unit: PlannedReviewUnit,
        cancel: CancellationToken,
    ) -> PlannedReviewUnitReport {
        let scope = unit_scope(
            &unit,
            &self.snapshot.snapshot_id,
            self.session_templates.first(),
        );
        self.events
            .emit_planned_runtime(self.policy.plan_session_started_runtime_event(&scope));
        let model = match self.model_router.client_for(&scope).await {
            Ok(model) => model,
            Err(error) => {
                self.events
                    .emit_legacy(self.policy.plan_model_router_error_event(&scope, &error));
                self.events.emit_planned_runtime(
                    self.policy
                        .plan_session_finished_runtime_event(&scope, "failed"),
                );
                return PlannedReviewUnitReport::empty(unit_diagnostic(&unit, false, "failed"));
            }
        };

        let mut transcript = planned_unit_transcript(review_plan, &unit);
        let mut evidence = SessionEvidence::for_scope(&scope);
        let mut terminal = SessionTerminal::default();
        let mut tool_counts = ToolCounts::default();
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut tokens = TokenUsage::default();
        let mut model_calls = 0usize;
        let mut file_evidence = FileEvidenceTracker::new(&unit);

        for turn_index in 0..3 {
            if cancel.is_cancelled() {
                break;
            }
            let turn_id = TurnId(turn_index);
            self.events.emit_planned_runtime(
                self.policy
                    .plan_model_started_runtime_event(&scope, turn_id),
            );
            let model_started = Instant::now();
            let turn = match tokio::time::timeout(
                std::time::Duration::from_millis(self.limits.max_model_turn_ms.max(1)),
                model.complete(&scope, &transcript, turn_id, cancel.child_token()),
            )
            .await
            {
                Ok(Ok(turn)) => turn,
                Ok(Err(error)) => {
                    self.events.emit_planned_runtime(
                        self.policy
                            .plan_model_failed_runtime_event(&scope, turn_id, 1, false, &error),
                    );
                    break;
                }
                Err(_) => {
                    self.events
                        .emit_planned_runtime(self.policy.plan_model_failed_runtime_event(
                            &scope,
                            turn_id,
                            1,
                            false,
                            &RuntimeError::Timeout,
                        ));
                    break;
                }
            };
            model_calls += 1;
            model_metrics.calls += 1;
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
                    let result = parse_unit_result(&content, &unit);
                    let candidate_findings = collect_candidate_findings(
                        &scope,
                        &unit,
                        review_plan,
                        result.findings,
                        &file_evidence,
                    );
                    let file_reviews =
                        validate_file_reviews(&scope, &unit, result.file_verdicts, &file_evidence);
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
                        terminal_diagnostic: SessionTerminalDiagnostic {
                            session_id: scope.id.0,
                            completed: true,
                            terminal_tool: Some("structured_unit_result".to_string()),
                            terminal_summary: Some(result.summary),
                            saw_diff: true,
                            saw_file: true,
                            saw_search: tool_counts.search_text > 0,
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
                    let allowed_calls = calls
                        .into_iter()
                        .filter(|call| !is_terminal_tool(&call.name))
                        .collect::<Vec<_>>();
                    transcript.push(ConversationItem::AssistantToolCalls {
                        calls: allowed_calls.clone(),
                    });
                    let results = ToolBatchRunner::new(
                        self.policy.as_ref(),
                        self.tools.as_ref(),
                        &self.events,
                    )
                    .execute(
                        scope.clone(),
                        turn_id,
                        allowed_calls,
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
                            terminal: &mut terminal,
                            tool_counts: &mut tool_counts,
                            transcript: &mut transcript,
                        },
                    );
                    transcript.push(ConversationItem::User {
                        content: if turn_index == 0 {
                            "Use the gathered evidence to either request one targeted follow-up batch for related searches/ranges/callers, or return the final review unit result as JSON with keys summary, fileVerdicts, and findings. Do not call terminal tools.".to_string()
                        } else {
                            "Return the final review unit result now as JSON with keys summary, fileVerdicts, and findings. Do not call terminal tools.".to_string()
                        },
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
            terminal_diagnostic: unit_diagnostic(&unit, false, "partial"),
        }
    }
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

#[derive(Debug, Default)]
struct FileEvidenceTracker {
    by_path: BTreeMap<String, BTreeSet<String>>,
    global_artifact_ids: BTreeSet<String>,
    unit_paths: Vec<String>,
    diff_content: Option<String>,
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
                .map(|path| (path.clone(), BTreeSet::new()))
                .collect(),
            global_artifact_ids: BTreeSet::new(),
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
                    self.global_artifact_ids.insert(artifact_id.clone());
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
                            .insert(artifact_id.clone());
                    }
                }
                Some(ToolName::SearchText) => {
                    self.global_artifact_ids.insert(artifact_id.clone());
                    for path in &self.unit_paths {
                        self.by_path
                            .entry(path.clone())
                            .or_default()
                            .insert(artifact_id.clone());
                    }
                }
                Some(ToolName::ReadFile | ToolName::ReadFileRange | ToolName::ReadHeadFile) => {
                    if let Some(path) = result
                        .data
                        .as_ref()
                        .and_then(|data| data.get("path"))
                        .and_then(|path| path.as_str())
                    {
                        self.by_path
                            .entry(path.to_string())
                            .or_default()
                            .insert(artifact_id);
                    }
                }
                _ => {}
            }
        }
    }

    fn evidence_for(&self, path: &str) -> Vec<String> {
        let mut evidence = self
            .global_artifact_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Some(path_evidence) = self.by_path.get(path) {
            evidence.extend(path_evidence.iter().cloned());
        }
        evidence.into_iter().collect()
    }

    fn has_path_evidence(&self, path: &str) -> bool {
        !self
            .by_path
            .get(path)
            .map(BTreeSet::is_empty)
            .unwrap_or(true)
            || !self.global_artifact_ids.is_empty()
    }
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
    terminal_diagnostic: SessionTerminalDiagnostic,
}

impl PlannedReviewUnitReport {
    fn empty(terminal_diagnostic: SessionTerminalDiagnostic) -> Self {
        Self {
            completed: false,
            model_calls: 0,
            model_metrics: ModelMetricsSnapshot::default(),
            tool_counts: ToolCounts::default(),
            tokens: TokenUsage::default(),
            candidate_findings: Vec::new(),
            file_reviews: Vec::new(),
            terminal_diagnostic,
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
    title: String,
    claim: String,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Debug, Clone)]
struct CandidateFinding {
    source_unit_id: String,
    source_session_id: String,
    title: String,
    claim: String,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
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

fn planned_unit_transcript(
    review_plan: &ReviewPlan,
    unit: &PlannedReviewUnit,
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
    vec![
        ConversationItem::System {
            content: "You are a focused code-review unit reviewer. Review the assigned changed files, and use exploration tools to inspect directly related context when needed. Return final output as strict JSON with keys summary, fileVerdicts, and findings. fileVerdicts must include only the assigned changed files. findings may include actionable candidate bugs for any changed file if the evidence was encountered during exploration; each finding requires title, claim, path, startLine, and endLine. Do not call record_finding, record_file_review, or finish.\n\nLook for actionable correctness bugs introduced by the change. Prefer concrete evidence over speculation. For each reviewed source file, audit the changed invariants before deciding it is clean: persistent state updates, destructive queries, branching filters, boundary and interval math, equality/value semantics, validation, authorization or scoping assumptions, concurrency assumptions, and contracts with nearby helpers or callers. Report only issues directly supported by the gathered evidence.".to_string(),
        },
        ConversationItem::User {
            content: format!(
                "Review unit: {}\nAssigned changed files:\n{}\nPlanner reasons:\n{}\n\nReturn fileVerdicts only for the assigned changed files above. Return actionable candidate findings for any changed file only when supported by gathered evidence. If no bug is supported, return findings: [] and clean fileVerdicts for the assigned files.",
                unit.id, unit_paths, plan_reasons
            ),
        },
    ]
}

fn unit_scope(
    unit: &PlannedReviewUnit,
    snapshot_id: &SnapshotId,
    template: Option<&SessionScope>,
) -> SessionScope {
    SessionScope {
        id: SessionId(unit.id.clone()),
        role: template.map(|scope| scope.role).unwrap_or(Role::Generalist),
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
        capabilities: template
            .map(|scope| scope.capabilities.clone())
            .unwrap_or_else(CapabilitySet::review_read_only),
        budget: template.map(|scope| scope.budget.clone()).unwrap_or(
            crate::contracts::AgentBudget {
                max_turns: 2,
                max_tool_calls: 8,
                max_prompt_tokens: 64_000,
                max_output_tokens: 8_000,
            },
        ),
    }
}

fn parse_unit_result(content: &str, unit: &PlannedReviewUnit) -> StructuredUnitResult {
    let trimmed = content.trim();
    if let Ok(result) = serde_json::from_str::<StructuredUnitResult>(trimmed) {
        return result;
    }
    let Some(start) = trimmed.find('{') else {
        return clean_result(unit);
    };
    let Some(end) = trimmed.rfind('}') else {
        return clean_result(unit);
    };
    serde_json::from_str(&trimmed[start..=end]).unwrap_or_else(|_| clean_result(unit))
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

fn validate_file_reviews(
    scope: &SessionScope,
    unit: &PlannedReviewUnit,
    candidates: Vec<StructuredFileVerdict>,
    evidence_tracker: &FileEvidenceTracker,
) -> Vec<FileReviewV1> {
    candidates
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
            let summary = candidate.summary.trim();
            let evidence_artifact_ids = evidence_tracker.evidence_for(&path.display());
            Some(FileReviewV1 {
                path: path.display(),
                verdict: verdict.to_string(),
                summary: if summary.is_empty() {
                    format!("Reviewed {} with verdict {verdict}.", path.display())
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
        .collect()
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
            } else if !evidence_tracker.has_path_evidence(&path) {
                rejection_reason = Some("missing_artifact_evidence".to_string());
            }
            CandidateFinding {
                source_unit_id: unit.id.clone(),
                source_session_id: scope.id.0.clone(),
                title: title.to_string(),
                claim: claim.to_string(),
                path: path.clone(),
                start_line: candidate.start_line,
                end_line: candidate.end_line,
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
    let mut by_key: BTreeMap<String, FindingV1> = BTreeMap::new();
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
                by_key
                    .entry(finding_key(&finding))
                    .and_modify(|existing| merge_finding(existing, finding.clone()))
                    .or_insert(finding);
            }
            Err(reason) => {
                rejected_count += 1;
                *rejection_reasons.entry(reason).or_insert(0) += 1;
            }
        }
    }
    SynthesisOutcome {
        findings: by_key.into_values().collect(),
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
    let finding_text = format!("{title} {claim}");
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
    Ok(FindingV1 {
        id: stable_id(&[&candidate.source_session_id, title, claim, &candidate.path]),
        title: title.to_string(),
        claim: claim.to_string(),
        severity: FindingSeverity::Low,
        confidence: 0.72,
        validation_status: ValidationStatus::Validated,
        report_status: ReportStatus::Included,
        publishability: FindingPublishability::Publishable,
        evidence,
        file_refs: vec![location],
        location_line_range: Some(line_range),
        discovered_by: vec![candidate.source_session_id],
        challenged_by: Vec::new(),
    })
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

fn finding_key(finding: &FindingV1) -> String {
    let path = finding
        .file_refs
        .first()
        .map(|location| match location {
            EvidenceLocationV1::SinglePath { path } => path.clone(),
            EvidenceLocationV1::Rename { new_path, .. } => new_path.clone(),
        })
        .unwrap_or_default();
    format!(
        "{}:{}",
        path,
        normalize_finding_text(&format!("{} {}", finding.title, finding.claim))
    )
}

fn normalize_finding_text(value: &str) -> String {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
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
        let Some(EvidenceLocationV1::SinglePath { path }) = finding.file_refs.first() else {
            continue;
        };
        if let Some(review) = file_reviews.iter_mut().find(|review| review.path == *path) {
            review.verdict = "issue_found".to_string();
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

fn is_terminal_tool(tool_id: &ToolId) -> bool {
    matches!(
        tool_id.as_builtin(),
        Some(
            ToolName::RecordFinding
                | ToolName::RecordFileReview
                | ToolName::ChallengeFinding
                | ToolName::Finish
        )
    )
}

fn record_usage(
    tokens: &mut TokenUsage,
    model_metrics: &mut ModelMetricsSnapshot,
    model: &dyn crate::runtime::model::ConcurrentModelClient,
    usage: TokenUsage,
) {
    tokens.add(usage);
    model_metrics.input_tokens += usage.input_tokens;
    model_metrics.output_tokens += usage.output_tokens;
    model_metrics.total_tokens += usage.total_tokens;
    if let Some(cost) = model.estimate_cost(&usage) {
        model_metrics.costed_calls += 1;
        model_metrics.estimated_input_cost_micro_usd += cost.input_cost_micro_usd;
        model_metrics.estimated_output_cost_micro_usd += cost.output_cost_micro_usd;
        model_metrics.estimated_total_cost_micro_usd += cost.total_cost_micro_usd;
    } else {
        model_metrics.unpriced_calls += 1;
    }
}

fn add_model_metrics(target: &mut ModelMetricsSnapshot, report: &ModelMetricsSnapshot) {
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
}

fn unit_diagnostic(
    unit: &PlannedReviewUnit,
    completed: bool,
    status: &str,
) -> SessionTerminalDiagnostic {
    SessionTerminalDiagnostic {
        session_id: unit.id.clone(),
        completed,
        terminal_tool: Some("structured_unit_result".to_string()),
        terminal_summary: Some(status.to_string()),
        saw_diff: completed,
        saw_file: completed,
        saw_search: false,
        model_calls: 0,
        tool_counts: ToolCounts::default(),
    }
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

fn elapsed_ms(started: Instant) -> u64 {
    (started.elapsed().as_micros().div_ceil(1000) as u64).max(1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::contracts::{
        ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
        RenameDetection, SnapshotMode,
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
            if turn_id.0 == 0 {
                let mut calls = vec![ModelToolCall {
                    call_id: ToolCallId(format!("{}-read-diff", scope.id.0)),
                    index: 0,
                    name: ToolId::from(ToolName::ReadDiff),
                    raw_arguments: "{}".to_string(),
                }];
                if matches!(self.mode, TestModelMode::OutOfUnitFinding) && scope.id.0 == "unit-001"
                {
                    calls.push(ModelToolCall {
                        call_id: ToolCallId(format!("{}-read-related", scope.id.0)),
                        index: 1,
                        name: ToolId::from(ToolName::ReadHeadFile),
                        raw_arguments: r#"{"path":"apps/api/credential-token.ts"}"#.to_string(),
                    });
                }
                return Ok(ModelTurn::ToolCalls {
                    calls,
                    usage: TokenUsage::default(),
                });
            }
            let content = match self.mode {
                TestModelMode::AssignedFinding => json!({
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
                        "endLine": 1
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
            };
            Ok(ModelTurn::Text {
                content: content.to_string(),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 12,
                    total_tokens: 22,
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
        assert_eq!(report.metrics.model_calls, 2);
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
            .terminal_diagnostics
            .iter()
            .any(|diagnostic| {
                diagnostic.session_id == "synthesis"
                    && diagnostic
                        .terminal_summary
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
            .terminal_diagnostics
            .iter()
            .any(|diagnostic| {
                diagnostic.session_id == "synthesis"
                    && diagnostic
                        .terminal_summary
                        .as_deref()
                        .unwrap_or_default()
                        .contains("rejectedCandidates=2")
            }));
    }

    async fn run_test_review(
        files: Vec<(&'static str, &'static str)>,
        mode: TestModelMode,
    ) -> PlannedReviewRunReport {
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
        let snapshot =
            RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64 * 1024, 20), &change)
                .expect("snapshot");
        let limits = Arc::new(RuntimeLimits::standard(2, 64 * 1024, 20));
        let tools = Arc::new(ToolEngine::new(Arc::clone(&snapshot), Arc::clone(&limits)).unwrap());
        let runtime = PlannedReviewRuntime {
            snapshot,
            model_router: Arc::new(StaticModelRouter::new(Arc::new(EvidenceBackedModel {
                mode,
            }))),
            tools,
            policy: Arc::new(ReviewerPolicy::new()),
            limits,
            review_revision_id: "head".to_string(),
            events: RuntimeEventDispatcher::new(None, None),
            session_templates: Vec::new(),
        };

        runtime.run_with_cancel(CancellationToken::new()).await
    }
}
