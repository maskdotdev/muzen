use std::sync::Arc;

use crate::runtime::contracts::{
    RuntimeError, RuntimeEvent, RuntimeEventContext, RuntimeEventSink, RuntimeLimits,
    RuntimeResult, SessionScope, SnapshotId,
};
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::model::ConcurrentModelRouter as RuntimeModelRouter;
use crate::runtime::tools::{
    ConcurrentArtifactStore as RuntimeArtifactStore, ToolRegistry as RuntimeToolRegistry,
};

use crate::contracts::{ChangeScopeV1, FileReviewV1, FindingV1, PathPolicyV1};
use crate::events::EventEmitter;
use crate::runtime::agent_sessions::AgentSessionRuntime;
use crate::runtime::contracts::AgentSessionOutput;
use crate::runtime::planned_units::{session_semaphore, PlannedReviewRuntime};
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
    legacy_event_emitter: Option<Arc<EventEmitter>>,
}

impl RunBuilder {
    pub fn new(spec: RunSpec) -> Self {
        Self {
            spec,
            model_router: None,
            tool_registry: None,
            reviewer_policy: None,
            event_sink: None,
            legacy_event_emitter: None,
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

    pub(crate) fn legacy_event_emitter(mut self, emitter: Option<Arc<EventEmitter>>) -> Self {
        self.legacy_event_emitter = emitter;
        self
    }

    pub fn build(self) -> RuntimeResult<Run> {
        let model_router = self
            .model_router
            .ok_or_else(|| RuntimeError::InvalidInput("run requires a model router".to_string()))?;
        let registry = match self.tool_registry {
            Some(registry) => registry,
            None => Arc::new(RuntimeToolRegistry::review_defaults()?),
        };
        let reviewer_policy = self
            .reviewer_policy
            .unwrap_or_else(|| Arc::new(ReviewerPolicy::new()));
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
            event_sink: self.event_sink,
            legacy_event_emitter: self.legacy_event_emitter,
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
    event_sink: Option<Arc<dyn RuntimeEventSink>>,
    legacy_event_emitter: Option<Arc<EventEmitter>>,
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
        let active_sessions = session_semaphore(&self.limits);
        let mode = self.mode;
        let mut joins = tokio::task::JoinSet::new();
        for (index, shard) in self.shards.into_iter().enumerate() {
            let event_sink = self.event_sink.clone();
            let legacy_event_emitter = self.legacy_event_emitter.clone();
            let run_id = self.run_id.clone();
            let model_router = Arc::clone(&self.model_router);
            let reviewer_policy = Arc::clone(&self.reviewer_policy);
            let limits = Arc::clone(&self.limits);
            let active_sessions = Arc::clone(&active_sessions);
            let cancel = cancel.clone();
            joins.spawn(async move {
                let snapshot_id = shard.snapshot_handle.snapshot_id.clone();
                let shard_event_sink = event_sink.map(|sink| {
                    Arc::new(ContextualEventSink::new(
                        sink,
                        run_id,
                        Some(snapshot_id.clone()),
                    )) as Arc<dyn RuntimeEventSink>
                });
                if let Some(sink) = &shard_event_sink {
                    sink.emit(RuntimeEvent::SnapshotStarted {
                        snapshot_id: snapshot_id.clone(),
                    });
                }
                let events =
                    RuntimeEventDispatcher::new(shard_event_sink.clone(), legacy_event_emitter);
                let tools = Arc::clone(&shard.tools);
                let outcome = match mode {
                    RunMode::PlannedReview => {
                        let runtime = Arc::new(PlannedReviewRuntime {
                            snapshot: shard.snapshot,
                            model_router,
                            tools: shard.tools,
                            policy: reviewer_policy,
                            limits,
                            review_revision_id: shard.review_revision_id,
                            session_templates: shard.sessions,
                            events,
                            active_sessions,
                        });
                        let summary = Arc::clone(&runtime).run_with_cancel(cancel).await;
                        ShardOutcome {
                            index,
                            metrics: summary.metrics,
                            findings: summary.findings,
                            file_reviews: summary.file_reviews,
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
                        let report = runtime.run_with_cancel(shard.sessions, cancel).await;
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
        let metrics = merge_run_summaries(summaries);
        if let Some(sink) = &run_event_sink {
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
