use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextEngineMode {
    Disabled,
    SnapshotV0,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextEngineConfig {
    pub mode: ContextEngineMode,
    #[serde(default)]
    pub semantic: ContextSemanticConfig,
    pub max_indexed_files: usize,
    pub max_indexed_bytes: usize,
    pub max_evidence_items: usize,
    pub max_pack_tokens: usize,
    pub max_query_results: usize,
    pub include_repository_guidance: bool,
    pub include_host_context: bool,
    pub strict_evidence_required: bool,
}

impl ContextEngineConfig {
    pub fn disabled() -> Self {
        Self {
            mode: ContextEngineMode::Disabled,
            ..Self::snapshot_v0()
        }
    }

    pub fn snapshot_v0() -> Self {
        Self {
            mode: ContextEngineMode::SnapshotV0,
            semantic: ContextSemanticConfig::default(),
            max_indexed_files: 20_000,
            max_indexed_bytes: 64 * 1024 * 1024,
            max_evidence_items: 5_000,
            max_pack_tokens: 12_000,
            max_query_results: 120,
            include_repository_guidance: true,
            include_host_context: false,
            strict_evidence_required: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextSemanticConfig {
    pub mode: ContextSemanticMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ContextEmbeddingProviderKind>,
    #[serde(default)]
    pub allow_restricted_hosted_inputs: bool,
    pub max_embedding_inputs: usize,
}

impl Default for ContextSemanticConfig {
    fn default() -> Self {
        Self {
            mode: ContextSemanticMode::NoVector,
            provider: None,
            allow_restricted_hosted_inputs: false,
            max_embedding_inputs: 0,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextSemanticMode {
    NoVector,
    Local,
    Hosted,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextEmbeddingProviderKind {
    Local,
    Hosted,
}

impl Default for ContextEngineConfig {
    fn default() -> Self {
        Self::disabled()
    }
}
