use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::review_planning::select_target_path;
use crate::review_planning::{
    changed_file_specs, default_max_active_sessions, default_review_orchestrator_session,
    review_change_spec, session_instruction, ReviewChangeDescriptor, ReviewChangedFileDescriptor,
};
use crate::review_sources::materialize::{materialize_run_source, SourceProviderConfig};
use crate::reviewer_kernel::events::InMemoryReviewEventSink;
use crate::reviewer_kernel::kernel::Run;
use crate::reviewer_kernel::kernel_types::RuntimeLimits;
use crate::reviewer_kernel::snapshots::{SnapshotPathPolicy, SnapshotSpec};
use crate::reviewer_kernel::spec::RunSpec;
#[cfg(not(test))]
use crate::runner_protocol::{RunModelCredentialParams, RunModelProfileParams};
use crate::runner_protocol::{RunModelParams, RunToolParams, RunnerWiring};

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
    run_id: String,
    metadata: BTreeMap<String, Value>,
    spec: RunSpec,
    max_active_sessions: usize,
    model: RunModelParams,
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
    let tools = Vec::<RunToolParams>::new();
    let wiring = RunnerWiring::new(&plan.run_id, &tools, None)
        .map_err(|error| ReviewSessionError::Runner(error.to_string()))?;
    let builder = Run::builder(plan.spec);
    let builder = wiring
        .wire_model(
            builder,
            &plan.run_id,
            &plan.model,
            plan.max_active_sessions,
            None,
            #[cfg(test)]
            plan.target_path,
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
    let materialized = materialize_run_source(
        source
            .local_repo()
            .map(|path| path.to_path_buf())
            .as_deref(),
        Some(source),
        &options.scope.files,
        source_provider.as_ref(),
        None,
    )?;
    let repo_root = materialized.repo_root().to_path_buf();
    #[cfg(test)]
    let target_path = select_target_path(&repo_root, materialized.changed_files())?;
    let change_descriptor = options.change.as_ref().map(review_change_descriptor);
    let changed_files = changed_file_specs(
        &repo_root,
        materialized.changed_files(),
        change_descriptor.as_ref(),
    );
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
    let session = default_review_orchestrator_session(review_instructions(&options.instructions));
    let runtime_limits =
        RuntimeLimits::standard(max_active_sessions, max_file_bytes, max_search_matches);
    let model = review_model(options)
        .ok_or_else(|| anyhow::anyhow!("run requires model; configure a hosted provider model"))?;
    Ok(LocalReviewPlan {
        run_id: run_id.clone(),
        metadata: options.metadata.clone(),
        spec: RunSpec::single_snapshot(run_id, snapshot, vec![session], runtime_limits),
        max_active_sessions,
        model,
        #[cfg(test)]
        target_path,
    })
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

fn review_model(_options: &ReviewOptions) -> Option<RunModelParams> {
    #[cfg(test)]
    {
        Some(RunModelParams {
            callback: false,
            default_model_profile_id: None,
            model_profiles: Vec::new(),
        })
    }
    #[cfg(not(test))]
    {
        hosted_review_model(_options)
    }
}

#[cfg(not(test))]
fn hosted_review_model(options: &ReviewOptions) -> Option<RunModelParams> {
    let snapshot = options.config_snapshot.as_ref()?;
    let profile = snapshot.model_profile.as_ref()?;
    let provider = snapshot.routing.get("model.provider")?.clone();
    let model = snapshot.routing.get("model.name")?.clone();
    Some(RunModelParams {
        callback: false,
        default_model_profile_id: Some(profile.id.clone()),
        model_profiles: vec![RunModelProfileParams {
            id: profile.id.clone(),
            provider,
            model,
            credential: profile.secret_ref.as_deref().map(model_credential_from_ref),
            base_url: snapshot.routing.get("model.baseUrl").cloned(),
            api_protocol: Some("responses".to_string()),
            max_input_tokens: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
        }],
    })
}

#[cfg(not(test))]
fn model_credential_from_ref(secret_ref: &str) -> RunModelCredentialParams {
    if let Some(env) = secret_ref.strip_prefix("env:") {
        return RunModelCredentialParams {
            env: Some(env.to_string()),
            secret_ref: None,
        };
    }
    RunModelCredentialParams {
        env: None,
        secret_ref: Some(secret_ref.to_owned()),
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
