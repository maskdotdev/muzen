use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context_engine::ContextEngineConfig;
use crate::contracts::Role;
use crate::review_session::{
    HostConfiguration, ReviewSource, ReviewWorkerRun, WebhookReviewOptions,
};
use crate::runtime::contracts::ArtifactView;

fn default_role() -> Role {
    Role::Generalist
}

fn default_worker_max_sessions() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerHandshakeParams {
    pub protocol_version: String,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub client_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStartParams {
    #[serde(default)]
    pub protocol_version: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub repo: Option<PathBuf>,
    #[serde(default)]
    pub source: Option<ReviewSource>,
    #[serde(default)]
    pub source_provider: Option<RunSourceProviderParams>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub change: Option<RunChangeParams>,
    #[serde(default)]
    pub instructions: Vec<RunInstructionParams>,
    #[serde(default)]
    pub sessions: Vec<RunSessionParams>,
    #[serde(default)]
    pub limits: Option<RunLimitParams>,
    #[serde(default)]
    pub model: Option<RunModelParams>,
    #[serde(default)]
    pub tools: Vec<RunToolParams>,
    #[serde(default)]
    pub heartbeat: Option<RunHeartbeatConfigParams>,
    /// "planned_review" (default) or "direct_sessions".
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub context_engine: Option<ContextEngineConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunHeartbeatConfigParams {
    #[serde(default)]
    pub callback: bool,
    #[serde(default)]
    pub interval_ms: Option<u64>,
    #[serde(default)]
    pub lease_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSourceProviderParams {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub callback: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunChangeParams {
    pub kind: String,
    #[serde(default)]
    pub base_revision: Option<String>,
    #[serde(default)]
    pub start_revision: Option<String>,
    #[serde(default)]
    pub head_revision: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<RunChangeFileParams>,
    #[serde(default)]
    pub diff: Option<String>,
    #[serde(default)]
    pub review_target: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunChangeFileParams {
    pub path: String,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInstructionParams {
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub trusted: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunModelParams {
    #[serde(default)]
    pub callback: bool,
    #[serde(default)]
    pub default_model_profile_id: Option<String>,
    #[serde(default)]
    pub model_profiles: Vec<RunModelProfileParams>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunModelProfileParams {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub credential: Option<RunModelCredentialParams>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_protocol: Option<String>,
    #[serde(default)]
    pub max_input_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunModelCredentialParams {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub secret_ref: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunToolParams {
    pub id: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub effects: Vec<String>,
    #[serde(default)]
    pub cacheable: bool,
    #[serde(default)]
    pub provider_resources: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSessionParams {
    pub id: String,
    #[serde(default = "default_role")]
    pub role: Role,
    pub objective: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model_profile_id: Option<String>,
    #[serde(default)]
    pub instructions: Vec<RunInstructionParams>,
    #[serde(default)]
    pub tool_grants: Vec<String>,
    #[serde(default)]
    pub budget: Option<RunAgentBudgetParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSecretResolveParams {
    pub protocol_version: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSecretResolveResult {
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAgentBudgetParams {
    pub max_turns: usize,
    pub max_tool_calls: usize,
    pub max_prompt_tokens: u64,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLimitParams {
    #[serde(default)]
    pub max_active_sessions: Option<usize>,
    #[serde(default)]
    pub max_file_bytes: Option<usize>,
    #[serde(default)]
    pub max_search_matches: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLookupParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactReadParams {
    pub run_id: String,
    pub artifact_id: String,
    #[serde(default)]
    pub view: RunnerArtifactView,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactExportParams {
    pub run_id: String,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub view: RunnerArtifactView,
    #[serde(default)]
    pub max_artifacts: Option<usize>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReadTextParams {
    pub run_id: String,
    #[serde(default)]
    pub snapshot_id: Option<String>,
    pub path: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerContextIndexParams {
    pub repo: PathBuf,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub config: Option<ContextEngineConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookHandleParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: String,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub options: WebhookReviewOptions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunOnceParams {
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default = "default_worker_max_sessions")]
    pub max_sessions: usize,
    #[serde(default)]
    pub host_config: HostConfiguration,
}

impl WorkerRunOnceParams {
    pub fn worker_id(&self) -> &str {
        self.worker_id.as_deref().unwrap_or("worker-stdio")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunOnceResult {
    pub worker_id: String,
    pub claimed: usize,
    pub completed: usize,
    pub retried: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl WorkerRunOnceResult {
    pub fn from_run(worker_id: impl Into<String>, run: ReviewWorkerRun) -> Self {
        Self {
            worker_id: worker_id.into(),
            claimed: run.claimed,
            completed: run.completed,
            retried: run.retried,
            failed: run.failed,
            skipped: run.skipped,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunStatusResult {
    pub run_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunCancelResult {
    pub run_id: String,
    pub status: String,
    pub cancelled: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunHeartbeatParams {
    pub protocol_version: String,
    pub run_id: String,
    pub sequence: u64,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunHeartbeatResult {
    #[serde(default = "default_continue_run")]
    pub continue_run: bool,
}

fn default_continue_run() -> bool {
    true
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerFailureKind {
    SourceUnavailable,
    AuthFailed,
    ToolFailed,
    ModelFailed,
    BudgetExhausted,
    Cancelled,
    PolicyDenied,
    InternalError,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerRetryHint {
    Retryable,
    NotRetryable,
    RetryAfter,
    RequiresUserAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunFailedNotification {
    pub error: String,
    pub kind: String,
    pub failure_kind: RunnerFailureKind,
    pub retry_hint: RunnerRetryHint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl RunFailedNotification {
    pub(crate) fn from_runner_error(error: impl Into<String>) -> Self {
        let error = error.into();
        let (failure_kind, retry_hint) = classify_runner_failure(&error);
        Self {
            error,
            kind: "runner_error".to_string(),
            failure_kind,
            retry_hint,
            retry_after_seconds: None,
        }
    }
}

fn classify_runner_failure(message: &str) -> (RunnerFailureKind, RunnerRetryHint) {
    let lower = message.to_ascii_lowercase();
    if lower.contains("cancel") || lower.contains("abort") {
        return (RunnerFailureKind::Cancelled, RunnerRetryHint::NotRetryable);
    }
    if lower.contains("auth")
        || lower.contains("credential")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("permission")
        || lower.contains("access denied")
    {
        return (
            RunnerFailureKind::AuthFailed,
            RunnerRetryHint::RequiresUserAction,
        );
    }
    if lower.contains("budget") || lower.contains("limit exceeded") {
        return (
            RunnerFailureKind::BudgetExhausted,
            RunnerRetryHint::NotRetryable,
        );
    }
    if lower.contains("policy") || lower.contains("not allowed") || lower.contains("denied") {
        return (
            RunnerFailureKind::PolicyDenied,
            RunnerRetryHint::NotRetryable,
        );
    }
    if lower.contains("source.materialize")
        || lower.contains("sourceprovider")
        || lower.contains("materializ")
        || lower.contains("repository unavailable")
        || lower.contains("repo unavailable")
    {
        let retry_hint = if lower.contains("requires") || lower.contains("invalid") {
            RunnerRetryHint::RequiresUserAction
        } else {
            RunnerRetryHint::Retryable
        };
        return (RunnerFailureKind::SourceUnavailable, retry_hint);
    }
    if lower.contains("model.complete") || lower.contains("model") {
        return (RunnerFailureKind::ModelFailed, RunnerRetryHint::Retryable);
    }
    if lower.contains("tool.execute") || lower.contains("tool") {
        return (RunnerFailureKind::ToolFailed, RunnerRetryHint::Retryable);
    }
    (RunnerFailureKind::InternalError, RunnerRetryHint::Retryable)
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerArtifactView {
    Redacted,
    Raw,
}

impl Default for RunnerArtifactView {
    fn default() -> Self {
        Self::Redacted
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerArtifact {
    pub artifact_id: String,
    pub bytes: usize,
    pub content_hash: String,
    pub content: String,
}

impl RunnerArtifact {
    pub(crate) fn from_artifact_view(artifact: ArtifactView) -> Self {
        Self {
            artifact_id: artifact.artifact_id.0,
            bytes: artifact.bytes,
            content_hash: artifact.content_hash,
            content: artifact.content,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerArtifactReadResult {
    pub run_id: String,
    pub view: RunnerArtifactView,
    pub artifact: RunnerArtifact,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerArtifactExportResult {
    pub run_id: String,
    pub view: RunnerArtifactView,
    pub artifact_count: usize,
    pub total_bytes: usize,
    pub artifacts: Vec<RunnerArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSnapshotTextResult {
    pub run_id: String,
    pub snapshot_id: String,
    pub path: String,
    pub content_hash: String,
    pub bytes: usize,
    pub truncated: bool,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerRunResult {
    pub protocol_version: String,
    pub run_id: String,
    pub status: String,
    pub summary: RunnerRunSummary,
    #[serde(default)]
    pub file_reviews: Vec<RunnerFileReview>,
    pub findings: Vec<RunnerFinding>,
    pub snapshots: Vec<RunnerSnapshotSummary>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    /// Per-session final outputs; populated by direct-session runs only.
    #[serde(default)]
    pub session_outputs: Vec<RunnerSessionOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSessionOutput {
    pub session_id: String,
    pub status: String,
    pub completed: bool,
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerFileReview {
    pub path: String,
    pub verdict: String,
    pub summary: String,
    #[serde(default)]
    pub related_paths: Vec<String>,
    #[serde(default)]
    pub evidence_artifact_ids: Vec<String>,
    pub evidence_count: usize,
    pub session_id: String,
    pub unit_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerRunSummary {
    pub sessions: usize,
    pub completed_sessions: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub findings: usize,
    pub publishable_findings: usize,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    pub artifacts: usize,
    pub artifact_bytes: usize,
    pub snapshot_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerFinding {
    pub id: String,
    pub title: String,
    pub claim: String,
    pub evidence_count: usize,
    pub publishable: bool,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub validation_status: Option<String>,
    #[serde(default)]
    pub evidence: Vec<RunnerFindingEvidence>,
    #[serde(default)]
    pub discovered_by: Vec<String>,
    #[serde(default)]
    pub validated_by: Vec<String>,
    #[serde(default)]
    pub challenged_by: Vec<String>,
    #[serde(default)]
    pub location: Option<RunnerFindingLocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerFindingEvidence {
    pub evidence_id: String,
    pub artifact_id: String,
    pub kind: String,
    pub content_hash: String,
    pub producing_tool_call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerFindingLocation {
    pub path: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub start_column: Option<usize>,
    #[serde(default)]
    pub end_column: Option<usize>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub provider_anchor: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSnapshotSummary {
    pub snapshot_id: String,
    pub files: usize,
    pub changed_files: usize,
    pub captured_files: usize,
    pub captured_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerHandshakeResult {
    pub protocol_version: String,
    pub runner_name: String,
    pub runner_version: String,
    pub capabilities: RunnerCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerCapabilities {
    pub supported_methods: Vec<String>,
    pub planned_methods: Vec<String>,
    pub transports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerCheckResult {
    pub ok: bool,
    pub protocol_version: String,
    pub runner_name: String,
    pub runner_version: String,
    pub rust_package: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerProtocolSchema {
    pub schema_version: String,
    pub transport: String,
    pub requests: Vec<RunnerMethodSchema>,
    pub callbacks: Vec<RunnerMethodSchema>,
    pub notifications: Vec<RunnerMethodSchema>,
    pub definitions: Vec<RunnerPayloadSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerMethodSchema {
    pub method: String,
    pub direction: RunnerMessageDirection,
    pub status: RunnerMethodStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<RunnerPayloadRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RunnerPayloadRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerPayloadRef {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerPayloadSchema {
    pub name: String,
    pub shape: RunnerPayloadShape,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<RunnerPayloadFieldSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerPayloadFieldSchema {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPayloadShape {
    Object,
    Enum,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMessageDirection {
    SdkToRunner,
    RunnerToSdk,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMethodStatus {
    Implemented,
    Reserved,
}
