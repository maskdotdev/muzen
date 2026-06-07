use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::contracts::{
    AgentBudget, ModelApiProtocol, ModelProfileRefV1, ProviderKind, Role, ToolCallingMode,
};
use crate::review_session::ReviewSource;
use crate::reviewer::runtime::RuntimeError;
use crate::reviewer::{
    capabilities, ids::ToolId, paths, runtime_events::EventSink as RuntimeEventSink, ChangeKind,
    ChangeSpec, ChangedFileSpec, ChangedFileStatus, ReviewEventRecord, ReviewEventSink,
    ReviewRunLimits, ReviewRunSummary, ReviewSessionSpec, ReviewToolRegistry, Run, RunSpec,
    SnapshotPathPolicy, SnapshotSpec,
};
use crate::runtime::contracts::{ProviderResourceId, SessionInstruction, ToolEffects};
use crate::runtime::model::{
    CredentialResolver, EnvCredentialResolver, ModelLimiter, ProfileModelRouter,
};
use crate::runtime::policy::ReviewerPolicy;
use crate::runtime::tools::ToolRegistry as RuntimeToolRegistry;

#[cfg(test)]
use super::adapters::TestRunnerModel;
use super::adapters::{CallbackReviewModel, CallbackReviewTool, StreamingRunnerEventSink};
use super::materialize::materialize_run_source;
use super::stored::RunnerStoredRun;
use super::transport::RunnerCallbackTransport;
use super::types::{
    RunChangeParams, RunHeartbeatConfigParams, RunHeartbeatParams, RunHeartbeatResult,
    RunInstructionParams, RunModelCredentialParams, RunModelParams, RunModelProfileParams,
    RunSessionParams, RunStartParams, RunToolParams, RunnerFileReview, RunnerFinding,
    RunnerFindingEvidence, RunnerFindingLocation, RunnerRunResult, RunnerRunSummary,
    RunnerSecretResolveParams, RunnerSecretResolveResult, RunnerSnapshotSummary,
};
use super::RUNNER_PROTOCOL_VERSION;

const LARGE_REVIEW_BATCH_THRESHOLD: usize = 8;
const LARGE_REVIEW_BATCH_SIZE: usize = 4;
const LARGE_REVIEW_DEFAULT_MAX_ACTIVE_SESSIONS: usize = 4;

pub(crate) struct ExecutedRun {
    pub(crate) result: RunnerRunResult,
    pub(crate) events: Vec<ReviewEventRecord>,
    pub(crate) stored: RunnerStoredRun,
}

pub(crate) fn execute_run_start(
    params: RunStartParams,
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
    cancel: CancellationToken,
) -> Result<ExecutedRun> {
    if let Some(protocol_version) = &params.protocol_version {
        if protocol_version != RUNNER_PROTOCOL_VERSION {
            anyhow::bail!("unsupported protocolVersion {protocol_version}");
        }
    }
    let run_id = params.run_id.unwrap_or_else(|| "muzen-run".to_string());
    let metadata = params.metadata.clone();
    let heartbeat = start_heartbeat(
        &run_id,
        params.heartbeat.as_ref(),
        transport.clone(),
        cancel.clone(),
    )?;
    let requested_changed_files =
        runner_changed_files(&params.changed_files, params.change.as_ref());
    let materialized = materialize_run_source(
        params.repo.as_deref(),
        params.source.as_ref(),
        &requested_changed_files,
        params.source_provider.as_ref(),
        transport.as_ref(),
    )?;
    let repo_root = materialized.repo_root().to_path_buf();
    let target_path = select_target_path(&repo_root, materialized.changed_files())?;
    #[cfg(not(test))]
    let _ = &target_path;
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
    let explicit_max_active_sessions = params
        .limits
        .as_ref()
        .and_then(|limits| limits.max_active_sessions);
    let max_active_sessions = default_max_active_sessions(
        params.sessions.len(),
        change.changed_files.len(),
        explicit_max_active_sessions,
    );
    let sessions = if params.sessions.is_empty() {
        vec![RunSessionParams {
            id: "generalist".to_string(),
            role: Role::Generalist,
            objective: "Review the repository change.".to_string(),
            cwd: None,
            model_profile_id: None,
            instructions: Vec::new(),
            tool_grants: Vec::new(),
            budget: None,
        }]
    } else {
        params.sessions
    };
    let sessions =
        expand_sessions_for_changed_file_batches(sessions, change.changed_files.as_slice());
    let snapshot = SnapshotSpec::new(&repo_root, change).with_path_policy(
        SnapshotPathPolicy::standard(max_file_bytes, max_search_matches),
    );
    let global_instructions = params.instructions.clone();
    let callback_tools = params
        .tools
        .iter()
        .map(CallbackToolGrant::from_tool_params)
        .collect::<Result<Vec<_>>>()?;
    let session_specs = sessions
        .into_iter()
        .map(|session| run_session_spec(session, &callback_tools, &global_instructions))
        .collect::<Result<Vec<_>>>()?;
    let limits = ReviewRunLimits::standard(max_active_sessions, max_file_bytes, max_search_matches);
    let spec = RunSpec::single_snapshot(run_id.clone(), snapshot, session_specs, limits);
    let event_sink = Arc::new(RecordingReviewEventSink::default());
    let streaming_sink = transport.as_ref().map(|transport| {
        Arc::new(StreamingRunnerEventSink::new(transport.clone())) as Arc<dyn RuntimeEventSink>
    });
    let reviewer_policy = Arc::new(ReviewerPolicy::new());
    let tool_registry = runner_tool_registry(&run_id, &params.tools, transport.clone())?;
    let mut builder = Run::builder(spec);
    let model = params.model.as_ref().ok_or_else(|| {
        anyhow::anyhow!("run requires a model; pass a callback or hosted provider model")
    })?;
    if model.callback {
        let transport = transport
            .clone()
            .ok_or_else(|| anyhow::anyhow!("callback model requires interactive stdio"))?;
        builder = builder.review_model(Arc::new(CallbackReviewModel::new(
            run_id.clone(),
            transport,
        )));
    } else if !model.model_profiles.is_empty() {
        let router = hosted_model_router(
            model,
            max_active_sessions,
            Arc::clone(&tool_registry),
            Arc::clone(&reviewer_policy),
            transport.clone(),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        builder = builder.model_router(Arc::new(router));
    } else {
        #[cfg(test)]
        {
            builder = builder.review_model(Arc::new(TestRunnerModel::new(
                target_path,
                "TODO|fn|class|export|pub".to_string(),
            )));
        }
        #[cfg(not(test))]
        anyhow::bail!("run model must be callback or hosted provider model");
    }
    builder = builder
        .shared_tool_registry(tool_registry)
        .reviewer_policy(reviewer_policy);
    let run = if let Some(streaming_sink) = streaming_sink {
        builder.event_sink(streaming_sink).build()
    } else {
        builder.review_event_sink(event_sink.clone()).build()
    }
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build runner tokio runtime")?;
    let report = runtime.block_on(run.execute_with_cancel(cancel));
    heartbeat.stop();
    let result = runner_result_from_report(&report, metadata);
    let stored = RunnerStoredRun::from_report(&report, result.clone());
    Ok(ExecutedRun {
        result,
        events: event_sink.records(),
        stored,
    })
}

struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
}

impl HeartbeatGuard {
    fn noop() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(true)),
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn start_heartbeat(
    run_id: &str,
    config: Option<&RunHeartbeatConfigParams>,
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
    cancel: CancellationToken,
) -> Result<HeartbeatGuard> {
    let Some(config) = config else {
        return Ok(HeartbeatGuard::noop());
    };
    if !config.callback {
        return Ok(HeartbeatGuard::noop());
    }
    let transport = transport
        .ok_or_else(|| anyhow::anyhow!("heartbeat callback requires interactive stdio"))?;
    let interval = Duration::from_millis(config.interval_ms.unwrap_or(30_000).max(1));
    let lease_seconds = config.lease_seconds;
    let run_id = run_id.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    thread::spawn(move || {
        let started = Instant::now();
        let mut sequence = 0u64;
        while !thread_stop.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            thread::sleep(interval);
            if thread_stop.load(Ordering::SeqCst) || cancel.is_cancelled() {
                break;
            }
            sequence += 1;
            let params = RunHeartbeatParams {
                protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
                run_id: run_id.clone(),
                sequence,
                elapsed_ms: started.elapsed().as_micros().div_ceil(1000) as u64,
                lease_seconds,
            };
            let should_continue = transport
                .request("run.heartbeat", json!(params))
                .ok()
                .and_then(|value| serde_json::from_value::<RunHeartbeatResult>(value).ok())
                .map(|result| result.continue_run)
                .unwrap_or(false);
            if !should_continue {
                cancel.cancel();
                break;
            }
        }
    });
    Ok(HeartbeatGuard { stop })
}

fn runner_tool_registry(
    run_id: &str,
    tools: &[RunToolParams],
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
) -> Result<Arc<RuntimeToolRegistry>> {
    let mut registry = ReviewToolRegistry::review_defaults()
        .map_err(|error| anyhow::anyhow!("failed to create review tool registry: {error}"))?;
    if !tools.is_empty() {
        let transport =
            transport.ok_or_else(|| anyhow::anyhow!("callback tools require interactive stdio"))?;
        for tool in tools {
            let provider_resources = parse_provider_resources(&tool.provider_resources)?;
            let effects = parse_tool_effects(&tool.effects)?;
            registry
                .register_scoped_tool_with_effects(
                    &tool.id,
                    tool.description.clone(),
                    tool.parameters.clone(),
                    tool.cacheable,
                    provider_resources,
                    effects,
                    Arc::new(CallbackReviewTool::new(
                        run_id.to_string(),
                        transport.clone(),
                    )),
                )
                .map_err(|error| {
                    anyhow::anyhow!("failed to register SDK tool {}: {error}", tool.id)
                })?;
        }
    }
    Ok(Arc::new(registry.into_tool_registry()))
}

fn hosted_model_router(
    model: &RunModelParams,
    max_active_sessions: usize,
    tool_registry: Arc<RuntimeToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
) -> crate::reviewer::runtime::RuntimeResult<ProfileModelRouter> {
    let profiles = model
        .model_profiles
        .iter()
        .map(model_profile_ref)
        .collect::<crate::reviewer::runtime::RuntimeResult<Vec<_>>>()?;
    let default_profile_id = model
        .default_model_profile_id
        .clone()
        .or_else(|| profiles.first().map(|profile| profile.id.clone()))
        .ok_or_else(|| RuntimeError::InvalidInput("hosted model requires a profile".to_string()))?;
    let base_url = hosted_model_base_url(model, &default_profile_id)?;
    ProfileModelRouter::from_profiles(
        &profiles,
        default_profile_id,
        base_url,
        Arc::new(ModelLimiter::new_with_per_key(
            max_active_sessions.max(1),
            max_active_sessions.max(1),
        )),
        tool_registry,
        reviewer_policy,
        Arc::new(RunnerCredentialResolver::new(transport)),
    )
}

fn hosted_model_base_url(
    model: &RunModelParams,
    default_profile_id: &str,
) -> crate::reviewer::runtime::RuntimeResult<String> {
    let mut configured_base_url: Option<&str> = None;
    for profile in &model.model_profiles {
        let Some(base_url) = profile
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if let Some(existing) = configured_base_url {
            if existing != base_url {
                return Err(RuntimeError::InvalidInput(
                    "runner v1 hosted model profiles must share one baseUrl".to_string(),
                ));
            }
        }
        configured_base_url = Some(base_url);
    }
    Ok(configured_base_url
        .map(ToString::to_string)
        .or_else(|| {
            model
                .model_profiles
                .iter()
                .find(|profile| profile.id == default_profile_id)
                .and_then(|profile| profile.base_url.clone())
        })
        .or_else(|| {
            std::env::var("OAI_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .ok()
        })
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string()))
}

fn model_profile_ref(
    params: &RunModelProfileParams,
) -> crate::reviewer::runtime::RuntimeResult<ModelProfileRefV1> {
    let provider_kind = match params.provider.as_str() {
        "openai" | "openai_compatible" => ProviderKind::OpenaiCompatible,
        unknown => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported model provider {unknown}"
            )))
        }
    };
    let api_protocol = match params.api_protocol.as_deref().unwrap_or("responses") {
        "responses" => ModelApiProtocol::Responses,
        "chat_completions" => ModelApiProtocol::ChatCompletions,
        unknown => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported model apiProtocol {unknown}"
            )))
        }
    };
    Ok(ModelProfileRefV1 {
        id: params.id.clone(),
        provider_kind,
        api_protocol,
        provider_profile_id: params.provider.clone(),
        credential_ref: credential_ref(params.credential.as_ref())?,
        model: params.model.clone(),
        max_input_tokens: params.max_input_tokens.unwrap_or(128_000),
        max_output_tokens: params.max_output_tokens.unwrap_or(8_000),
        tool_calling_mode: ToolCallingMode::Auto,
        temperature: params.temperature,
        top_p: params.top_p,
    })
}

fn credential_ref(
    credential: Option<&RunModelCredentialParams>,
) -> crate::reviewer::runtime::RuntimeResult<String> {
    let Some(credential) = credential else {
        return Ok("env:OPENAI_API_KEY".to_string());
    };
    match (
        credential
            .env
            .as_deref()
            .filter(|value| !value.trim().is_empty()),
        credential
            .secret_ref
            .as_deref()
            .filter(|value| !value.trim().is_empty()),
    ) {
        (Some(env), None) => Ok(format!("env:{}", env.trim())),
        (None, Some(secret_ref)) => Ok(format!("secret:{}", secret_ref.trim())),
        _ => Err(RuntimeError::InvalidInput(
            "model credential must be exactly one of env or secretRef".to_string(),
        )),
    }
}

struct RunnerCredentialResolver {
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
    env: EnvCredentialResolver,
}

impl RunnerCredentialResolver {
    fn new(transport: Option<Arc<dyn RunnerCallbackTransport>>) -> Self {
        Self {
            transport,
            env: EnvCredentialResolver,
        }
    }
}

impl CredentialResolver for RunnerCredentialResolver {
    fn resolve_credential(
        &self,
        credential_ref: &str,
    ) -> crate::reviewer::runtime::RuntimeResult<String> {
        let Some(secret_ref) = credential_ref.strip_prefix("secret:") else {
            return self.env.resolve_credential(credential_ref);
        };
        let transport = self.transport.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "model credential secretRef requires interactive stdio".to_string(),
            )
        })?;
        let params = RunnerSecretResolveParams {
            protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
            ref_name: secret_ref.to_string(),
        };
        let value = transport
            .request("secret.resolve", json!(params))
            .map_err(|_| {
                RuntimeError::InvalidInput("model credential is unavailable".to_string())
            })?;
        let result = serde_json::from_value::<RunnerSecretResolveResult>(value)
            .map_err(|_| RuntimeError::InvalidInput("invalid secret.resolve result".to_string()))?;
        if result.value.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "model credential is unavailable".to_string(),
            ));
        }
        Ok(result.value)
    }
}

fn expand_sessions_for_changed_file_batches(
    sessions: Vec<RunSessionParams>,
    changed_files: &[ChangedFileSpec],
) -> Vec<RunSessionParams> {
    if changed_files.len() <= LARGE_REVIEW_BATCH_THRESHOLD {
        return sessions;
    }

    let batch_paths = changed_files
        .iter()
        .filter_map(changed_file_review_path)
        .collect::<Vec<_>>();
    if batch_paths.len() <= LARGE_REVIEW_BATCH_THRESHOLD {
        return sessions;
    }

    let total_batches = batch_paths.len().div_ceil(LARGE_REVIEW_BATCH_SIZE);
    let mut expanded = Vec::with_capacity(sessions.len() * total_batches);
    for session in sessions {
        for (batch_index, batch) in batch_paths.chunks(LARGE_REVIEW_BATCH_SIZE).enumerate() {
            let batch_number = batch_index + 1;
            let mut batched = session.clone();
            batched.id = format!("{}-batch-{batch_number:02}", session.id);
            batched.objective = format!(
                "{} Focus on changed-file batch {batch_number}/{total_batches}.",
                session.objective
            );
            batched.instructions.push(RunInstructionParams {
                kind: "changed_file_batch".to_string(),
                trusted: true,
                text: changed_file_batch_instruction(batch_number, total_batches, batch),
            });
            expanded.push(batched);
        }
    }
    expanded
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
    requested_session_count.max(1)
}

fn changed_file_review_path(file: &ChangedFileSpec) -> Option<String> {
    file.new_path
        .as_ref()
        .or(file.old_path.as_ref())
        .map(|path| path.to_string_lossy().into_owned())
}

fn changed_file_batch_instruction(
    batch_number: usize,
    total_batches: usize,
    batch: &[String],
) -> String {
    let files = batch
        .iter()
        .enumerate()
        .map(|(index, path)| format!("{}. {}", index + 1, path))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Changed-file batch {batch_number}/{total_batches} ({count} files):\n{files}\n\nReview these changed files first. For each file in this batch, read the diff and inspect the file directly unless the file no longer exists or direct file inspection is impossible. For every changed hunk, identify the changed behavior, the input/state that reaches it, and the nearest caller, test, template, or contract needed to judge it. Continue reviewing all assigned files after recording a finding; one finding is not a stopping condition. You may inspect related files outside the batch when needed. Do not call finish until this batch has concrete diff, file, and search evidence, and call out any batch file you could not inspect.",
        count = batch.len()
    )
}

fn run_session_spec(
    params: RunSessionParams,
    callback_tools: &[CallbackToolGrant],
    global_instructions: &[RunInstructionParams],
) -> Result<ReviewSessionSpec> {
    let budget = params.budget.map_or(
        AgentBudget {
            max_turns: 7,
            max_tool_calls: 14,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
        },
        |budget| AgentBudget {
            max_turns: budget.max_turns,
            max_tool_calls: budget.max_tool_calls,
            max_prompt_tokens: budget.max_prompt_tokens,
            max_output_tokens: budget.max_output_tokens,
        },
    );
    let session_id = params.id.clone();
    let mut spec =
        ReviewSessionSpec::review_read_only(params.id, params.role, params.objective, budget);
    let instructions = global_instructions
        .iter()
        .chain(params.instructions.iter())
        .map(runner_instruction)
        .collect::<Vec<_>>();
    if !instructions.is_empty() {
        spec = spec.with_instructions(instructions);
    }
    if let Some(model_profile_id) = params.model_profile_id {
        spec = spec.with_model_profile_id(model_profile_id);
    }
    if let Some(cwd) = params.cwd {
        let repo_path = paths::RepoPath::parse(&cwd).map_err(|error| anyhow::anyhow!("{error}"))?;
        let capabilities = capabilities::CapabilitySet::review_read_only()
            .with_fs_scope(capabilities::FsScope::subtree(repo_path));
        spec = spec.with_capabilities(capabilities);
    }
    let granted_tools = if params.tool_grants.is_empty() {
        callback_tools.iter().collect::<Vec<_>>()
    } else {
        let mut granted_tools = Vec::new();
        for grant in &params.tool_grants {
            let grant_id = ToolId::parse(grant).map_err(|error| anyhow::anyhow!("{error}"))?;
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

#[derive(Debug, Clone)]
struct CallbackToolGrant {
    id: ToolId,
    provider_resources: Vec<ProviderResourceId>,
    effects: ToolEffects,
}

impl CallbackToolGrant {
    fn from_tool_params(tool: &RunToolParams) -> Result<Self> {
        Ok(Self {
            id: ToolId::parse(&tool.id).map_err(|error| anyhow::anyhow!("{error}"))?,
            provider_resources: parse_provider_resources(&tool.provider_resources)?,
            effects: parse_tool_effects(&tool.effects)?,
        })
    }
}

fn parse_tool_effects(effects: &[String]) -> Result<ToolEffects> {
    if effects.is_empty() {
        return Ok(ToolEffects::custom_read_only());
    }
    let mut parsed = ToolEffects::default();
    for effect in effects {
        match effect.as_str() {
            "read_repo" | "read_diff" => parsed.repo_read = true,
            "read_artifact" => parsed.artifact_read = true,
            "write_artifact" => parsed.artifact_write = true,
            "read_host" => parsed.host_read = true,
            "read_network" => parsed.network_read = true,
            "read_scratch" => parsed.scratch_read = true,
            "write_scratch" => parsed.scratch_write = true,
            "external_side_effect" => {
                anyhow::bail!("external_side_effect tools are not supported in V1")
            }
            unknown => anyhow::bail!("unknown tool effect {unknown}"),
        }
    }
    Ok(parsed)
}

fn parse_provider_resources(resources: &[String]) -> Result<Vec<ProviderResourceId>> {
    resources
        .iter()
        .map(|resource| {
            ProviderResourceId::parse(resource).map_err(|error| anyhow::anyhow!("{error}"))
        })
        .collect()
}

fn runner_instruction(instruction: &RunInstructionParams) -> SessionInstruction {
    SessionInstruction {
        kind: instruction.kind.clone(),
        text: instruction.text.clone(),
        trusted: instruction.trusted,
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

fn runner_result_from_report(
    report: &crate::reviewer::RunReport,
    metadata: BTreeMap<String, Value>,
) -> RunnerRunResult {
    let mut summary = runner_summary_from_review(&report.summary);
    let snapshots = report
        .snapshot_manifests()
        .into_iter()
        .map(|manifest| RunnerSnapshotSummary {
            snapshot_id: manifest.snapshot_id.0,
            files: manifest.files.len(),
            changed_files: manifest.changed_files.len(),
            captured_files: manifest
                .files
                .iter()
                .filter(|file| {
                    matches!(
                        file.capture_status,
                        crate::reviewer::storage::SnapshotCaptureStatus::Captured
                    )
                })
                .count(),
            captured_bytes: manifest.captured_text_bytes as u64,
        })
        .collect();
    let findings = dedupe_runner_findings(
        report
            .findings()
            .into_iter()
            .map(|finding| RunnerFinding {
                id: finding.id,
                title: finding.title,
                claim: finding.claim,
                evidence_count: finding.evidence_count,
                publishable: finding.publishable,
                severity: Some(finding.severity),
                confidence: Some(finding.confidence),
                validation_status: Some(finding.validation_status),
                evidence: finding
                    .evidence
                    .into_iter()
                    .map(|evidence| RunnerFindingEvidence {
                        evidence_id: evidence.evidence_id,
                        artifact_id: evidence.artifact_id.0,
                        kind: evidence.kind,
                        content_hash: evidence.content_hash,
                        producing_tool_call_id: evidence.producing_tool_call_id.0,
                    })
                    .collect(),
                discovered_by: finding.discovered_by,
                validated_by: finding.validated_by,
                challenged_by: finding.challenged_by,
                location: finding.location.map(|location| RunnerFindingLocation {
                    path: location.path,
                    revision: None,
                    start_line: location.start_line,
                    end_line: location.end_line,
                    start_column: None,
                    end_column: None,
                    side: None,
                    provider_anchor: None,
                }),
            })
            .collect(),
    );
    summary.findings = findings.len();
    summary.publishable_findings = findings
        .iter()
        .filter(|finding| finding.publishable)
        .count();
    let file_reviews = report
        .file_reviews()
        .into_iter()
        .map(|review| RunnerFileReview {
            path: review.path,
            verdict: review.verdict,
            summary: review.summary,
            related_paths: review.related_paths,
            evidence_artifact_ids: review.evidence_artifact_ids,
            evidence_count: review.evidence_count,
            session_id: review.session_id,
            unit_id: review.unit_id,
        })
        .collect();
    RunnerRunResult {
        protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
        run_id: report.run_id.clone(),
        status: summary_status(&summary),
        summary,
        file_reviews,
        findings,
        snapshots,
        metadata,
    }
}

fn dedupe_runner_findings(findings: Vec<RunnerFinding>) -> Vec<RunnerFinding> {
    let mut by_key: BTreeMap<String, RunnerFinding> = BTreeMap::new();
    let mut order = Vec::new();
    for finding in findings {
        let key = finding_dedupe_key(&finding);
        if let Some(existing) = by_key.get_mut(&key) {
            merge_runner_finding(existing, finding);
            continue;
        }
        order.push(key.clone());
        by_key.insert(key, finding);
    }
    order
        .into_iter()
        .filter_map(|key| by_key.remove(&key))
        .collect()
}

fn finding_dedupe_key(finding: &RunnerFinding) -> String {
    let text = normalize_finding_text(&format!("{} {}", finding.title, finding.claim));
    if text.contains("foreach")
        && text.contains("async")
        && (text.contains("await") || text.contains("promise"))
        && (text.contains("cleanup") || text.contains("delete") || text.contains("deletion"))
    {
        return "root-cause:unawaited-async-iteration-cleanup".to_string();
    }
    format!("text:{text}")
}

fn normalize_finding_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn merge_runner_finding(existing: &mut RunnerFinding, duplicate: RunnerFinding) {
    existing.confidence = match (existing.confidence, duplicate.confidence) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (None, right) => right,
        (left, None) => left,
    };
    existing.publishable |= duplicate.publishable;
    append_unique(&mut existing.discovered_by, duplicate.discovered_by);
    append_unique(&mut existing.validated_by, duplicate.validated_by);
    append_unique(&mut existing.challenged_by, duplicate.challenged_by);

    let mut seen_evidence = existing
        .evidence
        .iter()
        .map(|evidence| evidence.evidence_id.clone())
        .collect::<BTreeSet<_>>();
    for evidence in duplicate.evidence {
        if seen_evidence.insert(evidence.evidence_id.clone()) {
            existing.evidence.push(evidence);
        }
    }
    existing.evidence_count = existing.evidence.len();

    if let Some(path) = duplicate
        .location
        .as_ref()
        .map(|location| location.path.clone())
    {
        append_related_location(&mut existing.claim, &path);
    }
}

fn append_unique(values: &mut Vec<String>, additions: Vec<String>) {
    let mut seen = values.iter().cloned().collect::<BTreeSet<_>>();
    for value in additions {
        if seen.insert(value.clone()) {
            values.push(value);
        }
    }
}

fn append_related_location(claim: &mut String, path: &str) {
    if claim.contains(path) {
        return;
    }
    if claim.contains("Also observed in:") {
        claim.push_str(", ");
        claim.push_str(path);
    } else {
        claim.push_str(" Also observed in: ");
        claim.push_str(path);
    }
}

fn runner_summary_from_review(summary: &ReviewRunSummary) -> RunnerRunSummary {
    RunnerRunSummary {
        sessions: summary.sessions,
        completed_sessions: summary.completed_sessions,
        model_calls: summary.model_calls,
        tool_calls: summary.tool_calls,
        findings: summary.findings,
        publishable_findings: summary.publishable_findings,
        elapsed_ms: summary.elapsed_ms,
        input_tokens: summary.input_tokens,
        output_tokens: summary.output_tokens,
        total_tokens: summary.total_tokens,
        artifacts: summary.artifacts,
        artifact_bytes: summary.artifact_bytes,
        snapshot_count: summary.snapshot_count,
    }
}

fn summary_status(summary: &RunnerRunSummary) -> String {
    if summary.completed_sessions == summary.sessions {
        "completed".to_string()
    } else {
        "partial".to_string()
    }
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
        .to_vec()
        .into_iter()
        .filter(|path| repo_root.join(path).is_file())
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
        snapshot_mode: crate::reviewer::SnapshotMode::WorktreeHead,
        rename_detection: crate::reviewer::RenameDetection::None,
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

fn select_target_path(repo_root: &Path, changed_files: &[String]) -> Result<String> {
    for path in changed_files {
        if repo_root.join(path).is_file() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!("run requires at least one changed file that exists in the materialized worktree")
}

#[derive(Default)]
struct RecordingReviewEventSink {
    records: std::sync::Mutex<Vec<ReviewEventRecord>>,
}

impl RecordingReviewEventSink {
    fn records(&self) -> Vec<ReviewEventRecord> {
        self.records
            .lock()
            .expect("review event sink poisoned")
            .clone()
    }
}

impl ReviewEventSink for RecordingReviewEventSink {
    fn emit_review_event(&self, record: ReviewEventRecord) {
        self.records
            .lock()
            .expect("review event sink poisoned")
            .push(record);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn empty_tool_effects_keep_legacy_custom_read_only_authority() {
        let effects = parse_tool_effects(&[]).expect("default effects");

        assert_eq!(effects, ToolEffects::custom_read_only());
    }

    #[test]
    fn parses_explicit_provider_neutral_tool_effects() {
        let effects = parse_tool_effects(&[
            "read_host".to_string(),
            "read_network".to_string(),
            "write_artifact".to_string(),
            "write_scratch".to_string(),
        ])
        .expect("explicit effects");

        assert!(effects.host_read);
        assert!(effects.network_read);
        assert!(effects.artifact_write);
        assert!(effects.scratch_write);
        assert!(!effects.repo_read);
        assert!(!effects.artifact_read);
        assert!(!effects.scratch_read);
        assert!(!effects.external_side_effect);
    }

    #[test]
    fn rejects_external_side_effect_tools_in_runner_v1() {
        let error = parse_tool_effects(&["external_side_effect".to_string()])
            .expect_err("external side effects are unsupported");

        assert!(error
            .to_string()
            .contains("external_side_effect tools are not supported in V1"));
    }

    #[test]
    fn rejects_unknown_tool_effects() {
        let error = parse_tool_effects(&["write_host".to_string()])
            .expect_err("unknown effects are rejected");

        assert!(error.to_string().contains("unknown tool effect write_host"));
    }

    #[test]
    fn maps_runner_change_params_into_core_change_spec() {
        let change = RunChangeParams {
            kind: "revision_range".to_string(),
            base_revision: Some("base-sha".to_string()),
            start_revision: Some("merge-base-sha".to_string()),
            head_revision: Some("head-sha".to_string()),
            changed_files: Vec::new(),
            diff: Some("diff --git a/src/lib.rs b/src/lib.rs".to_string()),
            review_target: Some("gitlab:group/project!42".to_string()),
            metadata: BTreeMap::new(),
        };

        let spec = runner_change_spec(
            Some(&ReviewSource::gitlab_merge_request("group", "project", 42).unwrap()),
            Some(&change),
            vec![changed_file_spec("src/lib.rs", Some("modified"))],
            Some("materialized diff should not override explicit diff"),
            "review-1",
        );

        assert_eq!(spec.kind, ChangeKind::MergeRequest);
        assert_eq!(spec.change_id, "gitlab:group/project!42");
        assert_eq!(spec.base_revision_id, "base-sha");
        assert_eq!(
            spec.merge_base_revision_id.as_deref(),
            Some("merge-base-sha")
        );
        assert_eq!(spec.head_revision_id, "head-sha");
        assert_eq!(spec.source_ref, "head-sha");
        assert_eq!(spec.target_ref, "base-sha");
        assert_eq!(
            spec.inline_diff.as_deref(),
            Some("diff --git a/src/lib.rs b/src/lib.rs")
        );

        let mut change_without_diff = change;
        change_without_diff.diff = None;
        let fallback_spec = runner_change_spec(
            Some(&ReviewSource::gitlab_merge_request("group", "project", 42).unwrap()),
            Some(&change_without_diff),
            vec![changed_file_spec("src/lib.rs", Some("modified"))],
            Some("materialized diff"),
            "review-1",
        );
        assert_eq!(
            fallback_spec.inline_diff.as_deref(),
            Some("materialized diff")
        );
    }

    #[test]
    fn maps_runner_changed_file_statuses_without_requiring_existing_files() {
        let added = changed_file_spec("src/new.rs", Some("added"));
        let deleted = changed_file_spec("src/old.rs", Some("deleted"));
        let renamed = changed_file_spec("src/renamed.rs", Some("renamed"));

        assert_eq!(added.status, ChangedFileStatus::Added);
        assert!(added.old_path.is_none());
        assert_eq!(
            added.new_path.as_deref(),
            Some(std::path::Path::new("src/new.rs"))
        );
        assert_eq!(deleted.status, ChangedFileStatus::Deleted);
        assert_eq!(
            deleted.old_path.as_deref(),
            Some(std::path::Path::new("src/old.rs"))
        );
        assert!(deleted.new_path.is_none());
        assert_eq!(renamed.status, ChangedFileStatus::Renamed);
    }

    #[test]
    fn keeps_small_reviews_as_single_sessions() {
        let sessions = vec![test_session("correctness")];
        let changed_files = (0..LARGE_REVIEW_BATCH_THRESHOLD)
            .map(|index| ChangedFileSpec::modified(format!("src/file_{index}.rs")))
            .collect::<Vec<_>>();

        let expanded = expand_sessions_for_changed_file_batches(sessions, &changed_files);

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].id, "correctness");
        assert!(expanded[0].instructions.is_empty());
    }

    #[test]
    fn expands_large_reviews_into_changed_file_batches_per_session() {
        let sessions = vec![test_session("correctness"), test_session("security")];
        let changed_files = (0..25)
            .map(|index| ChangedFileSpec::modified(format!("src/file_{index}.rs")))
            .collect::<Vec<_>>();

        let expanded = expand_sessions_for_changed_file_batches(sessions, &changed_files);

        assert_eq!(expanded.len(), 14);
        assert_eq!(expanded[0].id, "correctness-batch-01");
        assert_eq!(expanded[1].id, "correctness-batch-02");
        assert_eq!(expanded[2].id, "correctness-batch-03");
        assert_eq!(expanded[7].id, "security-batch-01");
        assert_eq!(expanded[0].instructions.len(), 1);
        assert!(expanded[0].objective.contains("batch 1/7"));
        assert!(expanded[0].instructions[0]
            .text
            .contains("Changed-file batch 1/7 (4 files)"));
        assert!(expanded[0].instructions[0]
            .text
            .contains("1. src/file_0.rs"));
        assert!(expanded[6].instructions[0]
            .text
            .contains("Changed-file batch 7/7 (1 files)"));
        assert!(expanded[6].instructions[0]
            .text
            .contains("1. src/file_24.rs"));
    }

    #[test]
    fn defaults_large_reviews_to_four_active_sessions() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, None),
            4
        );
    }

    #[test]
    fn keeps_small_review_default_session_parallelism() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD, None),
            2
        );
        assert_eq!(default_max_active_sessions(0, 1, None), 1);
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
    fn dedupes_unawaited_async_iteration_cleanup_findings() {
        let findings = dedupe_runner_findings(vec![
            test_finding(
                "finding-a",
                "Unawaited reschedule cleanup deletions can be lost",
                "src/a.ts starts async cleanup deletions inside forEach(async ...) without awaiting returned promises.",
                "src/a.ts",
            ),
            test_finding(
                "finding-b",
                "Reschedule fires deletion promises without awaiting them",
                "src/b.ts starts deletion promises from forEach(async ...) and never awaits them.",
                "src/b.ts",
            ),
        ]);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "finding-a");
        assert!(findings[0].claim.contains("Also observed in: src/b.ts"));
        assert_eq!(
            findings[0].discovered_by,
            vec!["session-finding-a", "session-finding-b"]
        );
    }

    fn test_session(id: &str) -> RunSessionParams {
        RunSessionParams {
            id: id.to_string(),
            role: Role::Generalist,
            objective: "Review the change.".to_string(),
            cwd: None,
            model_profile_id: None,
            instructions: Vec::new(),
            tool_grants: Vec::new(),
            budget: None,
        }
    }

    fn test_finding(id: &str, title: &str, claim: &str, path: &str) -> RunnerFinding {
        RunnerFinding {
            id: id.to_string(),
            title: title.to_string(),
            claim: claim.to_string(),
            evidence_count: 0,
            publishable: true,
            severity: Some("warning".to_string()),
            confidence: Some(0.72),
            validation_status: Some("validated".to_string()),
            evidence: Vec::new(),
            discovered_by: vec![format!("session-{id}")],
            validated_by: Vec::new(),
            challenged_by: Vec::new(),
            location: Some(RunnerFindingLocation {
                path: path.to_string(),
                revision: None,
                start_line: Some(1),
                end_line: Some(2),
                start_column: None,
                end_column: None,
                side: None,
                provider_anchor: None,
            }),
        }
    }

    #[test]
    fn heartbeat_callback_can_cancel_active_run() {
        struct HeartbeatTransport {
            requests: Mutex<Vec<(String, Value)>>,
        }

        impl RunnerCallbackTransport for HeartbeatTransport {
            fn request(&self, method: &str, params: Value) -> Result<Value> {
                self.requests
                    .lock()
                    .expect("heartbeat requests poisoned")
                    .push((method.to_string(), params));
                Ok(json!({ "continueRun": false }))
            }

            fn notify(&self, _method: &str, _params: Value) -> Result<()> {
                Ok(())
            }

            fn respond(&self, _response: &crate::runner::JsonRpcResponse) -> Result<()> {
                Ok(())
            }
        }

        let transport = Arc::new(HeartbeatTransport {
            requests: Mutex::new(Vec::new()),
        });
        let cancel = CancellationToken::new();
        let config = RunHeartbeatConfigParams {
            callback: true,
            interval_ms: Some(1),
            lease_seconds: Some(30),
        };

        let guard = start_heartbeat(
            "review-heartbeat",
            Some(&config),
            Some(transport.clone()),
            cancel.clone(),
        )
        .expect("heartbeat starts");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cancel.is_cancelled() {
            assert!(Instant::now() < deadline, "heartbeat did not cancel run");
            thread::sleep(Duration::from_millis(5));
        }
        guard.stop();

        let requests = transport
            .requests
            .lock()
            .expect("heartbeat requests poisoned");
        assert_eq!(requests[0].0, "run.heartbeat");
        assert_eq!(requests[0].1["runId"], "review-heartbeat");
        assert_eq!(requests[0].1["leaseSeconds"], 30);
    }
}
