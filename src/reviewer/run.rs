use std::sync::Arc;

use crate::context_engine::{
    context_tool_effects_for_id, context_tool_ids, register_context_tools, ContextEngine,
    ContextEngineMode, ContextFindingEvidence, ContextIndexRequest, ContextPackPurpose,
    ContextPackRequest, ContextSufficiencyStatus, NoopContextEngine,
};
use crate::runtime::contracts::{
    stable_id, ArtifactKey, RuntimeError, RuntimeEvent, RuntimeEventContext, RuntimeEventSink,
    RuntimeLimits, RuntimeResult, SessionInstruction, SessionScope, SnapshotId, ToolGrant,
};
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::model::ConcurrentModelRouter as RuntimeModelRouter;
use crate::runtime::tools::{
    ConcurrentArtifactStore as RuntimeArtifactStore, ToolRegistry as RuntimeToolRegistry,
};
use crate::util::peak_rss_bytes;

use crate::contracts::{ChangeScopeV1, FileReviewV1, PathPolicyV1};
use crate::contracts::{FindingPublishability, FindingV1, ReportStatus, ValidationStatus};
use crate::runtime::agent_sessions::AgentSessionRuntime;
use crate::runtime::autonomous_review::{
    register_autonomous_delegate_tools, AutonomousDelegateHost, AutonomousReviewRuntime,
};
use crate::runtime::contracts::AgentSessionOutput;
use crate::runtime::policy::ReviewerPolicy;
use crate::runtime::repo::RepoSnapshot;
use crate::runtime::tools::ToolEngine;

use crate::reviewer::adapters::{model_adapters, tool_adapters, Cancellation};
use crate::reviewer::events::*;
use crate::reviewer::model::*;
use crate::reviewer::report::*;
use crate::reviewer::runtime_events;
use crate::reviewer::snapshots::*;
use crate::reviewer::spec::*;
use crate::reviewer::tools::*;
pub struct RunBuilder {
    spec: RunSpec,
    model_router: Option<Arc<dyn RuntimeModelRouter>>,
    tool_registry: Option<Arc<RuntimeToolRegistry>>,
    reviewer_policy: Option<Arc<ReviewerPolicy>>,
    event_sink: Option<Arc<dyn RuntimeEventSink>>,
    context_engine: Option<Arc<dyn ContextEngine>>,
}

impl RunBuilder {
    pub fn new(spec: RunSpec) -> Self {
        Self {
            spec,
            model_router: None,
            tool_registry: None,
            reviewer_policy: None,
            event_sink: None,
            context_engine: None,
        }
    }

    pub fn model_router(mut self, model_router: Arc<dyn model_adapters::ModelRouter>) -> Self {
        self.model_router = Some(model_router);
        self
    }

    pub fn review_model(mut self, model: Arc<dyn ReviewModel>) -> Self {
        self.model_router = Some(review_model_router(model));
        self
    }

    pub fn tool_registry(mut self, tool_registry: tool_adapters::ToolRegistry) -> Self {
        self.tool_registry = Some(Arc::new(tool_registry));
        self
    }

    pub fn review_tool_registry(mut self, tool_registry: ReviewToolRegistry) -> Self {
        self.tool_registry = Some(Arc::new(tool_registry.into_tool_registry()));
        self
    }

    pub fn shared_tool_registry(mut self, tool_registry: Arc<tool_adapters::ToolRegistry>) -> Self {
        self.tool_registry = Some(tool_registry);
        self
    }

    pub fn reviewer_policy(mut self, reviewer_policy: Arc<ReviewerPolicy>) -> Self {
        self.reviewer_policy = Some(reviewer_policy);
        self
    }

    pub fn event_sink(mut self, event_sink: Arc<dyn runtime_events::EventSink>) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    pub fn review_event_sink(mut self, event_sink: Arc<dyn ReviewEventSink>) -> Self {
        self.event_sink = Some(Arc::new(ReviewEventSinkAdapter::new(event_sink)));
        self
    }

    pub fn context_engine(mut self, context_engine: Arc<dyn ContextEngine>) -> Self {
        self.context_engine = Some(context_engine);
        self
    }

    pub fn build(self) -> RuntimeResult<Run> {
        let model_router = self
            .model_router
            .ok_or_else(|| RuntimeError::InvalidInput("run requires a model router".to_string()))?;
        let reviewer_policy = self
            .reviewer_policy
            .unwrap_or_else(|| Arc::new(ReviewerPolicy::new()));
        let context_engine = self
            .context_engine
            .unwrap_or_else(|| Arc::new(NoopContextEngine));
        let context_config = context_engine.config();
        let mut registry = match self.tool_registry {
            Some(registry) => (*registry).clone(),
            None => RuntimeToolRegistry::review_defaults()?,
        };
        if context_config.mode != ContextEngineMode::Disabled {
            register_context_tools(&mut registry, Arc::clone(&context_engine))?;
        }
        let autonomous_delegate_host = AutonomousDelegateHost::default();
        register_autonomous_delegate_tools(&mut registry, autonomous_delegate_host.clone())?;
        let registry = Arc::new(registry);
        let limits = Arc::new(self.spec.limits.into_runtime_limits());
        let mut shards = Vec::new();
        for snapshot_spec in self.spec.snapshots {
            let change: ChangeScopeV1 = snapshot_spec.change.clone().into();
            let path_policy: PathPolicyV1 = snapshot_spec.path_policy.into();
            let mut snapshot = RepoSnapshot::build_with_storage(
                &snapshot_spec.repo_root,
                &path_policy,
                &change,
                snapshot_spec.storage_policy,
            )?;
            if let Some(snapshot_id) = snapshot_spec.snapshot_id {
                Arc::get_mut(&mut snapshot)
                    .ok_or(RuntimeError::Invariant("snapshot unexpectedly shared"))?
                    .snapshot_id = snapshot_id;
            }
            let tools = Arc::new(ToolEngine::with_registry(
                Arc::clone(&snapshot),
                Arc::clone(&limits),
                Arc::clone(&registry),
            )?);
            shards.push(RunShard {
                snapshot_handle: SnapshotHandle {
                    snapshot_id: snapshot.snapshot_id.clone(),
                },
                snapshot,
                tools,
                review_revision_id: change.head_revision_id,
                sessions: Vec::new(),
            });
        }
        if shards.is_empty() {
            return Err(RuntimeError::InvalidInput("missing snapshot".to_string()));
        }
        let default_snapshot_id = shards[0].snapshot_handle.snapshot_id.clone();
        for session in self.spec.sessions {
            let session = session.into_session_scope();
            let target_snapshot_id = session
                .snapshot_id
                .clone()
                .unwrap_or_else(|| default_snapshot_id.clone());
            let Some(shard) = shards
                .iter_mut()
                .find(|shard| shard.snapshot_handle.snapshot_id == target_snapshot_id)
            else {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown session snapshot id {}",
                    target_snapshot_id.0
                )));
            };
            shard.sessions.push(session);
        }
        let snapshot_handles = shards
            .iter()
            .map(|shard| shard.snapshot_handle.clone())
            .collect::<Vec<_>>();
        Ok(Run {
            run_id: self.spec.run_id,
            mode: self.spec.mode,
            snapshot_handles,
            shards,
            limits,
            model_router,
            reviewer_policy,
            context_engine,
            event_sink: self.event_sink,
            autonomous_delegate_host,
        })
    }
}

pub struct Run {
    run_id: String,
    mode: RunMode,
    snapshot_handles: Vec<SnapshotHandle>,
    pub(crate) shards: Vec<RunShard>,
    limits: Arc<RuntimeLimits>,
    model_router: Arc<dyn RuntimeModelRouter>,
    reviewer_policy: Arc<ReviewerPolicy>,
    context_engine: Arc<dyn ContextEngine>,
    event_sink: Option<Arc<dyn RuntimeEventSink>>,
    autonomous_delegate_host: AutonomousDelegateHost,
}

pub(crate) struct RunShard {
    snapshot_handle: SnapshotHandle,
    snapshot: Arc<RepoSnapshot>,
    tools: Arc<ToolEngine>,
    review_revision_id: String,
    pub(crate) sessions: Vec<SessionScope>,
}

struct ShardOutcome {
    index: usize,
    metrics: crate::runtime::contracts::ConcurrentRunReport,
    findings: Vec<FindingV1>,
    file_reviews: Vec<FileReviewV1>,
    session_outputs: Vec<AgentSessionOutput>,
    tools: Arc<ToolEngine>,
}

impl Run {
    pub fn builder(spec: RunSpec) -> RunBuilder {
        RunBuilder::new(spec)
    }

    pub async fn execute(self) -> RunReport {
        self.execute_with_cancel(Cancellation::new()).await
    }

    pub async fn execute_with_cancel(self, cancel: Cancellation) -> RunReport {
        let first_snapshot = self.snapshot_handles[0].clone();
        let run_event_sink = self.event_sink.as_ref().map(|sink| {
            Arc::new(ContextualEventSink::new(
                Arc::clone(sink),
                self.run_id.clone(),
                None,
            )) as Arc<dyn RuntimeEventSink>
        });
        if let Some(sink) = &run_event_sink {
            sink.emit(RuntimeEvent::JobStarted {
                snapshot_id: first_snapshot.snapshot_id.clone(),
            });
            emit_resource_sample(sink.as_ref(), "run_started", None, None, None);
        }
        let aggregate_artifacts = Arc::new(RuntimeArtifactStore::default());
        let snapshot_readers = self
            .shards
            .iter()
            .map(|shard| SnapshotReader::new(Arc::clone(&shard.snapshot)))
            .collect::<Vec<_>>();
        // Shards run concurrently; the shared semaphore keeps the whole run
        // within max_active_sessions. Results are re-ordered by shard index
        // before aggregation so the report stays deterministic.
        let active_sessions = Arc::new(tokio::sync::Semaphore::new(
            self.limits.max_active_sessions.max(1),
        ));
        let mode = self.mode;
        let mut joins = tokio::task::JoinSet::new();
        for (index, shard) in self.shards.into_iter().enumerate() {
            let event_sink = self.event_sink.clone();
            let run_id = self.run_id.clone();
            let model_router = Arc::clone(&self.model_router);
            let reviewer_policy = Arc::clone(&self.reviewer_policy);
            let limits = Arc::clone(&self.limits);
            let active_sessions = Arc::clone(&active_sessions);
            let cancel = cancel.clone();
            let context_engine = Arc::clone(&self.context_engine);
            let aggregate_artifacts = Arc::clone(&aggregate_artifacts);
            let autonomous_delegate_host = self.autonomous_delegate_host.clone();
            joins.spawn(async move {
                let snapshot_id = shard.snapshot_handle.snapshot_id.clone();
                let shard_event_sink = event_sink.map(|sink| {
                    Arc::new(ContextualEventSink::new(
                        sink,
                        run_id.clone(),
                        Some(snapshot_id.clone()),
                    )) as Arc<dyn RuntimeEventSink>
                });
                if let Some(sink) = &shard_event_sink {
                    sink.emit(RuntimeEvent::SnapshotStarted {
                        snapshot_id: snapshot_id.clone(),
                    });
                    emit_resource_sample(sink.as_ref(), "snapshot_started", None, None, None);
                }
                let context_config = context_engine.config();
                let context_enabled = context_config.mode != ContextEngineMode::Disabled;
                if context_enabled {
                    if let Some(sink) = &shard_event_sink {
                        sink.emit(RuntimeEvent::ContextIndexStarted {
                            snapshot_id: snapshot_id.clone(),
                        });
                    }
                    let mut request = ContextIndexRequest::for_snapshot(
                        Arc::clone(&shard.snapshot),
                        &context_config,
                    );
                    request.run_id = Some(run_id.clone());
                    request.instructions = shard
                        .sessions
                        .iter()
                        .flat_map(|session| session.instructions.iter().cloned())
                        .collect();
                    if let Ok(report) = context_engine.index_snapshot(request, cancel.clone()).await
                    {
                        if let Some(index) = context_engine.get_index(&report.snapshot_id) {
                            let _artifact_id = insert_context_manifest_artifact(
                                &aggregate_artifacts,
                                &index.manifest_artifact,
                            );
                        }
                        if let Some(sink) = &shard_event_sink {
                            sink.emit(RuntimeEvent::ContextIndexCompleted {
                                snapshot_id: report.snapshot_id,
                                index_id: report.index_id.0,
                                evidence_count: report.evidence_count,
                                indexed_files: report.indexed_files,
                                skipped_files: report.skipped_files,
                                ms: report.elapsed_ms,
                            });
                        }
                    }
                }
                let mut sessions = shard.sessions;
                if context_enabled {
                    for session in &mut sessions {
                        let purpose = ContextPackPurpose::for_role(session.role);
                        if let Some(sink) = &shard_event_sink {
                            sink.emit(RuntimeEvent::ContextPackStarted {
                                session_id: Some(session.id.clone()),
                                purpose: purpose.as_str().to_string(),
                            });
                        }
                        let started = std::time::Instant::now();
                        if let Ok(pack) = context_engine
                            .build_pack(
                                ContextPackRequest {
                                    run_id: Some(run_id.clone()),
                                    snapshot_id: snapshot_id.clone(),
                                    session_id: Some(session.id.clone()),
                                    purpose,
                                    max_tokens: context_config.max_pack_tokens,
                                },
                                cancel.clone(),
                            )
                            .await
                        {
                            session.instructions.push(SessionInstruction {
                                kind: "context_pack".to_string(),
                                text: context_pack_instruction(&pack),
                                trusted: true,
                            });
                            if let Ok(tool_ids) = context_tool_ids() {
                                for tool_id in tool_ids {
                                    let effects = context_tool_effects_for_id(&tool_id);
                                    session.capabilities.runtime_authority.host_read |=
                                        effects.host_read;
                                    session.capabilities.runtime_authority.network_read |=
                                        effects.network_read;
                                    session.capabilities.runtime_authority.scratch_read |=
                                        effects.scratch_read;
                                    session.capabilities.runtime_authority.scratch_write |=
                                        effects.scratch_write;
                                    session.capabilities.runtime_authority.external_side_effect |=
                                        effects.external_side_effect;
                                    session.capabilities.grant_tool(
                                        tool_id,
                                        ToolGrant {
                                            allow: true,
                                            max_calls: None,
                                            effects_allowed: effects,
                                        },
                                    );
                                }
                            }
                            let _artifact_id =
                                insert_context_pack_artifact(&aggregate_artifacts, &pack);
                            if let Some(sink) = &shard_event_sink {
                                sink.emit(RuntimeEvent::ContextPackCompleted {
                                    pack_id: pack.id.0,
                                    session_id: Some(session.id.clone()),
                                    purpose: pack.purpose.as_str().to_string(),
                                    evidence_count: pack.evidence.len(),
                                    omitted_count: pack.omitted_candidates.len(),
                                    used_tokens: pack.budget.used_tokens,
                                    sufficiency: pack.sufficiency.status.as_str().to_string(),
                                    ms: started
                                        .elapsed()
                                        .as_millis()
                                        .try_into()
                                        .unwrap_or(u64::MAX),
                                });
                            }
                        }
                    }
                }
                let events = RuntimeEventDispatcher::new(shard_event_sink.clone());
                let tools = Arc::clone(&shard.tools);
                let outcome = match mode {
                    RunMode::Review => {
                        let runtime = Arc::new(AutonomousReviewRuntime {
                            snapshot: shard.snapshot,
                            model_router,
                            tools: shard.tools,
                            policy: reviewer_policy,
                            limits,
                            review_revision_id: shard.review_revision_id,
                            events,
                            active_sessions,
                            delegate_host: autonomous_delegate_host,
                        });
                        let report = Arc::clone(&runtime)
                            .run_with_cancel(sessions, cancel.child_token())
                            .await;
                        let mut shard_findings = report.findings;
                        if context_enabled {
                            apply_context_evidence_policy(
                                &mut shard_findings,
                                context_config.strict_evidence_required,
                            );
                        }
                        ShardOutcome {
                            index,
                            metrics: report.metrics,
                            findings: shard_findings,
                            file_reviews: report.file_reviews,
                            session_outputs: Vec::new(),
                            tools,
                        }
                    }
                    RunMode::DirectSessions => {
                        let runtime = Arc::new(AgentSessionRuntime {
                            model_router,
                            tools: shard.tools,
                            policy: reviewer_policy,
                            limits,
                            review_revision_id: shard.review_revision_id,
                            events,
                            active_sessions,
                        });
                        let report = runtime.run_with_cancel(sessions, cancel).await;
                        ShardOutcome {
                            index,
                            metrics: report.metrics,
                            findings: Vec::new(),
                            file_reviews: Vec::new(),
                            session_outputs: report.outputs,
                            tools,
                        }
                    }
                };
                if let Some(sink) = &shard_event_sink {
                    sink.emit(RuntimeEvent::SnapshotFinished {
                        snapshot_id,
                        sessions: outcome.metrics.sessions,
                        completed_sessions: outcome.metrics.completed_sessions,
                    });
                    emit_resource_sample(
                        sink.as_ref(),
                        "snapshot_finished",
                        None,
                        Some(outcome.metrics.sessions),
                        Some(outcome.metrics.completed_sessions),
                    );
                }
                outcome
            });
        }
        let mut shard_outcomes = Vec::new();
        while let Some(result) = joins.join_next().await {
            if let Ok(outcome) = result {
                shard_outcomes.push(outcome);
            }
        }
        shard_outcomes.sort_by_key(|outcome| outcome.index);
        let mut summaries = Vec::new();
        let mut findings = Vec::new();
        let mut file_reviews = Vec::new();
        let mut session_outputs = Vec::new();
        for outcome in shard_outcomes {
            aggregate_artifacts.merge_from(&outcome.tools.artifacts);
            findings.extend(outcome.findings);
            file_reviews.extend(outcome.file_reviews);
            session_outputs.extend(outcome.session_outputs);
            summaries.push(outcome.metrics);
        }
        let mut metrics = merge_run_summaries(summaries);
        if self.context_engine.config().mode != ContextEngineMode::Disabled {
            let _artifact_id = insert_context_findings_evidence_artifact(
                &aggregate_artifacts,
                &self.run_id,
                &findings,
            );
        }
        let (artifact_count, artifact_bytes) = aggregate_artifacts.stats();
        metrics.findings = findings.len();
        metrics.publishable_findings = findings
            .iter()
            .filter(|finding| matches!(finding.publishability, FindingPublishability::Publishable))
            .count();
        metrics.artifacts = artifact_count;
        metrics.artifact_bytes = artifact_bytes;
        if let Some(sink) = &run_event_sink {
            emit_resource_sample(
                sink.as_ref(),
                "run_finished",
                None,
                Some(metrics.sessions),
                Some(metrics.completed_sessions),
            );
            sink.emit(RuntimeEvent::JobFinished {
                status: if metrics.completed_sessions == metrics.sessions {
                    "completed".to_string()
                } else {
                    "partial".to_string()
                },
            });
        }
        RunReport {
            run_id: self.run_id,
            snapshot: first_snapshot,
            snapshots: self.snapshot_handles,
            summary: ReviewRunSummary::from_metrics(&metrics),
            metrics,
            artifacts: aggregate_artifacts,
            session_outputs,
            snapshot_readers,
            findings,
            file_reviews,
        }
    }
}

fn emit_resource_sample(
    sink: &dyn RuntimeEventSink,
    phase: &str,
    session_id: Option<crate::runtime::contracts::SessionId>,
    sessions: Option<usize>,
    completed_sessions: Option<usize>,
) {
    let Some(peak_rss_bytes) = peak_rss_bytes() else {
        return;
    };
    let session_id =
        session_id.unwrap_or_else(|| crate::runtime::contracts::SessionId("run".to_string()));
    sink.emit(RuntimeEvent::AgentTrace {
        session_id,
        turn_id: None,
        trace_kind: "resource_sample".to_string(),
        summary: format!("{phase} peak_rss_bytes={peak_rss_bytes}"),
        details: serde_json::json!({
            "phase": phase,
            "peakRssBytes": peak_rss_bytes,
            "sessions": sessions,
            "completedSessions": completed_sessions,
        }),
    });
}

fn apply_context_evidence_policy(findings: &mut [FindingV1], strict: bool) {
    for finding in findings {
        if !finding.evidence.is_empty() {
            continue;
        }
        if !finding
            .challenged_by
            .iter()
            .any(|source| source == "context_evidence_policy")
        {
            finding
                .challenged_by
                .push("context_evidence_policy".to_string());
        }
        if finding.validation_status == ValidationStatus::Validated {
            finding.validation_status = ValidationStatus::Challenged;
        }
        if strict {
            finding.publishability = FindingPublishability::NotPublishable;
            finding.report_status = ReportStatus::Suppressed;
        }
    }
}

fn insert_context_manifest_artifact(
    artifacts: &RuntimeArtifactStore,
    manifest: &crate::context_engine::ContextManifestArtifact,
) -> RuntimeResult<crate::runtime::contracts::ArtifactId> {
    let content = serde_json::to_string_pretty(manifest).map_err(|error| {
        RuntimeError::InvalidInput(format!("failed to serialize context manifest: {error}"))
    })?;
    Ok(artifacts.insert(
        ArtifactKey(stable_id(&[
            "context_manifest",
            &manifest.snapshot_id.0,
            &manifest.index_id.0,
        ])),
        content,
    ))
}

fn insert_context_pack_artifact(
    artifacts: &RuntimeArtifactStore,
    pack: &crate::context_engine::ContextPack,
) -> RuntimeResult<crate::runtime::contracts::ArtifactId> {
    let content = serde_json::to_string_pretty(pack).map_err(|error| {
        RuntimeError::InvalidInput(format!("failed to serialize context pack: {error}"))
    })?;
    Ok(artifacts.insert(
        ArtifactKey(stable_id(&[
            "context_pack",
            &pack.snapshot_id.0,
            &pack.id.0,
        ])),
        content,
    ))
}

fn insert_context_findings_evidence_artifact(
    artifacts: &RuntimeArtifactStore,
    run_id: &str,
    findings: &[FindingV1],
) -> RuntimeResult<crate::runtime::contracts::ArtifactId> {
    let records = findings
        .iter()
        .map(|finding| ContextFindingEvidence {
            finding_id: finding.id.clone(),
            primary_evidence: finding
                .evidence
                .iter()
                .map(|evidence| crate::runtime::contracts::EvidenceId(evidence.evidence_id.clone()))
                .collect(),
            supporting_evidence: Vec::new(),
            contradicted_by: Vec::new(),
            sufficiency: if finding.evidence.is_empty() {
                ContextSufficiencyStatus::Insufficient
            } else {
                ContextSufficiencyStatus::Sufficient
            },
            artifact_id: None,
        })
        .collect::<Vec<_>>();
    let content = serde_json::to_string_pretty(&serde_json::json!({
        "schemaVersion": "muzen.context_findings_evidence.v1",
        "runId": run_id,
        "findings": records,
    }))
    .map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "failed to serialize context findings evidence: {error}"
        ))
    })?;
    Ok(artifacts.insert(
        ArtifactKey(stable_id(&["context_findings_evidence", run_id])),
        content,
    ))
}

fn context_pack_instruction(pack: &crate::context_engine::ContextPack) -> String {
    let evidence = pack
        .evidence
        .iter()
        .take(12)
        .map(|evidence| {
            let path = evidence
                .path
                .as_ref()
                .map(|path| path.display())
                .unwrap_or_else(|| "<run>".to_string());
            format!(
                "- {} {:?}: {}",
                evidence.id.0,
                evidence.kind,
                evidence.summary.as_deref().unwrap_or(&path)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut instruction = format!(
        "Context pack {} for purpose {} has sufficiency {} and {} selected evidence item(s).\nUse context tools when this pack is insufficient. Selected evidence:\n{}",
        pack.id.0,
        pack.purpose.as_str(),
        pack.sufficiency.status.as_str(),
        pack.evidence.len(),
        evidence
    );
    // R7: skeleton evidence is a synthesized view that exists nowhere on
    // disk, so its text ships inline. It already fit the pack budget by
    // construction.
    let skeletons = pack
        .evidence
        .iter()
        .filter_map(|evidence| {
            let text = evidence.skeleton_text.as_deref()?;
            let path = evidence
                .path
                .as_ref()
                .map(|path| path.display())
                .unwrap_or_else(|| evidence.id.0.clone());
            Some(format!(
                "--- {path} (skeleton: bodies elided, line numbers preserved) ---\n{text}"
            ))
        })
        .collect::<Vec<_>>();
    if !skeletons.is_empty() {
        instruction.push_str(
            "\nSkeleton views included because the full content exceeded the pack budget:\n",
        );
        instruction.push_str(&skeletons.join("\n"));
    }
    let omitted_hints = pack
        .omitted_candidates
        .iter()
        .filter(|candidate| {
            candidate.reason == crate::context_engine::ContextOmissionReason::BudgetExhausted
        })
        .filter_map(|candidate| {
            let path = candidate.path.as_ref()?.display();
            Some(format!(
                "- {path} {:?}, score {:.2}, {} tokens, reason {}; not evidence, query/read before relying on it",
                candidate.kind,
                candidate.score,
                candidate.token_estimate,
                candidate.reason.as_str()
            ))
        })
        .take(12)
        .collect::<Vec<_>>();
    if !omitted_hints.is_empty() {
        instruction
            .push_str("\nHigh-ranked candidates omitted by budget (hints only, not evidence):\n");
        instruction.push_str(&omitted_hints.join("\n"));
    }
    // R6: when coverage gaps exist, hand the session ready-to-run queries
    // so it can fill them before producing findings. Iteration stays
    // bounded by the existing session budgets.
    if !pack.sufficiency.gaps.is_empty() {
        let gaps = pack
            .sufficiency
            .gaps
            .iter()
            .take(12)
            .map(|gap| {
                format!(
                    "- {}:{}-{} missing [{}]; run context query: {}",
                    gap.path,
                    gap.start_line,
                    gap.end_line,
                    gap.missing
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    gap.suggested_query
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        instruction.push_str(&format!(
            "\nCoverage gaps to fill before producing findings (bounded by session budgets):\n{gaps}"
        ));
    }
    instruction
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{FindingSeverity, LineRangeV1};

    #[test]
    fn context_evidence_policy_warns_or_suppresses_missing_evidence() {
        let mut warning_findings = vec![test_finding()];
        apply_context_evidence_policy(&mut warning_findings, false);
        assert_eq!(
            warning_findings[0].validation_status,
            ValidationStatus::Challenged
        );
        assert!(matches!(
            warning_findings[0].publishability,
            FindingPublishability::Publishable
        ));
        assert!(warning_findings[0]
            .challenged_by
            .contains(&"context_evidence_policy".to_string()));

        let mut strict_findings = vec![test_finding()];
        apply_context_evidence_policy(&mut strict_findings, true);
        assert!(matches!(
            strict_findings[0].publishability,
            FindingPublishability::NotPublishable
        ));
        assert!(matches!(
            strict_findings[0].report_status,
            ReportStatus::Suppressed
        ));
    }

    #[test]
    fn pack_instruction_inlines_skeleton_views() {
        use crate::context_engine::{
            ContextBudgetUsage, ContextEvidence, ContextEvidenceKind,
            ContextEvidenceRepresentation, ContextEvidenceSource, ContextOmissionReason,
            ContextPack, ContextPackId, ContextProvenance, ContextRankSignals, ContextScope,
            ContextSensitivity, ContextSufficiency, ContextTrust, OmittedContextCandidate,
        };
        let skeleton_text = "    1| pub fn generated_0() {\n     | ...\n   20| }";
        let pack = ContextPack {
            id: ContextPackId("pack_1".to_string()),
            run_id: None,
            snapshot_id: crate::runtime::contracts::SnapshotId("snap_1".to_string()),
            session_id: None,
            purpose: ContextPackPurpose::Correctness,
            evidence: vec![ContextEvidence {
                id: crate::runtime::contracts::EvidenceId("ev_skel".to_string()),
                kind: ContextEvidenceKind::FileSpan,
                source: ContextEvidenceSource::Snapshot,
                trust: ContextTrust::Kernel,
                sensitivity: ContextSensitivity::Private,
                scope: ContextScope::Snapshot,
                path: Some(crate::runtime::contracts::RepoPath::parse("src/big.rs").unwrap()),
                revision: None,
                range: None,
                content_hash: None,
                summary: Some("skeleton of src/big.rs".to_string()),
                is_changed_span: false,
                representation: ContextEvidenceRepresentation::Skeleton,
                skeleton_text: Some(skeleton_text.to_string()),
                signals: ContextRankSignals::default(),
                token_estimate: 12,
                provenance: ContextProvenance {
                    provider: "snapshot_skeleton_v1".to_string(),
                    query: None,
                    tool_call_id: None,
                    snapshot_id: None,
                    original_url: None,
                },
                created_at_utc: None,
                expires_at_utc: None,
            }],
            selected_candidates: Vec::new(),
            relationships: Vec::new(),
            omitted_candidates: vec![OmittedContextCandidate {
                evidence_id: crate::runtime::contracts::EvidenceId("ev_omitted".to_string()),
                kind: ContextEvidenceKind::FileSpan,
                path: Some(crate::runtime::contracts::RepoPath::parse("src/omitted.rs").unwrap()),
                signals: ContextRankSignals::default(),
                score: 0.42,
                rank_index: 9,
                token_estimate: 320,
                reason: ContextOmissionReason::BudgetExhausted,
                budget_state: None,
                graph_paths: Vec::new(),
                graph_paths_truncated: 0,
            }],
            budget: ContextBudgetUsage {
                max_tokens: 100,
                used_tokens: 12,
            },
            sufficiency: ContextSufficiency::probably_sufficient(),
            compiler_version: "test".to_string(),
            created_at_utc: "0".to_string(),
        };
        let instruction = context_pack_instruction(&pack);
        assert!(instruction.contains("Skeleton views included"));
        assert!(instruction.contains("src/big.rs (skeleton: bodies elided"));
        assert!(instruction.contains(skeleton_text));
        assert!(instruction.contains("High-ranked candidates omitted by budget"));
        assert!(instruction.contains("src/omitted.rs FileSpan, score 0.42"));
        assert!(instruction.contains("not evidence, query/read before relying on it"));
    }

    fn test_finding() -> FindingV1 {
        FindingV1 {
            id: "finding".to_string(),
            title: "title".to_string(),
            claim: "claim".to_string(),
            severity: FindingSeverity::Medium,
            confidence: 0.8,
            validation_status: ValidationStatus::Validated,
            report_status: ReportStatus::Included,
            publishability: FindingPublishability::Publishable,
            challenge_status: crate::contracts::ChallengeStatus::NotRun,
            evidence: Vec::new(),
            file_refs: Vec::new(),
            location_line_range: Some(LineRangeV1 {
                start_line: 1,
                end_line: 1,
            }),
            discovered_by: vec!["test".to_string()],
            challenged_by: Vec::new(),
        }
    }
}

struct ContextualEventSink {
    inner: Arc<dyn RuntimeEventSink>,
    run_id: String,
    snapshot_id: Option<SnapshotId>,
}

impl ContextualEventSink {
    fn new(
        inner: Arc<dyn RuntimeEventSink>,
        run_id: String,
        snapshot_id: Option<SnapshotId>,
    ) -> Self {
        Self {
            inner,
            run_id,
            snapshot_id,
        }
    }

    fn context_for(&self, event: &RuntimeEvent) -> RuntimeEventContext {
        let mut context = RuntimeEventContext::from_event(event).with_run_id(self.run_id.clone());
        if let Some(snapshot_id) = &self.snapshot_id {
            context = context.with_default_snapshot_id(snapshot_id.clone());
        }
        context
    }
}

impl RuntimeEventSink for ContextualEventSink {
    fn emit(&self, event: RuntimeEvent) {
        let context = self.context_for(&event);
        self.inner.emit_with_context(context, event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let mut merged = self.context_for(&event);
        if context.session_id.is_some() {
            merged.session_id = context.session_id;
        }
        if context.turn_id.is_some() {
            merged.turn_id = context.turn_id;
        }
        if context.tool_call_id.is_some() {
            merged.tool_call_id = context.tool_call_id;
        }
        if context.artifact_id.is_some() {
            merged.artifact_id = context.artifact_id;
        }
        if context.finding_id.is_some() {
            merged.finding_id = context.finding_id;
        }
        if context.snapshot_id.is_some() {
            merged.snapshot_id = context.snapshot_id;
        }
        self.inner.emit_with_context(merged, event);
    }
}
