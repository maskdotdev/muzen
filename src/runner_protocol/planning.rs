use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::Value;

#[cfg(test)]
use crate::review_planning::select_target_path;
use crate::review_planning::{
    changed_file_paths, changed_file_specs, default_max_active_sessions,
    default_review_orchestrator_session, review_change_spec, session_instruction,
    ReviewChangeDescriptor, ReviewChangedFileDescriptor,
};
use crate::reviewer_kernel::kernel_types::{
    CapabilitySet, FsScope, ProviderResourceId, RepoPath, ToolEffects, ToolId,
};
use crate::reviewer_kernel::review_contract::AgentBudget;
#[cfg(test)]
use crate::reviewer_kernel::review_contract::Role;
use crate::reviewer_kernel::snapshots::{SnapshotPathPolicy, SnapshotSpec};
use crate::reviewer_kernel::spec::{ReviewSessionSpec, RunSpec};

use super::transport::RunnerCallbackTransport;
use super::types::{
    RunChangeParams, RunInstructionParams, RunSessionParams, RunStartParams, RunToolParams,
};
use crate::review_sources::materialize::{materialize_run_source, SourceProviderConfig};
use std::sync::Arc;

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
    let change_descriptor = params.change.as_ref().map(runner_change_descriptor);
    let requested_changed_files = if params.changed_files.is_empty() {
        changed_file_paths(change_descriptor.as_ref())
    } else {
        params.changed_files.clone()
    };
    let source_provider = params
        .source_provider
        .as_ref()
        .map(|provider| SourceProviderConfig {
            base_url: provider.base_url.clone(),
            callback: provider.callback,
        });
    let materialized = materialize_run_source(
        params.repo.as_deref(),
        params.source.as_ref(),
        &requested_changed_files,
        source_provider.as_ref(),
        transport,
    )?;
    let repo_root = materialized.repo_root().to_path_buf();
    #[cfg(test)]
    let target_path = select_target_path(&repo_root, materialized.changed_files())?;
    let changed_files =
        changed_file_specs(materialized.changed_files(), change_descriptor.as_ref());
    let change = review_change_spec(
        params.source.as_ref(),
        change_descriptor.as_ref(),
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
    let snapshot = SnapshotSpec::new(&repo_root, change).with_path_policy(
        SnapshotPathPolicy::standard(max_file_bytes, max_search_matches),
    );
    let callback_tools = params
        .tools
        .iter()
        .map(CallbackToolGrant::from_tool_params)
        .collect::<Result<Vec<_>>>()?;
    let session_specs = if params.sessions.is_empty() {
        let spec = default_review_orchestrator_session(runner_instructions(&params.instructions));
        vec![grant_callback_tools(
            spec,
            "review-orchestrator",
            &callback_tools,
            &[],
        )?]
    } else {
        params
            .sessions
            .into_iter()
            .map(|session| run_session_spec(session, &callback_tools, &params.instructions))
            .collect::<Result<Vec<_>>>()?
    };
    let mut runtime_limits = crate::reviewer_kernel::kernel_types::RuntimeLimits::standard(
        max_active_sessions,
        max_file_bytes,
        max_search_matches,
    );
    if let Some(limit_params) = params.limits.as_ref() {
        runtime_limits.max_child_sessions = limit_params.max_child_sessions;
        runtime_limits.orchestrator_model_profile_id =
            limit_params.orchestrator_model_profile_id.clone();
        runtime_limits.search_model_profile_id = limit_params.search_model_profile_id.clone();
        runtime_limits.explore_model_profile_id = limit_params.explore_model_profile_id.clone();
        runtime_limits.validator_model_profile_id = limit_params.validator_model_profile_id.clone();
    }
    Ok(RunnerPlan {
        run_id: run_id.clone(),
        metadata,
        spec: RunSpec::single_snapshot(run_id, snapshot, session_specs, runtime_limits),
        max_active_sessions,
        #[cfg(test)]
        target_path,
    })
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
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::CallerHardCap,
        });
    let session_id = params.id.clone();
    let mut spec =
        ReviewSessionSpec::review_read_only(params.id, params.role, params.objective, budget);
    if let Some(cwd) = params.cwd {
        let capabilities = CapabilitySet::review_read_only()
            .with_fs_scope(FsScope::subtree(RepoPath::parse(&cwd)?));
        spec = spec.with_capabilities(capabilities);
    }
    if let Some(model_profile_id) = params.model_profile_id {
        spec = spec.with_model_profile_id(model_profile_id);
    }
    if let Some(response_format) = params.response_format {
        spec = spec.with_response_format(response_format);
    }
    let mut instructions = runner_instructions(global_instructions);
    instructions.extend(params.instructions.iter().map(runner_instruction));
    if !instructions.is_empty() {
        spec = spec.with_instructions(instructions);
    }
    grant_callback_tools(spec, &session_id, callback_tools, &params.tool_grants)
}

fn grant_callback_tools(
    mut spec: ReviewSessionSpec,
    session_id: &str,
    callback_tools: &[CallbackToolGrant],
    tool_grants: &[String],
) -> Result<ReviewSessionSpec> {
    let granted_tools = if tool_grants.is_empty() {
        callback_tools.iter().collect::<Vec<_>>()
    } else {
        let mut granted_tools = Vec::new();
        for grant in tool_grants {
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

fn runner_instructions(
    instructions: &[RunInstructionParams],
) -> Vec<crate::reviewer_kernel::kernel_types::SessionInstruction> {
    instructions.iter().map(runner_instruction).collect()
}

fn runner_instruction(
    instruction: &RunInstructionParams,
) -> crate::reviewer_kernel::kernel_types::SessionInstruction {
    session_instruction(
        instruction.kind.clone(),
        instruction.text.clone(),
        instruction.trusted,
    )
}

fn runner_change_descriptor(change: &RunChangeParams) -> ReviewChangeDescriptor<'_> {
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

#[cfg(test)]
mod tests;
