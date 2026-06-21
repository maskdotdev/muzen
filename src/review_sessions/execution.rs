use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::review_planning::select_target_path;
use crate::review_planning::{
    changed_file_paths, changed_file_specs, default_max_active_sessions,
    default_review_orchestrator_session, review_change_spec, session_instruction,
    ReviewChangeDescriptor, ReviewChangedFileDescriptor,
};
use crate::review_sources::materialize::{materialize_run_source, SourceProviderConfig};
use crate::reviewer_kernel::events::InMemoryReviewEventSink;
use crate::reviewer_kernel::kernel::{Run, RunBuilder};
#[cfg(test)]
use crate::reviewer_kernel::kernel_types::RuntimeLimits;
#[cfg(not(test))]
use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeLimits, RuntimeResult};
#[cfg(not(test))]
use crate::reviewer_kernel::model::{EnvCredentialResolver, ModelLimiter, ProfileModelRouter};
use crate::reviewer_kernel::policy::ReviewerPolicy;
use crate::reviewer_kernel::review_contract::AgentBudget;
#[cfg(not(test))]
use crate::reviewer_kernel::review_contract::{ModelApiProtocol, ModelProfileRefV1, ProviderKind};
use crate::reviewer_kernel::snapshots::{SnapshotPathPolicy, SnapshotSpec};
use crate::reviewer_kernel::spec::RunSpec;
#[cfg(test)]
use crate::reviewer_kernel::test_model::DeterministicReviewModel;
use crate::reviewer_kernel::tool_engine::ToolRegistry;

use super::{
    ReviewArtifact, ReviewChangeSpec, ReviewEvent, ReviewInstruction, ReviewOptions, ReviewResult,
    ReviewSessionError, ReviewSessionId, ReviewSource,
};

pub(super) struct LocalReviewExecution {
    pub(super) result: ReviewResult,
    pub(super) events: Vec<ReviewEvent>,
    pub(super) redacted_artifacts: Vec<ReviewArtifact>,
    pub(super) raw_artifacts: Vec<ReviewArtifact>,
}

struct LocalReviewPlan {
    metadata: BTreeMap<String, Value>,
    spec: RunSpec,
    #[cfg(not(test))]
    max_active_sessions: usize,
    #[cfg(not(test))]
    model_profile: ModelProfileRefV1,
    #[cfg(test)]
    target_path: String,
}

pub(super) fn execute_local_review(
    review_id: &ReviewSessionId,
    source: &ReviewSource,
    options: &ReviewOptions,
) -> Result<LocalReviewExecution, ReviewSessionError> {
    let plan = plan_local_review(review_id, source, options)
        .map_err(|error| ReviewSessionError::Runner(error.to_string()))?;
    let event_sink = Arc::new(InMemoryReviewEventSink::default());
    #[cfg(not(test))]
    let max_active_sessions = plan.max_active_sessions;
    #[cfg(test)]
    let target_path = plan.target_path.clone();
    #[cfg(not(test))]
    let model_profile = plan.model_profile.clone();
    let builder = Run::builder(plan.spec);
    let builder = wire_local_review_runtime(
        builder,
        #[cfg(not(test))]
        max_active_sessions,
        #[cfg(test)]
        target_path,
        #[cfg(not(test))]
        model_profile,
    )
    .map_err(|error| ReviewSessionError::Runner(error.to_string()))?
    .review_event_sink(event_sink.clone());
    let run = builder
        .build()
        .map_err(|error| ReviewSessionError::Runner(error.to_string()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ReviewSessionError::Runner(error.to_string()))?;
    let report = runtime.block_on(run.execute_with_cancel(CancellationToken::new()));
    Ok(LocalReviewExecution {
        result: ReviewResult::from_run_report(review_id.clone(), source, &report, plan.metadata),
        events: event_sink
            .records()
            .into_iter()
            .map(ReviewEvent::from_internal_record)
            .collect(),
        redacted_artifacts: report
            .artifacts
            .list()
            .iter()
            .map(ReviewArtifact::from_artifact_view)
            .collect(),
        raw_artifacts: report
            .artifacts
            .list_raw()
            .iter()
            .map(ReviewArtifact::from_artifact_view)
            .collect(),
    })
}

fn plan_local_review(
    review_id: &ReviewSessionId,
    source: &ReviewSource,
    options: &ReviewOptions,
) -> Result<LocalReviewPlan> {
    let run_id = review_id.as_str().to_string();
    let source_provider = source_provider(options);
    let change_descriptor = options.change.as_ref().map(review_change_descriptor);
    let requested_changed_files = changed_file_paths(change_descriptor.as_ref());
    let materialized = materialize_run_source(
        source
            .local_repo()
            .map(|path| path.to_path_buf())
            .as_deref(),
        Some(source),
        &requested_changed_files,
        source_provider.as_ref(),
        None,
    )?;
    let repo_root = materialized.repo_root().to_path_buf();
    #[cfg(test)]
    let target_path = select_target_path(&repo_root, materialized.changed_files())?;
    let changed_files =
        changed_file_specs(materialized.changed_files(), change_descriptor.as_ref());
    let change = review_change_spec(
        Some(source),
        change_descriptor.as_ref(),
        changed_files,
        materialized.inline_diff(),
        &run_id,
    );
    let max_file_bytes = options
        .limits
        .as_ref()
        .and_then(|limits| limits.max_file_bytes)
        .unwrap_or(200 * 1024);
    let max_search_matches = options
        .limits
        .as_ref()
        .and_then(|limits| limits.max_search_matches)
        .unwrap_or(120);
    let max_active_sessions = default_max_active_sessions(
        0,
        change.changed_files.len(),
        options
            .limits
            .as_ref()
            .and_then(|limits| limits.max_active_sessions),
    );
    let snapshot = SnapshotSpec::new(&repo_root, change).with_path_policy(
        SnapshotPathPolicy::standard(max_file_bytes, max_search_matches),
    );
    let session = default_review_orchestrator_session(
        review_instructions(&options.instructions),
        AgentBudget::planned_baseline(),
    );
    let runtime_limits =
        RuntimeLimits::standard(max_active_sessions, max_file_bytes, max_search_matches);
    #[cfg(not(test))]
    let model_profile = review_model_profile(options)
        .ok_or_else(|| anyhow::anyhow!("run requires model; configure a hosted provider model"))?;
    Ok(LocalReviewPlan {
        metadata: options.metadata.clone(),
        spec: RunSpec::single_snapshot(run_id, snapshot, vec![session], runtime_limits),
        #[cfg(not(test))]
        max_active_sessions,
        #[cfg(not(test))]
        model_profile,
        #[cfg(test)]
        target_path,
    })
}

fn wire_local_review_runtime(
    builder: RunBuilder,
    #[cfg(not(test))] max_active_sessions: usize,
    #[cfg(test)] target_path: String,
    #[cfg(not(test))] model_profile: ModelProfileRefV1,
) -> Result<RunBuilder> {
    let tool_registry = Arc::new(
        ToolRegistry::review_defaults()
            .map_err(|error| anyhow::anyhow!("failed to create review tool registry: {error}"))?,
    );
    let reviewer_policy = Arc::new(ReviewerPolicy::new());
    #[cfg(test)]
    let builder = builder.model_client(Arc::new(DeterministicReviewModel::new(
        target_path,
        "TODO|fn|class|export|pub".to_string(),
    )));
    #[cfg(not(test))]
    let builder = builder.model_router(Arc::new(hosted_review_model_router(
        &model_profile,
        max_active_sessions,
        Arc::clone(&tool_registry),
        Arc::clone(&reviewer_policy),
    )?));
    Ok(builder
        .shared_tool_registry(tool_registry)
        .reviewer_policy(reviewer_policy))
}

fn source_provider(options: &ReviewOptions) -> Option<SourceProviderConfig> {
    options.config_snapshot.as_ref().and_then(|snapshot| {
        snapshot
            .routing
            .get("provider.baseUrl")
            .map(|base_url| SourceProviderConfig {
                base_url: Some(base_url.clone()),
                callback: false,
            })
    })
}

#[cfg(not(test))]
fn review_model_profile(options: &ReviewOptions) -> Option<ModelProfileRefV1> {
    let snapshot = options.config_snapshot.as_ref()?;
    let profile = snapshot.model_profile.as_ref()?;
    let provider = snapshot.routing.get("model.provider")?;
    let provider_kind = review_provider_kind(provider)?;
    let api_protocol = match provider_kind {
        ProviderKind::OpenaiCompatible => ModelApiProtocol::Responses,
        ProviderKind::Anthropic => ModelApiProtocol::Messages,
    };
    Some(ModelProfileRefV1 {
        id: profile.id.clone(),
        provider_kind,
        api_protocol,
        provider_profile_id: provider.clone(),
        credential_ref: model_credential_ref(provider_kind, profile.secret_ref.as_deref()),
        model: snapshot.routing.get("model.name")?.clone(),
        base_url: snapshot.routing.get("model.baseUrl").cloned(),
        max_input_tokens: 128_000,
        max_output_tokens: 8_000,
        temperature: None,
        top_p: None,
    })
}

#[cfg(not(test))]
fn hosted_review_model_router(
    profile: &ModelProfileRefV1,
    max_active_sessions: usize,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
) -> RuntimeResult<ProfileModelRouter> {
    ProfileModelRouter::from_profiles(
        std::slice::from_ref(profile),
        profile.id.clone(),
        std::env::var("OPENAI_BASE_URL")
            .ok()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        Arc::new(ModelLimiter::new_with_per_key(
            max_active_sessions.max(1),
            max_active_sessions.max(1),
        )),
        tool_registry,
        reviewer_policy,
        Arc::new(ReviewSessionCredentialResolver),
    )
}

#[cfg(not(test))]
fn review_provider_kind(provider: &str) -> Option<ProviderKind> {
    match provider {
        "openai" | "openai_compatible" => Some(ProviderKind::OpenaiCompatible),
        "anthropic" => Some(ProviderKind::Anthropic),
        _ => None,
    }
}

#[cfg(not(test))]
fn model_credential_ref(provider_kind: ProviderKind, secret_ref: Option<&str>) -> String {
    if let Some(secret_ref) = secret_ref.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(env) = secret_ref.strip_prefix("env:") {
            return format!("env:{}", env.trim());
        }
        return format!("secret:{secret_ref}");
    }
    match provider_kind {
        ProviderKind::OpenaiCompatible => "env:OPENAI_API_KEY".to_string(),
        ProviderKind::Anthropic => "env:ANTHROPIC_API_KEY".to_string(),
    }
}

#[cfg(not(test))]
struct ReviewSessionCredentialResolver;

#[cfg(not(test))]
impl crate::reviewer_kernel::model::CredentialResolver for ReviewSessionCredentialResolver {
    fn resolve_credential(&self, credential_ref: &str) -> RuntimeResult<String> {
        if credential_ref.starts_with("secret:") {
            return Err(RuntimeError::InvalidInput(
                "model credential secretRef requires a configured secret resolver".to_string(),
            ));
        }
        EnvCredentialResolver.resolve_credential(credential_ref)
    }
}

fn review_change_descriptor(change: &ReviewChangeSpec) -> ReviewChangeDescriptor<'_> {
    ReviewChangeDescriptor {
        kind: &change.kind,
        base_revision: change.base_revision.as_deref(),
        start_revision: change.start_revision.as_deref(),
        head_revision: change.head_revision.as_deref(),
        changed_files: change
            .changed_files
            .iter()
            .map(|file| ReviewChangedFileDescriptor {
                path: &file.path,
                status: file.status.as_deref(),
            })
            .collect(),
        diff: change.diff.as_deref(),
        review_target: change.review_target.as_deref(),
    }
}

fn review_instructions(
    instructions: &[ReviewInstruction],
) -> Vec<crate::reviewer_kernel::kernel_types::SessionInstruction> {
    instructions.iter().map(review_instruction).collect()
}

fn review_instruction(
    instruction: &ReviewInstruction,
) -> crate::reviewer_kernel::kernel_types::SessionInstruction {
    session_instruction(
        instruction.kind.clone(),
        instruction.text.clone(),
        instruction.trusted,
    )
}
