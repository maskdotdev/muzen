use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::contracts::{AgentBudget, Role};
use crate::review_session::ReviewSource;
use crate::reviewer::{
    capabilities, ids::ToolId, paths, runtime_events::EventSink as RuntimeEventSink, ChangeKind,
    ChangeSpec, ChangedFileSpec, ChangedFileStatus, ReviewEventRecord, ReviewEventSink,
    ReviewRunLimits, ReviewRunSummary, ReviewSessionSpec, ReviewToolRegistry, Run, RunSpec,
    SnapshotPathPolicy, SnapshotSpec,
};
use crate::runtime::contracts::{ProviderResourceId, SessionInstruction, ToolEffects};

use super::adapters::{
    CallbackReviewModel, CallbackReviewTool, DeterministicRunnerModel, StreamingRunnerEventSink,
};
use super::materialize::materialize_run_source;
use super::stored::RunnerStoredRun;
use super::transport::RunnerCallbackTransport;
use super::types::{
    RunChangeParams, RunHeartbeatConfigParams, RunHeartbeatParams, RunHeartbeatResult,
    RunInstructionParams, RunSessionParams, RunStartParams, RunToolParams, RunnerFinding,
    RunnerFindingEvidence, RunnerRunResult, RunnerRunSummary, RunnerSnapshotSummary,
};
use super::RUNNER_PROTOCOL_VERSION;

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
    let changed_files = changed_file_specs(
        &repo_root,
        materialized.changed_files(),
        &target_path,
        params.change.as_ref(),
    );
    let change = runner_change_spec(
        params.source.as_ref(),
        params.change.as_ref(),
        changed_files,
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
    let max_active_sessions = params
        .limits
        .as_ref()
        .and_then(|limits| limits.max_active_sessions)
        .unwrap_or_else(|| params.sessions.len().max(1));
    let snapshot = SnapshotSpec::new(&repo_root, change).with_path_policy(
        SnapshotPathPolicy::standard(max_file_bytes, max_search_matches),
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
    let mut builder = Run::builder(spec);
    let use_callback_model = params.model.as_ref().is_some_and(|model| model.callback);
    if use_callback_model {
        let transport = transport
            .clone()
            .ok_or_else(|| anyhow::anyhow!("callback model requires interactive stdio"))?;
        builder = builder.review_model(Arc::new(CallbackReviewModel::new(
            run_id.clone(),
            transport,
        )));
    } else {
        builder = builder.review_model(Arc::new(DeterministicRunnerModel::new(
            target_path,
            "TODO|fn|class|export|pub".to_string(),
        )));
    }
    if !params.tools.is_empty() {
        let transport = transport
            .clone()
            .ok_or_else(|| anyhow::anyhow!("callback tools require interactive stdio"))?;
        let mut registry = ReviewToolRegistry::review_defaults()
            .map_err(|error| anyhow::anyhow!("failed to create review tool registry: {error}"))?;
        for tool in params.tools {
            let provider_resources = parse_provider_resources(&tool.provider_resources)?;
            let effects = parse_tool_effects(&tool.effects)?;
            registry
                .register_scoped_tool_with_effects(
                    &tool.id,
                    tool.description,
                    tool.parameters,
                    tool.cacheable,
                    provider_resources,
                    effects,
                    Arc::new(CallbackReviewTool::new(run_id.clone(), transport.clone())),
                )
                .map_err(|error| {
                    anyhow::anyhow!("failed to register SDK tool {}: {error}", tool.id)
                })?;
        }
        builder = builder.review_tool_registry(registry);
    }
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
    let summary = runner_summary_from_review(&report.summary);
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
    let findings = report
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
            location: None,
        })
        .collect();
    RunnerRunResult {
        protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
        run_id: report.run_id.clone(),
        status: summary_status(&summary),
        summary,
        findings,
        snapshots,
        metadata,
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
    target_path: &str,
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
    let files = if changed_files.is_empty() {
        vec![target_path.to_string()]
    } else {
        changed_files.to_vec()
    };
    files
        .into_iter()
        .filter(|path| repo_root.join(path).is_file())
        .map(|path| changed_file_spec(&path, None))
        .collect()
}

fn runner_change_spec(
    source: Option<&ReviewSource>,
    change: Option<&RunChangeParams>,
    changed_files: Vec<ChangedFileSpec>,
    run_id: &str,
) -> ChangeSpec {
    let Some(change) = change else {
        return ChangeSpec::local("sdk-run", "head", changed_files);
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
        inline_diff: change.diff.clone().filter(|value| !value.trim().is_empty()),
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
    for candidate in ["Cargo.toml", "package.json", "README.md", "pyproject.toml"] {
        if repo_root.join(candidate).is_file() {
            return Ok(candidate.to_string());
        }
    }
    find_first_text_candidate(repo_root)
        .ok_or_else(|| anyhow::anyhow!("repo has no obvious text file to review"))
}

fn find_first_text_candidate(repo_root: &Path) -> Option<String> {
    fn visit(root: &Path, dir: &Path, depth: usize) -> Option<String> {
        if depth > 4 {
            return None;
        }
        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | "dist" | "build" | ".next"
            ) {
                continue;
            }
            if path.is_file() && looks_textual(&path) {
                return path
                    .strip_prefix(root)
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned());
            }
            if path.is_dir() {
                if let Some(found) = visit(root, &path, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    visit(repo_root, repo_root, 0)
}

fn looks_textual(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension,
                "rs" | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "json"
                    | "toml"
                    | "md"
                    | "py"
                    | "go"
                    | "java"
                    | "kt"
                    | "rb"
                    | "php"
                    | "c"
                    | "h"
                    | "cpp"
                    | "hpp"
                    | "cs"
                    | "swift"
            )
        })
        .unwrap_or(false)
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
