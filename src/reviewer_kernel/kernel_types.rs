use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::reviewer_kernel::review_contract::{
    AgentBudget, Role, TokenUsage, ToolCounts, ToolName,
};

pub const CONCURRENT_CONTRACT_VERSION: u16 = 1;
pub const REDACTION_POLICY_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("resource limit exceeded: {kind}")]
    LimitExceeded { kind: &'static str },
    #[error("operation timed out")]
    Timeout,
    #[error("operation cancelled")]
    Cancelled,
    #[error("provider error: {status:?}")]
    Provider {
        status: Option<u16>,
        retryable: bool,
    },
    #[error("provider error: {status:?}: {message}")]
    ProviderMessage {
        status: Option<u16>,
        retryable: bool,
        message: String,
    },
    #[error("repository access denied")]
    RepoAccessDenied,
    #[error("repository unavailable: {0}")]
    RepoUnavailable(String),
    #[error("snapshot content changed after capture: {path}")]
    SnapshotStale { path: String },
    #[error("internal invariant violation: {0}")]
    Invariant(&'static str),
}

pub type RuntimeResult<T> = Result<T, RuntimeError>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub String);

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStoragePolicy {
    pub mode: SnapshotStorageMode,
    pub max_captured_text_bytes: usize,
    #[serde(skip, default)]
    remote_object_store: Option<Arc<dyn SnapshotObjectStore>>,
}

impl SnapshotStoragePolicy {
    pub fn memory(max_captured_text_bytes: usize) -> Self {
        Self {
            mode: SnapshotStorageMode::Memory,
            max_captured_text_bytes,
            remote_object_store: None,
        }
    }

    pub fn content_addressed_directory(
        root: impl Into<PathBuf>,
        max_captured_text_bytes: usize,
    ) -> Self {
        Self {
            mode: SnapshotStorageMode::ContentAddressedDirectory { root: root.into() },
            max_captured_text_bytes,
            remote_object_store: None,
        }
    }

    pub fn remote_object_store(
        base_uri: impl Into<String>,
        max_captured_text_bytes: usize,
        object_store: Arc<dyn SnapshotObjectStore>,
    ) -> RuntimeResult<Self> {
        Ok(Self {
            mode: SnapshotStorageMode::RemoteObjectStore {
                base_uri: normalize_remote_object_base_uri(base_uri.into())?,
            },
            max_captured_text_bytes,
            remote_object_store: Some(object_store),
        })
    }

    pub(crate) fn remote_store(&self) -> Option<Arc<dyn SnapshotObjectStore>> {
        self.remote_object_store.clone()
    }
}

impl std::fmt::Debug for SnapshotStoragePolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotStoragePolicy")
            .field("mode", &self.mode)
            .field("max_captured_text_bytes", &self.max_captured_text_bytes)
            .field(
                "has_remote_object_store",
                &self.remote_object_store.is_some(),
            )
            .finish()
    }
}

impl PartialEq for SnapshotStoragePolicy {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode && self.max_captured_text_bytes == other.max_captured_text_bytes
    }
}

impl Eq for SnapshotStoragePolicy {}

pub trait SnapshotObjectStore: Send + Sync {
    fn put_snapshot_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()>;

    fn read_snapshot_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>>;

    fn remove_snapshot_object(&self, uri: &str) -> RuntimeResult<bool>;
}

impl Default for SnapshotStoragePolicy {
    fn default() -> Self {
        Self::memory(64 * 1024 * 1024)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStorageMode {
    Memory,
    ContentAddressedDirectory { root: PathBuf },
    RemoteObjectStore { base_uri: String },
}

fn normalize_remote_object_base_uri(base_uri: String) -> RuntimeResult<String> {
    let normalized = base_uri.trim_end_matches('/').to_string();
    if normalized.is_empty()
        || !normalized.contains("://")
        || normalized.starts_with("file://")
        || normalized.chars().any(char::is_whitespace)
    {
        return Err(RuntimeError::InvalidInput(
            "remote snapshot object store requires a non-file URI base".to_string(),
        ));
    }
    Ok(normalized)
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCaptureStatus {
    Captured,
    SkippedMemoryLimit,
    SkippedUnreadable,
    NotTextCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactKey(pub String);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(pub String);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolId(String);

impl ToolId {
    pub fn parse(input: &str) -> RuntimeResult<Self> {
        if input.is_empty() || input.len() > 64 {
            return Err(RuntimeError::InvalidInput(
                "invalid tool id length".to_string(),
            ));
        }
        if !input
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(RuntimeError::InvalidInput("invalid tool id".to_string()));
        }
        Ok(Self(input.to_string()))
    }

    pub(crate) fn from_builtin(tool: ToolName) -> Self {
        Self(tool.as_str().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn as_builtin(&self) -> Option<ToolName> {
        match self.0.as_str() {
            "list_changed_files" => Some(ToolName::ListChangedFiles),
            "read_diff" => Some(ToolName::ReadDiff),
            "list_files" => Some(ToolName::ListFiles),
            "read_file" => Some(ToolName::ReadFile),
            "read_file_range" => Some(ToolName::ReadFileRange),
            "read_base_file" => Some(ToolName::ReadBaseFile),
            "read_head_file" => Some(ToolName::ReadHeadFile),
            "search_text" => Some(ToolName::SearchText),
            "find_related_files" => Some(ToolName::FindRelatedFiles),
            "find_tests_for_file" => Some(ToolName::FindTestsForFile),
            "list_imports" => Some(ToolName::ListImports),
            _ => None,
        }
    }
}

impl From<ToolName> for ToolId {
    fn from(value: ToolName) -> Self {
        Self::from_builtin(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolProviderId(String);

impl ToolProviderId {
    pub fn parse(input: &str) -> RuntimeResult<Self> {
        if input.is_empty() || input.len() > 64 {
            return Err(RuntimeError::InvalidInput(
                "invalid tool provider id length".to_string(),
            ));
        }
        if !input
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(RuntimeError::InvalidInput(
                "invalid tool provider id".to_string(),
            ));
        }
        Ok(Self(input.to_string()))
    }

    pub fn builtin_review() -> Self {
        Self("builtin_review".to_string())
    }

    pub fn in_process() -> Self {
        Self("in_process".to_string())
    }

    pub fn runtime() -> Self {
        Self("runtime".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderResourceId(String);

impl ProviderResourceId {
    pub fn parse(input: &str) -> RuntimeResult<Self> {
        if input.is_empty() || input.len() > 128 {
            return Err(RuntimeError::InvalidInput(
                "invalid provider resource id length".to_string(),
            ));
        }
        if !input.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')
        }) {
            return Err(RuntimeError::InvalidInput(
                "invalid provider resource id".to_string(),
            ));
        }
        Ok(Self(input.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderResourceScope {
    pub provider_id: ToolProviderId,
    pub resource_id: ProviderResourceId,
}

impl ProviderResourceScope {
    pub fn new(provider_id: ToolProviderId, resource_id: ProviderResourceId) -> Self {
        Self {
            provider_id,
            resource_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolMetricKey(String);

impl ToolMetricKey {
    pub fn new(provider_id: &ToolProviderId, tool_id: &ToolId) -> Self {
        Self(format!("{}:{}", provider_id.as_str(), tool_id.as_str()))
    }

    pub fn builtin(tool: ToolName) -> Self {
        Self::new(&ToolProviderId::builtin_review(), &ToolId::from(tool))
    }

    pub fn in_process(tool_id: &ToolId) -> Self {
        Self::new(&ToolProviderId::in_process(), tool_id)
    }

    pub(crate) fn from_encoded(value: String) -> Self {
        Self(value)
    }

    pub fn provider_id(&self) -> Option<ToolProviderId> {
        self.0
            .split_once(':')
            .and_then(|(provider, _)| ToolProviderId::parse(provider).ok())
    }

    pub fn tool_id(&self) -> Option<ToolId> {
        self.0
            .split_once(':')
            .and_then(|(_, tool)| ToolId::parse(tool).ok())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoPath(PathBuf);

impl RepoPath {
    pub fn parse(input: &str) -> RuntimeResult<Self> {
        if input.is_empty() {
            return Err(RuntimeError::InvalidInput("repo path is empty".to_string()));
        }
        if input.as_bytes().contains(&0) {
            return Err(RuntimeError::RepoAccessDenied);
        }
        if input.contains(':') {
            return Err(RuntimeError::RepoAccessDenied);
        }
        let path = Path::new(input);
        let mut clean = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => clean.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(RuntimeError::RepoAccessDenied)
                }
            }
        }
        if clean.as_os_str().is_empty() {
            return Err(RuntimeError::RepoAccessDenied);
        }
        Ok(Self(clean))
    }

    pub fn from_path(path: PathBuf) -> RuntimeResult<Self> {
        let text = path
            .to_str()
            .ok_or_else(|| RuntimeError::InvalidInput("repo path is not UTF-8".to_string()))?;
        Self::parse(text)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScopeKey(String);

impl ScopeKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsScope {
    pub cwd: Option<RepoPath>,
    pub allowed_roots: Vec<RepoPath>,
    pub candidate_set_hash: String,
}

impl FsScope {
    pub fn repo_root() -> Self {
        Self {
            cwd: None,
            allowed_roots: Vec::new(),
            candidate_set_hash: "root".to_string(),
        }
    }

    pub fn subtree(path: RepoPath) -> Self {
        let candidate_set_hash = stable_id(&[&path.display()]);
        Self {
            cwd: Some(path.clone()),
            allowed_roots: vec![path],
            candidate_set_hash,
        }
    }

    pub fn allows(&self, path: &RepoPath) -> bool {
        if let Some(cwd) = &self.cwd {
            if path.as_path() != cwd.as_path() && !path.as_path().starts_with(cwd.as_path()) {
                return false;
            }
        }
        if self.allowed_roots.is_empty() {
            return true;
        }
        self.allowed_roots.iter().any(|root| {
            path.as_path() == root.as_path() || path.as_path().starts_with(root.as_path())
        })
    }

    pub fn scope_key(&self, snapshot_id: &SnapshotId) -> ScopeKey {
        let mut owned_parts = vec![snapshot_id.0.clone(), self.candidate_set_hash.clone()];
        if let Some(cwd) = &self.cwd {
            owned_parts.push(cwd.display());
        }
        for root in &self.allowed_roots {
            owned_parts.push(root.display());
        }
        let parts = owned_parts.iter().map(String::as_str).collect::<Vec<_>>();
        ScopeKey(stable_id(&parts))
    }
}

#[derive(Debug, Copy, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolEffects {
    #[serde(default)]
    pub repo_read: bool,
    #[serde(default)]
    pub artifact_read: bool,
    #[serde(default)]
    pub artifact_write: bool,
    #[serde(default)]
    pub network_read: bool,
    #[serde(default)]
    pub host_read: bool,
    #[serde(default)]
    pub scratch_read: bool,
    #[serde(default)]
    pub scratch_write: bool,
    #[serde(default)]
    pub external_side_effect: bool,
}

impl ToolEffects {
    pub fn review_read_only() -> Self {
        Self {
            repo_read: true,
            artifact_read: true,
            artifact_write: true,
            network_read: false,
            host_read: false,
            scratch_read: false,
            scratch_write: false,
            external_side_effect: false,
        }
    }

    pub fn custom_read_only() -> Self {
        Self {
            repo_read: true,
            artifact_read: true,
            artifact_write: true,
            network_read: false,
            host_read: true,
            scratch_read: false,
            scratch_write: false,
            external_side_effect: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolGrant {
    pub allow: bool,
    pub max_calls: Option<u32>,
    pub effects_allowed: ToolEffects,
}

impl ToolGrant {
    pub fn allow_review_read_only() -> Self {
        Self {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::review_read_only(),
        }
    }

    pub fn allow_custom_read_only() -> Self {
        Self {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::custom_read_only(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySet {
    pub fs_scope: FsScope,
    pub tool_grants: BTreeMap<ToolId, ToolGrant>,
    #[serde(default)]
    pub artifact_access: ArtifactAccessPolicy,
    #[serde(default)]
    pub model_output: ModelOutputPolicy,
    #[serde(default)]
    pub tool_input: ToolInputPolicy,
    #[serde(default)]
    pub runtime_authority: RuntimeAuthorityPolicy,
}

impl CapabilitySet {
    pub fn review_read_only() -> Self {
        let mut capabilities = Self {
            fs_scope: FsScope::repo_root(),
            tool_grants: BTreeMap::new(),
            artifact_access: ArtifactAccessPolicy::review_read_only(),
            model_output: ModelOutputPolicy::review_read_only(),
            tool_input: ToolInputPolicy::review_read_only(),
            runtime_authority: RuntimeAuthorityPolicy::review_read_only(),
        };
        for &tool in ToolName::review_read_only_tools() {
            capabilities.grant(ToolId::from(tool), ToolGrant::allow_review_read_only());
        }
        capabilities
    }

    pub fn empty_review_policy(fs_scope: FsScope) -> Self {
        Self {
            fs_scope,
            tool_grants: BTreeMap::new(),
            artifact_access: ArtifactAccessPolicy::review_read_only(),
            model_output: ModelOutputPolicy::review_read_only(),
            tool_input: ToolInputPolicy::review_read_only(),
            runtime_authority: RuntimeAuthorityPolicy::review_read_only(),
        }
    }

    pub fn with_fs_scope(mut self, fs_scope: FsScope) -> Self {
        self.fs_scope = fs_scope;
        self
    }

    pub fn grant(&mut self, tool_id: ToolId, grant: ToolGrant) {
        self.tool_grants.insert(tool_id, grant);
    }

    pub fn grant_tool(&mut self, tool_id: ToolId, grant: ToolGrant) {
        self.grant(tool_id, grant);
    }

    pub fn allows_tool(&self, tool_id: &ToolId) -> bool {
        self.tool_grants
            .get(tool_id)
            .map(|grant| grant.allow)
            .unwrap_or(false)
    }

    pub fn allow_tool(&self, tool_id: &ToolId) -> bool {
        self.allows_tool(tool_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactAccessPolicy {
    pub read_redacted: bool,
    pub read_raw: bool,
    pub write: bool,
    #[serde(default)]
    pub allowed_artifact_ids: Option<Vec<ArtifactId>>,
}

impl ArtifactAccessPolicy {
    pub fn review_read_only() -> Self {
        Self {
            read_redacted: true,
            read_raw: false,
            write: true,
            allowed_artifact_ids: None,
        }
    }

    pub fn deny_all() -> Self {
        Self {
            read_redacted: false,
            read_raw: false,
            write: false,
            allowed_artifact_ids: None,
        }
    }

    pub fn allow_raw() -> Self {
        Self {
            read_redacted: true,
            read_raw: true,
            write: true,
            allowed_artifact_ids: None,
        }
    }

    pub fn scoped_to_artifacts(mut self, artifact_ids: Vec<ArtifactId>) -> Self {
        self.allowed_artifact_ids = Some(artifact_ids);
        self
    }

    pub fn allows_artifact(&self, artifact_id: &ArtifactId) -> bool {
        match &self.allowed_artifact_ids {
            Some(allowed) => allowed.iter().any(|allowed_id| allowed_id == artifact_id),
            None => true,
        }
    }
}

impl Default for ArtifactAccessPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelOutputPolicy {
    pub include_tool_data: bool,
    pub include_artifact_refs: bool,
    pub max_tool_data_bytes: usize,
}

impl ModelOutputPolicy {
    pub fn review_read_only() -> Self {
        Self {
            include_tool_data: true,
            include_artifact_refs: true,
            max_tool_data_bytes: 16 * 1024,
        }
    }

    pub fn metadata_only() -> Self {
        Self {
            include_tool_data: false,
            include_artifact_refs: false,
            max_tool_data_bytes: 0,
        }
    }
}

impl Default for ModelOutputPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolInputPolicy {
    pub max_argument_bytes: usize,
}

impl ToolInputPolicy {
    pub fn review_read_only() -> Self {
        Self {
            max_argument_bytes: 16 * 1024,
        }
    }
}

impl Default for ToolInputPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAuthorityPolicy {
    pub network_read: bool,
    pub host_read: bool,
    pub scratch_read: bool,
    pub scratch_write: bool,
    pub external_side_effect: bool,
    #[serde(default)]
    pub allowed_provider_ids: Option<Vec<ToolProviderId>>,
    #[serde(default)]
    pub allowed_provider_resources: Option<Vec<ProviderResourceScope>>,
}

impl RuntimeAuthorityPolicy {
    pub fn review_read_only() -> Self {
        Self {
            network_read: false,
            host_read: false,
            scratch_read: false,
            scratch_write: false,
            external_side_effect: false,
            allowed_provider_ids: None,
            allowed_provider_resources: None,
        }
    }

    pub fn trusted_host_read() -> Self {
        Self {
            host_read: true,
            ..Self::review_read_only()
        }
    }

    pub fn allow_all_trusted() -> Self {
        Self {
            network_read: true,
            host_read: true,
            scratch_read: true,
            scratch_write: true,
            external_side_effect: true,
            allowed_provider_ids: None,
            allowed_provider_resources: None,
        }
    }

    pub fn scoped_to_providers(mut self, provider_ids: Vec<ToolProviderId>) -> Self {
        self.allowed_provider_ids = Some(provider_ids);
        self
    }

    pub fn allows_provider(&self, provider_id: &ToolProviderId) -> bool {
        match &self.allowed_provider_ids {
            Some(allowed) => allowed.iter().any(|allowed_id| allowed_id == provider_id),
            None => true,
        }
    }

    pub fn scoped_to_provider_resources(mut self, resources: Vec<ProviderResourceScope>) -> Self {
        self.allowed_provider_resources = Some(resources);
        self
    }

    pub fn allows_provider_resource(
        &self,
        provider_id: &ToolProviderId,
        resource_id: &ProviderResourceId,
    ) -> bool {
        match &self.allowed_provider_resources {
            Some(allowed) => allowed.iter().any(|scope| {
                &scope.provider_id == provider_id && &scope.resource_id == resource_id
            }),
            None => true,
        }
    }

    pub fn allows_provider_resources(
        &self,
        provider_id: &ToolProviderId,
        resource_ids: &[ProviderResourceId],
    ) -> bool {
        resource_ids
            .iter()
            .all(|resource_id| self.allows_provider_resource(provider_id, resource_id))
    }
}

impl Default for RuntimeAuthorityPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionScope {
    pub id: SessionId,
    pub role: Role,
    pub objective: String,
    pub instructions: Vec<SessionInstruction>,
    pub snapshot_id: Option<SnapshotId>,
    pub model_profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ModelResponseFormat>,
    pub capabilities: CapabilitySet,
    pub budget: AgentBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelResponseFormat {
    pub name: String,
    pub schema: Value,
    #[serde(default = "default_strict_response_format")]
    pub strict: bool,
}

impl ModelResponseFormat {
    pub fn json_schema(name: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            schema,
            strict: true,
        }
    }
}

fn default_strict_response_format() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInstruction {
    pub kind: String,
    pub text: String,
    pub trusted: bool,
}

impl SessionScope {
    pub fn review_read_only(
        id: SessionId,
        role: Role,
        objective: impl Into<String>,
        budget: AgentBudget,
    ) -> Self {
        Self {
            id,
            role,
            objective: objective.into(),
            instructions: Vec::new(),
            snapshot_id: None,
            model_profile_id: None,
            response_format: None,
            capabilities: CapabilitySet::review_read_only(),
            budget,
        }
    }

    pub fn with_snapshot_id(mut self, snapshot_id: SnapshotId) -> Self {
        self.snapshot_id = Some(snapshot_id);
        self
    }

    pub fn with_response_format(mut self, response_format: ModelResponseFormat) -> Self {
        self.response_format = Some(response_format);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationItem {
    System {
        content: String,
    },
    User {
        content: String,
    },
    AssistantText {
        content: String,
    },
    AssistantToolCalls {
        calls: Vec<ModelToolCall>,
    },
    ToolResult {
        call_id: ToolCallId,
        name: ToolId,
        content: Box<ToolResultEnvelope>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelToolCall {
    pub call_id: ToolCallId,
    pub index: usize,
    pub name: ToolId,
    pub raw_arguments: String,
}

impl ModelToolCall {
    pub(crate) fn redacted_argument_summary(&self) -> Value {
        redacted_tool_argument_summary(&self.name, &self.raw_arguments)
    }
}

fn redacted_tool_argument_summary(tool_id: &ToolId, raw_arguments: &str) -> Value {
    let Ok(arguments) = serde_json::from_str::<Value>(raw_arguments) else {
        return serde_json::json!({ "parseable": false });
    };
    match tool_id.as_builtin() {
        Some(ToolName::ListChangedFiles | ToolName::ReadDiff | ToolName::ListFiles) => {
            serde_json::json!({ "parseable": true })
        }
        Some(
            ToolName::ReadFile
            | ToolName::ReadBaseFile
            | ToolName::ReadHeadFile
            | ToolName::FindRelatedFiles
            | ToolName::FindTestsForFile
            | ToolName::ListImports,
        ) => serde_json::json!({
            "parseable": true,
            "path": arguments
                .get("path")
                .and_then(Value::as_str)
                .map(compact_trace_string),
        }),
        Some(ToolName::ReadFileRange) => serde_json::json!({
            "parseable": true,
            "path": arguments
                .get("path")
                .and_then(Value::as_str)
                .map(compact_trace_string),
            "startLine": arguments.get("start_line").or_else(|| arguments.get("startLine")).cloned(),
            "endLine": arguments.get("end_line").or_else(|| arguments.get("endLine")).cloned(),
        }),
        Some(ToolName::SearchText) => serde_json::json!({
            "parseable": true,
            "query": arguments
                .get("query")
                .and_then(Value::as_str)
                .map(compact_trace_string),
        }),
        None => {
            let keys = arguments
                .as_object()
                .map(|object| object.keys().take(20).cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            serde_json::json!({
                "parseable": true,
                "keys": keys,
            })
        }
    }
}

fn compact_trace_string(value: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= MAX_CHARS {
            output.push_str("...");
            return output;
        }
        output.push(ch);
    }
    output
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelTurn {
    Text {
        content: String,
        usage: TokenUsage,
    },
    ToolCalls {
        calls: Vec<ModelToolCall>,
        usage: TokenUsage,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ToolInvocation {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) call_id: ToolCallId,
    pub(crate) tool_id: ToolId,
    pub(crate) builtin_name: Option<ToolName>,
    pub(crate) input_bytes: usize,
    pub(crate) args: ToolArgs,
    pub(crate) capabilities: CapabilitySet,
    pub(crate) scope_key: ScopeKey,
    pub(crate) assigned_changed_files: Vec<RepoPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArgs {
    Empty,
    ReadFile {
        path: RepoPath,
    },
    ReadFileRange {
        path: RepoPath,
        start_line: usize,
        end_line: usize,
    },
    SearchText {
        query: String,
    },
    Raw(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEnvelope {
    pub ok: bool,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolId,
    pub provider_id: ToolProviderId,
    pub snapshot_id: SnapshotId,
    pub artifact_id: Option<ArtifactId>,
    pub cache: CacheInfo,
    pub limits: LimitInfo,
    pub data: Option<Value>,
    pub error: Option<ToolErrorInfo>,
}

impl ToolResultEnvelope {
    pub(crate) fn for_call(
        &self,
        call_id: ToolCallId,
        tool_name: ToolId,
        cache_status: CacheStatus,
    ) -> Self {
        let mut cloned = self.clone();
        cloned.tool_call_id = call_id;
        cloned.tool_name = tool_name;
        cloned.cache.status = cache_status;
        cloned
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheInfo {
    pub status: CacheStatus,
    pub key_hash: Option<String>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Hit,
    Miss,
    Deduped,
    NotCacheable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitInfo {
    pub truncated: bool,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub latency_ms: u64,
    pub queue_wait_ms: u64,
    pub searched_files: usize,
    pub skipped_files: usize,
    pub bytes_scanned: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolErrorInfo {
    pub code: ToolErrorCode,
    pub message: String,
    pub retryable: bool,
    pub partial: bool,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    InvalidArgs,
    UnknownTool,
    ToolNotAllowed,
    PathDenied,
    NotFound,
    NotText,
    TooLarge,
    TooManyMatches,
    SnapshotStale,
    Timeout,
    Cancelled,
    BudgetExceeded,
    QueueFull,
    RepoUnavailable,
    RedactionFailed,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub evidence_id: EvidenceId,
    pub snapshot_id: SnapshotId,
    pub file_id: Option<FileId>,
    pub path: Option<RepoPath>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub snippet_hash: String,
    pub artifact_id: ArtifactId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeLimits {
    pub max_active_sessions: usize,
    pub max_model_concurrency_global: usize,
    pub max_model_concurrency_per_key: usize,
    #[serde(default = "default_max_model_turn_ms")]
    pub max_model_turn_ms: u64,
    /// Total attempts per model turn, including the first; `max_model_turn_ms`
    /// bounds each attempt separately, so a turn's worst-case wall time is
    /// `attempts * max_model_turn_ms` plus backoff.
    #[serde(default = "default_model_retry_max_attempts")]
    pub model_retry_max_attempts: usize,
    #[serde(default = "default_model_retry_base_delay_ms")]
    pub model_retry_base_delay_ms: u64,
    #[serde(default = "default_model_retry_max_delay_ms")]
    pub model_retry_max_delay_ms: u64,
    pub max_tool_calls_per_turn: usize,
    pub max_tool_parallelism_per_session: usize,
    pub max_tool_provider_concurrency_per_provider: usize,
    pub max_tool_provider_ms: u64,
    #[serde(default = "default_max_tool_output_bytes")]
    pub max_tool_output_bytes: usize,
    #[serde(default = "default_max_tool_artifact_bytes")]
    pub max_tool_artifact_bytes: usize,
    pub max_read_concurrency_global: usize,
    pub max_search_jobs_global: usize,
    pub max_search_queue_depth: usize,
    pub max_file_bytes_read: usize,
    pub max_file_bytes_search: usize,
    pub max_search_matches: usize,
    pub max_search_pattern_bytes: usize,
    pub file_content_cache_bytes: u64,
    pub search_result_cache_bytes: u64,
    pub search_threads: usize,
    #[serde(default)]
    pub max_child_sessions: Option<usize>,
    #[serde(default)]
    pub orchestrator_model_profile_id: Option<String>,
    #[serde(default)]
    pub search_model_profile_id: Option<String>,
    #[serde(default)]
    pub explore_model_profile_id: Option<String>,
    #[serde(default)]
    pub validator_model_profile_id: Option<String>,
}

impl RuntimeLimits {
    pub fn standard(sessions: usize, max_file_bytes: usize, max_search_matches: usize) -> Self {
        Self {
            max_active_sessions: sessions.max(1),
            max_model_concurrency_global: 16,
            max_model_concurrency_per_key: 4,
            max_model_turn_ms: default_max_model_turn_ms(),
            model_retry_max_attempts: default_model_retry_max_attempts(),
            model_retry_base_delay_ms: default_model_retry_base_delay_ms(),
            model_retry_max_delay_ms: default_model_retry_max_delay_ms(),
            max_tool_calls_per_turn: 8,
            max_tool_parallelism_per_session: 4,
            max_tool_provider_concurrency_per_provider: 8,
            max_tool_provider_ms: 30_000,
            max_tool_output_bytes: default_max_tool_output_bytes(),
            max_tool_artifact_bytes: default_max_tool_artifact_bytes(),
            max_read_concurrency_global: 32,
            max_search_jobs_global: 1,
            max_search_queue_depth: 128,
            max_file_bytes_read: max_file_bytes,
            max_file_bytes_search: max_file_bytes,
            max_search_matches,
            max_search_pattern_bytes: 512,
            file_content_cache_bytes: 32_000_000,
            search_result_cache_bytes: 16_000_000,
            search_threads: num_cpus::get().clamp(2, 8),
            max_child_sessions: None,
            orchestrator_model_profile_id: None,
            search_model_profile_id: None,
            explore_model_profile_id: None,
            validator_model_profile_id: None,
        }
    }
}

fn default_max_model_turn_ms() -> u64 {
    180_000
}

fn default_model_retry_max_attempts() -> usize {
    3
}

fn default_model_retry_base_delay_ms() -> u64 {
    500
}

fn default_model_retry_max_delay_ms() -> u64 {
    10_000
}

fn default_max_tool_output_bytes() -> usize {
    2 * 1024 * 1024
}

fn default_max_tool_artifact_bytes() -> usize {
    8 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RuntimeEvent {
    JobStarted {
        snapshot_id: SnapshotId,
    },
    SnapshotStarted {
        snapshot_id: SnapshotId,
    },
    ContextIndexStarted {
        snapshot_id: SnapshotId,
    },
    ContextIndexCompleted {
        snapshot_id: SnapshotId,
        index_id: String,
        evidence_count: usize,
        indexed_files: usize,
        skipped_files: usize,
        ms: u64,
    },
    ContextIndexFailed {
        snapshot_id: SnapshotId,
        message: String,
    },
    ContextPackStarted {
        session_id: Option<SessionId>,
        purpose: String,
    },
    ContextPackCompleted {
        pack_id: String,
        session_id: Option<SessionId>,
        purpose: String,
        evidence_count: usize,
        omitted_count: usize,
        used_tokens: usize,
        sufficiency: String,
        ms: u64,
    },
    ContextPackFailed {
        session_id: Option<SessionId>,
        purpose: String,
        message: String,
        ms: u64,
    },
    ContextQueryCompleted {
        session_id: Option<SessionId>,
        query_kind: String,
        result_count: usize,
        artifact_id: Option<ArtifactId>,
        ms: u64,
    },
    RepoManifestCompleted {
        files: usize,
        skipped: usize,
        bytes: u64,
        ms: u64,
    },
    SessionStarted {
        session_id: SessionId,
    },
    ModelStarted {
        session_id: SessionId,
        turn_id: TurnId,
    },
    AgentTrace {
        session_id: SessionId,
        turn_id: Option<TurnId>,
        trace_kind: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        details: Value,
    },
    ModelCompleted {
        session_id: SessionId,
        turn_id: TurnId,
        tool_call_count: usize,
    },
    ModelFailed {
        session_id: SessionId,
        turn_id: TurnId,
        attempt: usize,
        retrying: bool,
        message: String,
    },
    ToolBatchStarted {
        session_id: SessionId,
        turn_id: TurnId,
        count: usize,
    },
    ToolCallCompleted {
        call_id: ToolCallId,
        tool_name: ToolId,
        provider_id: ToolProviderId,
        cache_status: CacheStatus,
        output_bytes: usize,
        ok: bool,
        error_code: Option<ToolErrorCode>,
        error_message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    ToolCallDenied {
        call_id: ToolCallId,
        tool_name: ToolId,
        provider_id: ToolProviderId,
        error_code: ToolErrorCode,
        reason: String,
    },
    ArtifactCreated {
        artifact_id: ArtifactId,
        tool_call_id: ToolCallId,
        tool_name: ToolId,
        provider_id: ToolProviderId,
        bytes: usize,
        content_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    FindingRecorded {
        finding_id: String,
        session_id: SessionId,
        tool_call_id: ToolCallId,
    },
    SearchBatchCompleted {
        searched_files: usize,
        skipped_files: usize,
        bytes_scanned: usize,
        ms: u64,
    },
    SessionFinished {
        session_id: SessionId,
        status: String,
    },
    SnapshotFinished {
        snapshot_id: SnapshotId,
        sessions: usize,
        completed_sessions: usize,
    },
    JobFinished {
        status: String,
    },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEventContext {
    pub run_id: Option<String>,
    pub snapshot_id: Option<SnapshotId>,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub tool_call_id: Option<ToolCallId>,
    pub artifact_id: Option<ArtifactId>,
    pub finding_id: Option<String>,
}

impl RuntimeEventContext {
    pub fn from_event(event: &RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::JobStarted { snapshot_id }
            | RuntimeEvent::SnapshotStarted { snapshot_id }
            | RuntimeEvent::ContextIndexStarted { snapshot_id }
            | RuntimeEvent::ContextIndexCompleted { snapshot_id, .. }
            | RuntimeEvent::ContextIndexFailed { snapshot_id, .. }
            | RuntimeEvent::SnapshotFinished { snapshot_id, .. } => Self {
                snapshot_id: Some(snapshot_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ContextPackStarted { session_id, .. }
            | RuntimeEvent::ContextPackCompleted { session_id, .. }
            | RuntimeEvent::ContextPackFailed { session_id, .. }
            | RuntimeEvent::ContextQueryCompleted { session_id, .. } => Self {
                session_id: session_id.clone(),
                ..Self::default()
            },
            RuntimeEvent::RepoManifestCompleted { .. } | RuntimeEvent::JobFinished { .. } => {
                Self::default()
            }
            RuntimeEvent::SessionStarted { session_id }
            | RuntimeEvent::SessionFinished { session_id, .. } => Self {
                session_id: Some(session_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ModelStarted {
                session_id,
                turn_id,
            }
            | RuntimeEvent::AgentTrace {
                session_id,
                turn_id: Some(turn_id),
                ..
            }
            | RuntimeEvent::ModelCompleted {
                session_id,
                turn_id,
                ..
            }
            | RuntimeEvent::ModelFailed {
                session_id,
                turn_id,
                ..
            }
            | RuntimeEvent::ToolBatchStarted {
                session_id,
                turn_id,
                ..
            } => Self {
                session_id: Some(session_id.clone()),
                turn_id: Some(*turn_id),
                ..Self::default()
            },
            RuntimeEvent::AgentTrace {
                session_id,
                turn_id: None,
                ..
            } => Self {
                session_id: Some(session_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ToolCallCompleted { call_id, .. }
            | RuntimeEvent::ToolCallDenied { call_id, .. } => Self {
                tool_call_id: Some(call_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ArtifactCreated {
                artifact_id,
                tool_call_id,
                ..
            } => Self {
                tool_call_id: Some(tool_call_id.clone()),
                artifact_id: Some(artifact_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::FindingRecorded {
                finding_id,
                session_id,
                tool_call_id,
            } => Self {
                session_id: Some(session_id.clone()),
                tool_call_id: Some(tool_call_id.clone()),
                finding_id: Some(finding_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::SearchBatchCompleted { .. } => Self::default(),
        }
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_default_snapshot_id(mut self, snapshot_id: SnapshotId) -> Self {
        if self.snapshot_id.is_none() {
            self.snapshot_id = Some(snapshot_id);
        }
        self
    }
}

pub trait RuntimeEventSink: Send + Sync {
    fn emit(&self, event: RuntimeEvent);

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let _ = context;
        self.emit(event);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEventRecord {
    pub seq: u64,
    pub timestamp_utc: String,
    pub context: RuntimeEventContext,
    pub event: RuntimeEvent,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConcurrentCounters {
    pub search_scans: usize,
    pub search_dedupe_waiters: usize,
    pub search_cache_hits: usize,
    pub read_cache_hits: usize,
    pub read_file_reads: usize,
    pub tool_errors: usize,
    pub artifact_cache_hits: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolMetricsSnapshot {
    pub calls: usize,
    pub successes: usize,
    pub errors: usize,
    pub cache_hits: usize,
    pub deduped: usize,
    pub timeouts: usize,
    pub cancellations: usize,
    pub artifacts: usize,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub latency_ms: u64,
    pub max_latency_ms: u64,
    pub queue_wait_ms: u64,
    pub max_queue_wait_ms: u64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProviderHealthState {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProviderHealthSnapshot {
    pub provider_id: ToolProviderId,
    pub state: ToolProviderHealthState,
    pub calls: usize,
    pub errors: usize,
    pub timeouts: usize,
    pub cancellations: usize,
    pub consecutive_errors: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetricsSnapshot {
    pub calls: usize,
    pub successes: usize,
    pub errors: usize,
    pub retries: usize,
    pub costed_calls: usize,
    pub unpriced_calls: usize,
    pub latency_ms: u64,
    pub max_latency_ms: u64,
    pub estimated_input_cost_micro_usd: u64,
    pub estimated_output_cost_micro_usd: u64,
    pub estimated_total_cost_micro_usd: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
}

#[derive(Debug, Default, Copy, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostEstimate {
    pub input_cost_micro_usd: u64,
    pub output_cost_micro_usd: u64,
    pub total_cost_micro_usd: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactView {
    pub artifact_id: ArtifactId,
    pub bytes: usize,
    pub content_hash: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompletionDiagnostic {
    pub session_id: String,
    pub completed: bool,
    pub completion_kind: Option<String>,
    pub completion_summary: Option<String>,
    pub saw_diff: bool,
    pub saw_file: bool,
    pub saw_search: bool,
    pub model_calls: usize,
    pub tool_counts: ToolCounts,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMetricsSnapshot {
    pub snapshot_id: SnapshotId,
    pub sessions: usize,
    pub completed_sessions: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub artifacts: usize,
    pub artifact_bytes: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewQualityDiagnostics {
    pub contract_risk_units: usize,
    pub contract_seed_count: usize,
    pub contract_pack_count: usize,
    pub omitted_contract_pack_candidates: Vec<String>,
    pub selected_contract_packs: Vec<String>,
    pub contract_evidence_failures: usize,
    pub coverage_counts: BTreeMap<String, usize>,
    pub coverage_counts_by_lens: BTreeMap<String, BTreeMap<String, usize>>,
    pub high_risk_files_below_target: Vec<String>,
    pub challenge_status_counts: BTreeMap<String, usize>,
    pub sessions_run: usize,
    pub budgets_used: BTreeMap<String, usize>,
    pub explicit_caller_cap_sessions: usize,
    pub candidate_findings: usize,
    pub rescued_candidates: usize,
    pub rejected_candidates: usize,
    pub rejection_reasons: BTreeMap<String, usize>,
}

impl ReviewQualityDiagnostics {
    pub fn add(&mut self, other: Self) {
        self.contract_risk_units += other.contract_risk_units;
        self.contract_seed_count += other.contract_seed_count;
        self.contract_pack_count += other.contract_pack_count;
        self.omitted_contract_pack_candidates
            .extend(other.omitted_contract_pack_candidates);
        self.selected_contract_packs
            .extend(other.selected_contract_packs);
        self.contract_evidence_failures += other.contract_evidence_failures;
        merge_counts(&mut self.coverage_counts, other.coverage_counts);
        for (lens, counts) in other.coverage_counts_by_lens {
            merge_counts(
                self.coverage_counts_by_lens.entry(lens).or_default(),
                counts,
            );
        }
        self.high_risk_files_below_target
            .extend(other.high_risk_files_below_target);
        merge_counts(
            &mut self.challenge_status_counts,
            other.challenge_status_counts,
        );
        self.sessions_run += other.sessions_run;
        merge_counts(&mut self.budgets_used, other.budgets_used);
        self.explicit_caller_cap_sessions += other.explicit_caller_cap_sessions;
        self.candidate_findings += other.candidate_findings;
        self.rescued_candidates += other.rescued_candidates;
        self.rejected_candidates += other.rejected_candidates;
        for (reason, count) in other.rejection_reasons {
            *self.rejection_reasons.entry(reason).or_insert(0) += count;
        }
    }
}

impl Default for ReviewQualityDiagnostics {
    fn default() -> Self {
        Self {
            contract_risk_units: 0,
            contract_seed_count: 0,
            contract_pack_count: 0,
            omitted_contract_pack_candidates: Vec::new(),
            selected_contract_packs: Vec::new(),
            contract_evidence_failures: 0,
            coverage_counts: BTreeMap::new(),
            coverage_counts_by_lens: BTreeMap::new(),
            high_risk_files_below_target: Vec::new(),
            challenge_status_counts: BTreeMap::new(),
            sessions_run: 0,
            budgets_used: BTreeMap::new(),
            explicit_caller_cap_sessions: 0,
            candidate_findings: 0,
            rescued_candidates: 0,
            rejected_candidates: 0,
            rejection_reasons: BTreeMap::new(),
        }
    }
}

fn merge_counts(target: &mut BTreeMap<String, usize>, source: BTreeMap<String, usize>) {
    for (key, count) in source {
        *target.entry(key).or_insert(0) += count;
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConcurrentRunReport {
    pub runtime: &'static str,
    pub sessions: usize,
    pub completed_sessions: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub tool_counts: ToolCounts,
    pub findings: usize,
    pub publishable_findings: usize,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub artifacts: usize,
    pub artifact_bytes: usize,
    pub counters: ConcurrentCounters,
    pub tool_metrics: BTreeMap<ToolMetricKey, ToolMetricsSnapshot>,
    pub provider_health: Vec<ToolProviderHealthSnapshot>,
    pub snapshot_metrics: Vec<SnapshotMetricsSnapshot>,
    pub model_metrics: ModelMetricsSnapshot,
    pub completion_diagnostics: Vec<SessionCompletionDiagnostic>,
    pub quality_diagnostics: ReviewQualityDiagnostics,
    pub benchmark_valid: bool,
    pub benchmark_failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComparisonReport {
    pub sessions: usize,
    pub sync: ConcurrentRunReport,
    pub concurrent: ConcurrentRunReport,
    pub speedup: f64,
    pub search_scan_reduction: f64,
    pub optimization_valid: bool,
    pub optimization_failures: Vec<String>,
}

pub(crate) fn stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}
