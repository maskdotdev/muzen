use std::collections::BTreeMap;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    AgentName, ArtifactId, IdempotencyKey, ModelProfileId, SecretRef, SessionId, ToolProviderId,
};

pub type Metadata = BTreeMap<String, Value>;
pub type AgentPath = Vec<u32>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Artifact {
        #[serde(rename = "artifactId")]
        artifact_id: ArtifactId,
    },
    Image {
        #[serde(rename = "mediaType")]
        media_type: String,
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentInput {
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDefinition {
    pub name: AgentName,
    pub instructions: Vec<ContentBlock>,
    pub model: ModelProfileId,
    pub tools: Vec<ToolGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<AgentBudget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContract>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudget {
    pub max_turns: NonZeroU32,
    pub max_tool_calls: u32,
    pub max_prompt_tokens: NonZeroU64,
    pub max_output_tokens: NonZeroU64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputContract {
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelProviderKind {
    OpenaiCompatible,
    Anthropic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelProtocol {
    Responses,
    ChatCompletions,
    Messages,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProfile {
    pub id: ModelProfileId,
    pub provider: ModelProviderKind,
    pub protocol: ModelProtocol,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub credential: SecretRef,
    pub max_input_tokens: NonZeroU64,
    pub max_output_tokens: NonZeroU64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum HeaderValue {
    Literal(String),
    Secret(HeaderSecret),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HeaderSecret {
    pub secret: SecretRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolProvider {
    Builtin {
        id: ToolProviderId,
    },
    Client {
        id: ToolProviderId,
        #[serde(rename = "timeoutMs")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<NonZeroU64>,
    },
    McpHttp {
        id: ToolProviderId,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<SecretRef>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, HeaderValue>,
    },
}

impl ToolProvider {
    pub fn id(&self) -> &ToolProviderId {
        match self {
            Self::Builtin { id } | Self::Client { id, .. } | Self::McpHttp { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolGrant {
    pub provider: ToolProviderId,
    pub tool: String,
    pub effects: Vec<ToolEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls: Option<NonZeroU32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    WorkspaceRead,
    WorkspaceWrite,
    NetworkRead,
    NetworkWrite,
    ArtifactWrite,
    AgentSpawn,
    AgentMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSpec {
    pub base: WorkspaceBase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<WorkspaceLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceBase {
    Path {
        root: PathBuf,
    },
    Git {
        url: String,
        revision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<SecretRef>,
    },
    Snapshot {
        id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_bytes: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_base_bytes: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_overlay_bytes: Option<NonZeroU64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSpec {
    pub agent: AgentDefinition,
    pub models: Vec<ModelProfile>,
    pub tool_providers: Vec<ToolProvider>,
    pub workspace: WorkspaceSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_budget: Option<SessionBudget>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime_tokens: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime_tool_calls: Option<NonZeroU64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunSpec {
    pub roots: Vec<RunRoot>,
    pub limits: RunLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RunRoot {
    Existing(ExistingSessionRoot),
    New(NewSessionRoot),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExistingSessionRoot {
    pub session_id: SessionId,
    pub input: AgentInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewSessionRoot {
    pub session: SessionSpec,
    pub input: AgentInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunLimits {
    pub max_active_agents: NonZeroU32,
    pub max_agents: NonZeroU32,
    pub max_depth: u32,
    pub max_input_bytes: NonZeroU64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tool_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<NonZeroU64>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutSecretInput {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerToolCallInput {
    pub call_id: String,
    pub outcome: AnswerToolCallOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged, deny_unknown_fields)]
pub enum AnswerToolCallOutcome {
    Result { result: Value },
    Error { error: ClientToolCallError },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientToolCallError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl fmt::Debug for PutSecretInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PutSecretInput")
            .field("value", &"<redacted>")
            .field("idempotency_key", &self.idempotency_key)
            .finish()
    }
}
