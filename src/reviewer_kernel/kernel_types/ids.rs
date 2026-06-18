use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{RuntimeError, RuntimeResult};
use crate::reviewer_kernel::review_contract::ToolName;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub String);

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

    pub(crate) fn from_encoded(value: String) -> Self {
        Self(value)
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
pub struct ScopeKey(pub(crate) String);

impl ScopeKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}
