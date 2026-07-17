use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context_engine::{
    ContextEngineConfig, ContextLearningApproval, CrossRepoContractCandidate,
};
use crate::review_sessions::{HostConfiguration, ReviewWorkerRun, WebhookReviewOptions};
use crate::review_sources::ReviewSource;
use crate::reviewer_kernel::kernel_types::ModelResponseFormat;
use crate::reviewer_kernel::review_contract::Role;

use super::results::RunnerArtifactView;

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
    pub budget: Option<RunAgentBudgetParams>,
    #[serde(default)]
    pub limits: Option<RunLimitParams>,
    #[serde(default)]
    pub model: Option<RunModelParams>,
    #[serde(default)]
    pub tools: Vec<RunToolParams>,
    #[serde(default)]
    pub heartbeat: Option<RunHeartbeatConfigParams>,
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
    pub response_format: Option<ModelResponseFormat>,
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
    pub max_child_sessions: Option<usize>,
    #[serde(default)]
    pub max_file_bytes: Option<usize>,
    #[serde(default)]
    pub max_search_matches: Option<usize>,
    #[serde(default)]
    pub orchestrator_model_profile_id: Option<String>,
    #[serde(default)]
    pub search_model_profile_id: Option<String>,
    #[serde(default)]
    pub explore_model_profile_id: Option<String>,
    #[serde(default)]
    pub validator_model_profile_id: Option<String>,
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
    pub host_metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub cross_repo_contracts: Vec<CrossRepoContractCandidate>,
    #[serde(default)]
    pub allowed_cross_repo_resources: Vec<String>,
    #[serde(default)]
    pub config: Option<ContextEngineConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerContextLearningApprovalParams {
    pub snapshot_id: crate::reviewer_kernel::kernel_types::SnapshotId,
    #[serde(flatten)]
    pub approval: ContextLearningApproval,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookHandleParams {
    #[serde(default)]
    pub project_id: Option<String>,
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
pub struct RunReleaseResult {
    pub run_id: String,
    pub released: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerDebugStateResult {
    pub reports: usize,
    pub active_runs: usize,
    pub run_threads: usize,
    pub finished_run_threads: usize,
    pub context_engines: usize,
    pub retained_redacted_artifacts: usize,
    pub retained_raw_artifacts: usize,
    pub retained_artifact_bytes: usize,
    pub snapshot_readers: usize,
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
