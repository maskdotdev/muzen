use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::contracts::{AgentBudget, Role};
use crate::review_session::ReviewSource;
use crate::reviewer::adapters::{capabilities, ids::ToolId};
use crate::reviewer::snapshots::{
    ChangeKind, ChangeSpec, ChangedFileSpec, ChangedFileStatus, RenameDetection, SnapshotMode,
    SnapshotPathPolicy, SnapshotSpec,
};
use crate::reviewer::spec::{ReviewRunLimits, ReviewSessionSpec, RunMode, RunSpec};
use crate::runtime::contracts::{
    FsScope, ProviderResourceId, RepoPath, SessionInstruction, ToolEffects,
};

use super::materialize::materialize_run_source;
use super::transport::RunnerCallbackTransport;
use super::types::{
    RunChangeParams, RunInstructionParams, RunSessionParams, RunStartParams, RunToolParams,
};
use std::sync::Arc;

const LARGE_REVIEW_BATCH_THRESHOLD: usize = 8;
const LARGE_REVIEW_DEFAULT_MAX_ACTIVE_SESSIONS: usize = 8;

pub(crate) struct RunnerPlan {
    pub(crate) run_id: String,
    pub(crate) metadata: BTreeMap<String, Value>,
    pub(crate) spec: RunSpec,
    pub(crate) max_active_sessions: usize,
    #[cfg(test)]
    pub(crate) target_path: String,
}

pub(crate) fn plan_run_start(
    mut params: RunStartParams,
    transport: Option<&Arc<dyn RunnerCallbackTransport>>,
) -> Result<RunnerPlan> {
    let run_id = params
        .run_id
        .take()
        .unwrap_or_else(|| "muzen-run".to_string());
    let metadata = params.metadata.clone();
    let requested_changed_files =
        runner_changed_files(&params.changed_files, params.change.as_ref());
    let materialized = materialize_run_source(
        params.repo.as_deref(),
        params.source.as_ref(),
        &requested_changed_files,
        params.source_provider.as_ref(),
        transport,
    )?;
    let repo_root = materialized.repo_root().to_path_buf();
    #[cfg(test)]
    let target_path = select_target_path(&repo_root, materialized.changed_files())?;
    let changed_files = changed_file_specs(
        &repo_root,
        materialized.changed_files(),
        params.change.as_ref(),
    );
    let change = runner_change_spec(
        params.source.as_ref(),
        params.change.as_ref(),
        changed_files,
        materialized.inline_diff(),
        &run_id,
    );
    let max_file_bytes = params
        .limits
        .as_ref()
        .and_then(|limits| limits.max_file_bytes)
        .unwrap_or(200 * 1024);
    let max_search_matches = params
        .limits
        .as_ref()
        .and_then(|limits| limits.max_search_matches)
        .unwrap_or(120);
    let max_active_sessions = default_max_active_sessions(
        params.sessions.len(),
        change.changed_files.len(),
        params
            .limits
            .as_ref()
            .and_then(|limits| limits.max_active_sessions),
    );
    let mode = parse_run_mode(params.mode.as_deref())?;
    let sessions = if params.sessions.is_empty() {
        if mode == RunMode::DirectSessions {
            anyhow::bail!("direct_sessions mode requires at least one session");
        }
        default_planned_review_lens_panel()
    } else {
        params.sessions
    };
    let snapshot = SnapshotSpec::new(&repo_root, change).with_path_policy(
        SnapshotPathPolicy::standard(max_file_bytes, max_search_matches),
    );
    let callback_tools = params
        .tools
        .iter()
        .map(CallbackToolGrant::from_tool_params)
        .collect::<Result<Vec<_>>>()?;
    let session_specs = sessions
        .into_iter()
        .map(|session| run_session_spec(session, &callback_tools, &params.instructions))
        .collect::<Result<Vec<_>>>()?;
    let limits = ReviewRunLimits::standard(max_active_sessions, max_file_bytes, max_search_matches);
    Ok(RunnerPlan {
        run_id: run_id.clone(),
        metadata,
        spec: RunSpec::single_snapshot(run_id, snapshot, session_specs, limits).with_mode(mode),
        max_active_sessions,
        #[cfg(test)]
        target_path,
    })
}

fn parse_run_mode(mode: Option<&str>) -> Result<RunMode> {
    match mode.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(RunMode::default()),
        Some("planned_review") => Ok(RunMode::PlannedReview),
        Some("direct_sessions") => Ok(RunMode::DirectSessions),
        Some(unknown) => anyhow::bail!("unsupported run mode {unknown}"),
    }
}

fn default_max_active_sessions(
    requested_session_count: usize,
    changed_file_count: usize,
    explicit: Option<usize>,
) -> usize {
    if let Some(explicit) = explicit {
        return explicit.max(1);
    }
    if changed_file_count > LARGE_REVIEW_BATCH_THRESHOLD {
        return LARGE_REVIEW_DEFAULT_MAX_ACTIVE_SESSIONS;
    }
    if requested_session_count == 0 {
        return 4;
    }
    requested_session_count.max(1)
}

fn default_planned_review_lens_panel() -> Vec<RunSessionParams> {
    [
        (
            "correctness",
            Role::Correctness,
            "Review changed behavior for correctness regressions and concrete runtime failures.",
        ),
        (
            "security",
            Role::Security,
            "Review credentials, authorization, tenant boundaries, secrets, and destructive operations.",
        ),
        (
            "architecture-contracts",
            Role::Architecture,
            "Review public APIs, return shapes, lifecycle boundaries, adapters, protocols, and state contracts.",
        ),
        (
            "performance",
            Role::Performance,
            "Review query shape, repeated work, fan-out, large data paths, and resource use.",
        ),
    ]
    .into_iter()
    .map(|(id, role, objective)| RunSessionParams {
        id: id.to_string(),
        role,
        objective: objective.to_string(),
        cwd: None,
        model_profile_id: None,
        response_format: None,
        instructions: Vec::new(),
        tool_grants: Vec::new(),
        budget: None,
    })
    .collect()
}

fn run_session_spec(
    params: RunSessionParams,
    callback_tools: &[CallbackToolGrant],
    global_instructions: &[RunInstructionParams],
) -> Result<ReviewSessionSpec> {
    let budget = params
        .budget
        .map_or_else(AgentBudget::planned_baseline, |budget| AgentBudget {
            max_turns: budget.max_turns,
            max_tool_calls: budget.max_tool_calls,
            max_prompt_tokens: budget.max_prompt_tokens,
            max_output_tokens: budget.max_output_tokens,
            budget_source: crate::contracts::BudgetSource::CallerHardCap,
        });
    let session_id = params.id.clone();
    let mut spec =
        ReviewSessionSpec::review_read_only(params.id, params.role, params.objective, budget);
    if let Some(cwd) = params.cwd {
        let capabilities = capabilities::CapabilitySet::review_read_only()
            .with_fs_scope(FsScope::subtree(RepoPath::parse(&cwd)?));
        spec = spec.with_capabilities(capabilities);
    }
    if let Some(model_profile_id) = params.model_profile_id {
        spec = spec.with_model_profile_id(model_profile_id);
    }
    if let Some(response_format) = params.response_format {
        spec = spec.with_response_format(response_format);
    }
    let mut instructions = global_instructions
        .iter()
        .map(runner_instruction)
        .collect::<Vec<_>>();
    instructions.extend(params.instructions.iter().map(runner_instruction));
    if !instructions.is_empty() {
        spec = spec.with_instructions(instructions);
    }
    let granted_tools = if params.tool_grants.is_empty() {
        callback_tools.iter().collect::<Vec<_>>()
    } else {
        let mut granted_tools = Vec::new();
        for grant in &params.tool_grants {
            let grant_id = ToolId::parse(grant)?;
            let tool = callback_tools
                .iter()
                .find(|tool| tool.id == grant_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("session {session_id} grants unknown callback tool {grant}")
                })?;
            granted_tools.push(tool);
        }
        granted_tools
    };
    for tool in granted_tools {
        spec = if tool.provider_resources.is_empty() {
            spec.grant_custom_tool_with_effects(tool.id.clone(), tool.effects)
        } else {
            spec.grant_custom_tool_with_effects_for_resources(
                tool.id.clone(),
                tool.provider_resources.clone(),
                tool.effects,
            )
        };
    }
    Ok(spec)
}

#[derive(Clone)]
pub(crate) struct CallbackToolGrant {
    pub(crate) id: ToolId,
    pub(crate) provider_resources: Vec<ProviderResourceId>,
    pub(crate) effects: ToolEffects,
}

impl CallbackToolGrant {
    pub(crate) fn from_tool_params(tool: &RunToolParams) -> Result<Self> {
        Ok(Self {
            id: ToolId::parse(&tool.id)?,
            provider_resources: parse_provider_resources(&tool.provider_resources)?,
            effects: parse_tool_effects(&tool.effects)?,
        })
    }
}

pub(crate) fn parse_tool_effects(effects: &[String]) -> Result<ToolEffects> {
    if effects.is_empty() {
        return Ok(ToolEffects::custom_read_only());
    }
    let mut parsed = ToolEffects::default();
    for effect in effects {
        match effect.as_str() {
            "read_host" => parsed.host_read = true,
            "read_network" => parsed.network_read = true,
            "write_artifact" => parsed.artifact_write = true,
            "write_scratch" => parsed.scratch_write = true,
            "external_side_effect" => {
                anyhow::bail!("external_side_effect tools are not supported in V1")
            }
            unknown => anyhow::bail!("unknown tool effect {unknown}"),
        }
    }
    Ok(parsed)
}

pub(crate) fn parse_provider_resources(resources: &[String]) -> Result<Vec<ProviderResourceId>> {
    resources
        .iter()
        .map(|resource| {
            ProviderResourceId::parse(resource).map_err(|error| anyhow::anyhow!("{error}"))
        })
        .collect()
}

fn runner_instruction(instruction: &RunInstructionParams) -> SessionInstruction {
    SessionInstruction {
        text: instruction.text.clone(),
        trusted: instruction.trusted,
        kind: instruction.kind.clone(),
    }
}

fn runner_changed_files(changed_files: &[String], change: Option<&RunChangeParams>) -> Vec<String> {
    if !changed_files.is_empty() {
        return changed_files.to_vec();
    }
    change
        .map(|change| {
            change
                .changed_files
                .iter()
                .map(|file| file.path.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn changed_file_specs(
    repo_root: &Path,
    changed_files: &[String],
    change: Option<&RunChangeParams>,
) -> Vec<ChangedFileSpec> {
    if let Some(change) = change {
        let files = change
            .changed_files
            .iter()
            .map(|file| changed_file_spec(&file.path, file.status.as_deref()))
            .collect::<Vec<_>>();
        if !files.is_empty() {
            return files;
        }
    }
    changed_files
        .iter()
        .filter(|path| repo_root.join(path).is_file())
        .cloned()
        .map(|path| changed_file_spec(&path, None))
        .collect()
}

fn runner_change_spec(
    source: Option<&ReviewSource>,
    change: Option<&RunChangeParams>,
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
        kind: runner_change_kind(source),
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

fn runner_change_kind(source: Option<&ReviewSource>) -> ChangeKind {
    match source {
        Some(ReviewSource::GithubPullRequest { .. }) => ChangeKind::PullRequest,
        Some(ReviewSource::GitlabMergeRequest { .. }) => ChangeKind::MergeRequest,
        _ => ChangeKind::LocalDiff,
    }
}

fn changed_file_spec(path: &str, status: Option<&str>) -> ChangedFileSpec {
    let status = match status.map(|status| status.to_ascii_lowercase()) {
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
    };
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

#[cfg(test)]
fn select_target_path(repo_root: &Path, changed_files: &[String]) -> Result<String> {
    for path in changed_files {
        if repo_root.join(path).is_file() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!("run requires at least one changed file that exists in the materialized worktree")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_planned_review_uses_lens_panel() {
        let sessions = default_planned_review_lens_panel();

        assert_eq!(sessions.len(), 4);
        assert_eq!(sessions[0].id, "correctness");
        assert_eq!(sessions[1].role, Role::Security);
        assert_eq!(sessions[2].id, "architecture-contracts");
        assert_eq!(sessions[3].role, Role::Performance);
    }

    #[test]
    fn defaults_large_reviews_to_eight_active_sessions() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, None),
            8
        );
    }

    #[test]
    fn keeps_small_review_default_session_parallelism() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD, None),
            2
        );
        assert_eq!(default_max_active_sessions(0, 1, None), 4);
    }

    #[test]
    fn explicit_max_active_sessions_overrides_large_review_default() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, Some(3)),
            3
        );
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, Some(0)),
            1
        );
    }

    #[test]
    fn run_session_spec_preserves_response_format() {
        let response_format = crate::runtime::contracts::ModelResponseFormat::json_schema(
            "direct_findings",
            serde_json::json!({
                "type": "object",
                "required": ["findings"],
                "properties": {
                    "findings": {"type": "array"}
                }
            }),
        );
        let spec = run_session_spec(
            RunSessionParams {
                id: "direct".to_string(),
                role: Role::Generalist,
                objective: "return findings".to_string(),
                cwd: None,
                model_profile_id: None,
                response_format: Some(response_format),
                instructions: Vec::new(),
                tool_grants: Vec::new(),
                budget: None,
            },
            &[],
            &[],
        )
        .expect("session spec");

        let scope = spec.into_session_scope();

        assert_eq!(
            scope
                .response_format
                .as_ref()
                .map(|format| format.name.as_str()),
            Some("direct_findings")
        );
    }
}
