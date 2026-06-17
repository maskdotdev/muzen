use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::review_sources::materialize::materialize_run_source;
use crate::reviewer_kernel::events::InMemoryReviewEventSink;
use crate::reviewer_kernel::kernel::Run;
use crate::reviewer_kernel::kernel_types::{RuntimeLimits, SessionInstruction};
use crate::reviewer_kernel::review_contract::{AgentBudget, Role};
use crate::reviewer_kernel::snapshots::{
    ChangeKind, ChangeSpec, ChangedFileSpec, ChangedFileStatus, RenameDetection, SnapshotMode,
    SnapshotPathPolicy, SnapshotSpec,
};
use crate::reviewer_kernel::spec::{ReviewSessionSpec, RunSpec};
#[cfg(not(test))]
use crate::runner_protocol::{RunModelCredentialParams, RunModelProfileParams};
use crate::runner_protocol::{
    RunModelParams, RunSourceProviderParams, RunToolParams, RunnerWiring,
};

use super::{
    ReviewArtifact, ReviewChangeSpec, ReviewEvent, ReviewInstruction, ReviewOptions, ReviewResult,
    ReviewSessionError, ReviewSessionId, ReviewSource,
};

const LARGE_REVIEW_BATCH_THRESHOLD: usize = 8;
const LARGE_REVIEW_DEFAULT_MAX_ACTIVE_SESSIONS: usize = 8;

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
        source.local_repo().map(Path::to_path_buf).as_deref(),
        Some(source),
        &options.scope.files,
        source_provider.as_ref(),
        None,
    )?;
    let repo_root = materialized.repo_root().to_path_buf();
    #[cfg(test)]
    let target_path = select_target_path(&repo_root, materialized.changed_files())?;
    let changed_files = changed_file_specs(
        &repo_root,
        materialized.changed_files(),
        options.change.as_ref(),
    );
    let change = review_change_spec(
        source,
        options.change.as_ref(),
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
        change.changed_files.len(),
        options
            .limits
            .as_ref()
            .and_then(|limits| limits.max_active_sessions),
    );
    let snapshot = SnapshotSpec::new(&repo_root, change).with_path_policy(
        SnapshotPathPolicy::standard(max_file_bytes, max_search_matches),
    );
    let session = default_review_orchestrator_session(&options.instructions);
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

fn source_provider(options: &ReviewOptions) -> Option<RunSourceProviderParams> {
    options.config_snapshot.as_ref().and_then(|snapshot| {
        snapshot
            .routing
            .get("provider.baseUrl")
            .map(|base_url| RunSourceProviderParams {
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

fn default_max_active_sessions(changed_file_count: usize, explicit: Option<usize>) -> usize {
    if let Some(explicit) = explicit {
        return explicit.max(1);
    }
    if changed_file_count > LARGE_REVIEW_BATCH_THRESHOLD {
        return LARGE_REVIEW_DEFAULT_MAX_ACTIVE_SESSIONS;
    }
    4
}

fn default_review_orchestrator_session(instructions: &[ReviewInstruction]) -> ReviewSessionSpec {
    let spec = ReviewSessionSpec::review_read_only(
        "review-orchestrator",
        Role::Generalist,
        "Autonomously review the changed code.",
        AgentBudget::planned_baseline(),
    );
    let instructions = instructions
        .iter()
        .map(review_instruction)
        .collect::<Vec<_>>();
    if instructions.is_empty() {
        spec
    } else {
        spec.with_instructions(instructions)
    }
}

fn review_instruction(instruction: &ReviewInstruction) -> SessionInstruction {
    SessionInstruction {
        text: instruction.text.clone(),
        trusted: instruction.trusted,
        kind: instruction.kind.clone(),
    }
}

fn changed_file_specs(
    repo_root: &Path,
    changed_files: &[String],
    change: Option<&ReviewChangeSpec>,
) -> Vec<ChangedFileSpec> {
    let status_by_path = change
        .map(|change| {
            change
                .changed_files
                .iter()
                .filter_map(|file| file.status.as_ref().map(|status| (&file.path, status)))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    changed_files
        .iter()
        .filter(|path| repo_root.join(path).is_file())
        .cloned()
        .map(|path| {
            changed_file_spec(
                &path,
                status_by_path.get(&path).map(|status| status.as_str()),
            )
        })
        .collect()
}

fn review_change_spec(
    source: &ReviewSource,
    change: Option<&ReviewChangeSpec>,
    changed_files: Vec<ChangedFileSpec>,
    materialized_inline_diff: Option<&str>,
    run_id: &str,
) -> ChangeSpec {
    let Some(change) = change else {
        let mut spec = ChangeSpec::local("sdk-run", "head", changed_files);
        spec.inline_diff = materialized_inline_diff.map(ToOwned::to_owned);
        return spec;
    };
    ChangeSpec {
        kind: review_change_kind(source),
        change_id: change
            .review_target
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("{}:{run_id}", change.kind)),
        source_ref: change
            .head_revision
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "head".to_string()),
        target_ref: change
            .base_revision
            .clone()
            .or_else(|| change.start_revision.clone())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "base".to_string()),
        base_revision_id: change
            .base_revision
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "base".to_string()),
        head_revision_id: change
            .head_revision
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "head".to_string()),
        merge_base_revision_id: change
            .start_revision
            .clone()
            .filter(|value| !value.trim().is_empty()),
        inline_diff: change
            .diff
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| materialized_inline_diff.map(ToOwned::to_owned)),
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files,
    }
}

fn review_change_kind(source: &ReviewSource) -> ChangeKind {
    match source {
        ReviewSource::GithubPullRequest { .. } => ChangeKind::PullRequest,
        ReviewSource::GitlabMergeRequest { .. } => ChangeKind::MergeRequest,
        _ => ChangeKind::LocalDiff,
    }
}

fn changed_file_spec(path: &str, status: Option<&str>) -> ChangedFileSpec {
    let status = review_changed_file_status(status);
    let path = std::path::PathBuf::from(path);
    ChangedFileSpec {
        old_path: if status == ChangedFileStatus::Added {
            None
        } else {
            Some(path.clone())
        },
        new_path: if status == ChangedFileStatus::Deleted {
            None
        } else {
            Some(path)
        },
        status,
        old_content_hash: None,
        new_content_hash: None,
        is_binary: false,
        is_generated: false,
    }
}

fn review_changed_file_status(status: Option<&str>) -> ChangedFileStatus {
    match status.map(|status| status.to_ascii_lowercase()) {
        Some(status) if matches!(status.as_str(), "added" | "add" | "a") => {
            ChangedFileStatus::Added
        }
        Some(status) if matches!(status.as_str(), "deleted" | "delete" | "removed" | "d") => {
            ChangedFileStatus::Deleted
        }
        Some(status) if matches!(status.as_str(), "renamed" | "rename" | "r") => {
            ChangedFileStatus::Renamed
        }
        Some(status) if matches!(status.as_str(), "copied" | "copy" | "c") => {
            ChangedFileStatus::Copied
        }
        Some(status) if matches!(status.as_str(), "type_changed" | "typechanged" | "t") => {
            ChangedFileStatus::TypeChanged
        }
        _ => ChangedFileStatus::Modified,
    }
}

#[cfg(test)]
fn select_target_path(repo_root: &Path, changed_files: &[String]) -> Result<String> {
    for path in changed_files {
        if repo_root.join(path).is_file() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!("run requires at least one changed file that exists in the materialized worktree")
}
