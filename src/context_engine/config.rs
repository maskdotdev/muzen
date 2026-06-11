use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextEngineMode {
    Disabled,
    SnapshotV0,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Token budget for one AST-aligned chunk (the retrieval unit).
    pub chunk_max_tokens: usize,
    /// Cap on chunk evidence per file; the remainder is recorded as a
    /// `chunk_budget_exceeded` skip.
    pub max_chunks_per_file: usize,
    /// BM25 term-frequency saturation constant.
    pub bm25_k1: f32,
    /// BM25 document-length normalization constant.
    pub bm25_b: f32,
    /// Reciprocal Rank Fusion constant: `score = sum 1 / (rrf_k + rank)`.
    pub rrf_k: f32,
    /// Max hops for change-rooted graph expansion.
    pub graph_max_hops: usize,
    /// Max expansion candidates per changed-file anchor; overflow is
    /// recorded, not silently dropped.
    pub graph_max_candidates_per_anchor: usize,
    /// Commits of history walked for co-change mining (0 disables).
    pub co_change_commit_limit: usize,
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
            chunk_max_tokens: 400,
            max_chunks_per_file: 64,
            bm25_k1: 1.2,
            bm25_b: 0.75,
            rrf_k: 60.0,
            graph_max_hops: 2,
            graph_max_candidates_per_anchor: 16,
            co_change_commit_limit: 500,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_credential_ref: Option<String>,
    #[serde(default)]
    pub allow_restricted_hosted_inputs: bool,
    pub max_embedding_inputs: usize,
}

impl Default for ContextSemanticConfig {
    fn default() -> Self {
        Self {
            mode: ContextSemanticMode::NoVector,
            provider: None,
            hosted_base_url: None,
            hosted_model: None,
            hosted_credential_ref: None,
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
