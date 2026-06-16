use std::path::Path;

use crate::runner_protocol::{
    RunAgentBudgetParams, RunChangeFileParams, RunChangeParams, RunInstructionParams,
    RunLimitParams, RunModelParams, RunSessionParams, RunSourceProviderParams, RunStartParams,
    RunToolParams, RUNNER_PROTOCOL_VERSION,
};
#[cfg(not(test))]
use crate::runner_protocol::{RunModelCredentialParams, RunModelProfileParams};

use super::session::CreateReviewSessionInput;
use super::{
    ReviewAgentSession, ReviewChangeSpec, ReviewChangedFile, ReviewInstruction, ReviewLimits,
    ReviewOptions, ReviewSessionError, ReviewSessionId, ReviewSource, ReviewToolOption,
};

pub(super) fn review_input_to_runner_start(
    input: CreateReviewSessionInput,
    review_id: &ReviewSessionId,
) -> Result<RunStartParams, ReviewSessionError> {
    let changed_files = runner_changed_files(&input.options, &input.source);
    let repo = input.source.local_repo().map(Path::to_path_buf);
    let source_provider = runner_source_provider(&input.options);
    Ok(RunStartParams {
        protocol_version: Some(RUNNER_PROTOCOL_VERSION.to_string()),
        run_id: Some(review_id.as_str().to_string()),
        repo,
        source: Some(input.source),
        source_provider,
        changed_files,
        metadata: input.options.metadata.clone(),
        change: runner_change(&input.options),
        instructions: runner_instructions(&input.options),
        sessions: runner_sessions(&input.options),
        limits: input.options.limits.map(runner_limits),
        model: runner_model(&input.options),
        tools: runner_tools(&input.options),
        heartbeat: None,
        context_engine: input.options.context_engine,
    })
}

fn runner_sessions(options: &ReviewOptions) -> Vec<RunSessionParams> {
    options
        .sessions
        .iter()
        .map(|session| runner_session(session, options.model.as_deref()))
        .collect()
}

fn runner_instructions(options: &ReviewOptions) -> Vec<RunInstructionParams> {
    options
        .instructions
        .iter()
        .map(runner_instruction)
        .collect()
}

fn runner_tools(options: &ReviewOptions) -> Vec<RunToolParams> {
    options.tools.iter().map(runner_tool).collect()
}

fn runner_change(options: &ReviewOptions) -> Option<RunChangeParams> {
    options.change.as_ref().map(runner_change_spec)
}

fn runner_changed_files(options: &ReviewOptions, source: &ReviewSource) -> Vec<String> {
    if !options.scope.files.is_empty() {
        return options.scope.files.clone();
    }
    if let Some(change) = &options.change {
        let changed_files = change
            .changed_files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        if !changed_files.is_empty() {
            return changed_files;
        }
    }
    source.runner_changed_files(&options.scope)
}

fn runner_source_provider(options: &ReviewOptions) -> Option<RunSourceProviderParams> {
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

fn runner_model(_options: &ReviewOptions) -> Option<RunModelParams> {
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
        hosted_runner_model(_options)
    }
}

#[cfg(not(test))]
fn hosted_runner_model(options: &ReviewOptions) -> Option<RunModelParams> {
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

fn runner_change_spec(change: &ReviewChangeSpec) -> RunChangeParams {
    RunChangeParams {
        kind: change.kind.clone(),
        base_revision: change.base_revision.clone(),
        start_revision: change.start_revision.clone(),
        head_revision: change.head_revision.clone(),
        changed_files: change
            .changed_files
            .iter()
            .map(runner_changed_file)
            .collect(),
        diff: change.diff.clone(),
        review_target: change.review_target.clone(),
        metadata: change.metadata.clone(),
    }
}

fn runner_changed_file(file: &ReviewChangedFile) -> RunChangeFileParams {
    RunChangeFileParams {
        path: file.path.clone(),
        status: file.status.clone(),
    }
}

fn runner_instruction(instruction: &ReviewInstruction) -> RunInstructionParams {
    RunInstructionParams {
        kind: instruction.kind.clone(),
        text: instruction.text.clone(),
        trusted: instruction.trusted,
    }
}

fn runner_tool(tool: &ReviewToolOption) -> RunToolParams {
    RunToolParams {
        id: tool.id.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
        effects: tool.effects.clone(),
        cacheable: tool.cacheable,
        provider_resources: tool.provider_resources.clone(),
    }
}

fn runner_session(session: &ReviewAgentSession, default_model: Option<&str>) -> RunSessionParams {
    RunSessionParams {
        id: session.id.clone(),
        role: session.role,
        objective: session.objective.clone(),
        cwd: session.cwd.clone(),
        model_profile_id: session
            .model_profile_id
            .clone()
            .or_else(|| default_model.map(str::to_string)),
        response_format: None,
        instructions: session
            .instructions
            .iter()
            .map(runner_instruction)
            .collect(),
        tool_grants: session.tool_grants.clone(),
        budget: session.budget.as_ref().map(|budget| RunAgentBudgetParams {
            max_turns: budget.max_turns,
            max_tool_calls: budget.max_tool_calls,
            max_prompt_tokens: budget.max_prompt_tokens,
            max_output_tokens: budget.max_output_tokens,
        }),
    }
}

fn runner_limits(limits: ReviewLimits) -> RunLimitParams {
    RunLimitParams {
        max_active_sessions: limits.max_active_sessions,
        max_child_sessions: None,
        max_file_bytes: limits.max_file_bytes,
        max_search_matches: limits.max_search_matches,
        orchestrator_model_profile_id: None,
        search_model_profile_id: None,
        explore_model_profile_id: None,
        validator_model_profile_id: None,
    }
}
