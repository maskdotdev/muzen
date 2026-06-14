use std::sync::Arc;

use crate::runtime::contracts::{
    CapabilitySet, ConcurrentRunReport, FsScope, RuntimeEventSink, RuntimeLimits,
    SnapshotStoragePolicy, ToolGrant, ToolId,
};
use crate::runtime::model::{EnvCredentialResolver, ModelLimiter, ProfileModelRouter};
use crate::runtime::tools::ToolRegistry as RuntimeToolRegistry;

use crate::contracts::{ReviewRunJobV1, ToolMask, ToolName};
use crate::job::{effective_personas, tool_allowed, validate_job};
use crate::runtime::policy::ReviewerPolicy;

use crate::reviewer::run::*;
use crate::reviewer::snapshots::*;
use crate::reviewer::spec::*;
pub(crate) fn run_review_job(job: ReviewRunJobV1) -> anyhow::Result<ConcurrentRunReport> {
    run_review_job_with_event_sink(job, None)
}

pub(crate) fn run_review_job_with_event_sink(
    job: ReviewRunJobV1,
    event_sink: Option<Arc<dyn RuntimeEventSink>>,
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
    let base_url = std::env::var("OPENAI_BASE_URL")
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
    let mut builder = Run::builder(run_spec_from_job(&job, limits))
        .model_router(Arc::new(router))
        .shared_tool_registry(registry);
    if let Some(event_sink) = event_sink {
        builder = builder.event_sink(event_sink);
    }
    let run = builder
        .build()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get().clamp(2, 8))
        .enable_all()
        .build()
        .map_err(|error| anyhow::anyhow!("failed to build tokio runtime: {error}"))?;
    let run_report = tokio_runtime.block_on(run.execute());
    let report = run_report.metrics.clone();
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
