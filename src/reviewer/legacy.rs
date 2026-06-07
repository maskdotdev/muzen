use std::sync::Arc;

use crate::contracts::TokenUsage;
use crate::runtime::contracts::{
    CapabilitySet, ConcurrentRunReport, FsScope, RuntimeLimits, SnapshotStoragePolicy, ToolGrant,
    ToolId,
};
use crate::runtime::model::{EnvCredentialResolver, ModelLimiter, ProfileModelRouter};
use crate::runtime::tools::ToolRegistry as RuntimeToolRegistry;

use crate::contracts::{
    EventLevel, EventType, Publishability, ReviewRunJobV1, ReviewRunResultV1, ReviewRuntimeV1,
    ToolMask, ToolName,
};
use crate::events::{EventEmitter, EventRecord};
use crate::job::{effective_personas, tool_allowed, validate_job};
use crate::runtime::policy::ReviewerPolicy;
use crate::util::SCHEMA_VERSION;

use crate::reviewer::report::*;
use crate::reviewer::run::*;
use crate::reviewer::snapshots::*;
use crate::reviewer::spec::*;
pub(crate) fn run_review_job_with_events(
    job: ReviewRunJobV1,
    emitter: Option<Arc<EventEmitter>>,
) -> anyhow::Result<ConcurrentRunReport> {
    validate_job(&job)?;
    let registry = Arc::new(
        RuntimeToolRegistry::review_defaults()
            .map_err(|error| anyhow::anyhow!("failed to build tool registry: {error}"))?,
    );
    let mut limits = RuntimeLimits::standard(
        job.budgets.max_active_sessions.max(1),
        job.path_policy.max_file_bytes,
        job.path_policy.max_search_results,
    );
    limits.max_tool_calls_per_turn = 4;
    limits.max_model_concurrency_global = job.budgets.max_active_sessions.max(1);
    let base_url = std::env::var("OAI_BASE_URL")
        .or_else(|_| std::env::var("OPENAI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let router = ProfileModelRouter::from_profiles(
        &job.model_profiles,
        job.default_model_profile_id.clone(),
        base_url,
        Arc::new(ModelLimiter::new_with_per_key(
            limits.max_model_concurrency_global,
            limits.max_model_concurrency_per_key,
        )),
        Arc::clone(&registry),
        Arc::new(ReviewerPolicy::new()),
        Arc::new(EnvCredentialResolver),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let run = Run::builder(run_spec_from_job(&job, limits))
        .model_router(Arc::new(router))
        .shared_tool_registry(registry)
        .legacy_event_emitter(emitter.clone())
        .build()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let session_count = run
        .shards
        .iter()
        .map(|shard| shard.sessions.len())
        .sum::<usize>();
    if let Some(emitter) = &emitter {
        emitter.emit(EventRecord::new(
            EventLevel::Info,
            EventType::RunStarted,
            serde_json::json!({
            "projectId": job.project_id,
            "sessions": session_count,
            "runtime": "concurrent"
            }),
        ));
    }
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get().clamp(2, 8))
        .enable_all()
        .build()
        .map_err(|error| anyhow::anyhow!("failed to build tokio runtime: {error}"))?;
    let run_report = tokio_runtime.block_on(run.execute());
    let report = run_report.metrics.clone();
    if let Some(emitter) = &emitter {
        emitter.emit(EventRecord::new(
            EventLevel::Info,
            EventType::RunFinished,
            serde_json::json!(ReviewRunResultV1 {
                schema_version: SCHEMA_VERSION,
                run_id: job.run_id,
                attempt: job.attempt,
                runtime: ReviewRuntimeV1::Concurrent,
                outcome: concurrent_review_outcome(&report, run_report.findings.len()),
                publishability: if report.completed_sessions == report.sessions {
                    Publishability::Publishable
                } else {
                    Publishability::DiagnosticOnly
                },
                sessions: report.sessions,
                completed_sessions: report.completed_sessions,
                file_reviews: run_report.file_reviews.clone(),
                findings: run_report.findings,
                tool_counts: report.tool_counts,
                model_calls: report.model_calls,
                tokens: TokenUsage {
                    input_tokens: report.input_tokens,
                    output_tokens: report.output_tokens,
                    total_tokens: report.total_tokens,
                },
                artifact_stats: crate::contracts::ArtifactStats {
                    artifacts: report.artifacts,
                    artifact_bytes: report.artifact_bytes,
                    content_refs: report.artifacts,
                },
                elapsed_ms: report.elapsed_ms,
            }),
        ));
    }
    Ok(report)
}

pub(crate) fn run_spec_from_job(job: &ReviewRunJobV1, limits: RuntimeLimits) -> RunSpec {
    RunSpec::single_snapshot(
        job.run_id.clone(),
        SnapshotSpec {
            snapshot_id: None,
            repo_root: job.repo.worktree_root.clone(),
            default_cwd: Some(job.repo.default_cwd.clone()),
            change: job.change.clone().into(),
            path_policy: job.path_policy.clone().into(),
            storage_policy: SnapshotStoragePolicy::default(),
        },
        effective_personas(job)
            .into_iter()
            .map(|persona| {
                ReviewSessionSpec::review_read_only(
                    persona.id,
                    persona.role,
                    persona.objective,
                    persona.budget,
                )
                .with_model_profile_id(
                    persona
                        .model_profile_id
                        .unwrap_or_else(|| job.default_model_profile_id.clone()),
                )
                .with_capabilities(capabilities_from_mask(persona.allowed_tools))
            })
            .collect(),
        limits,
    )
}

pub(crate) fn capabilities_from_mask(mask: ToolMask) -> CapabilitySet {
    let mut capabilities = CapabilitySet::empty_review_policy(FsScope::repo_root());
    for &tool in ToolName::review_read_only_tools() {
        if tool_allowed(mask, tool) {
            capabilities.grant(ToolId::from(tool), ToolGrant::allow_review_read_only());
        }
    }
    capabilities
}
