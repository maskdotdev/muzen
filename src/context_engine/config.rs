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
    /// Entry cap per section (files, vectors) of the durable derived-data
    /// cache (R9); least-recently-used entries are pruned at flush.
    pub derived_cache_max_entries: usize,
    /// Ranking weight for evidence overlapping changed lines.
    pub weight_changed_span: f32,
    /// Ranking weight for reference-graph proximity to the change;
    /// applied as `weight / graph_distance` for distance >= 1.
    pub weight_graph_proximity: f32,
    /// Ranking weight for co-change history; applied to the saturating
    /// normalization `score / (1 + score)`.
    pub weight_co_change: f32,
    /// Ranking weight for directory proximity to changed files.
    pub weight_path_proximity: f32,
    /// Ranking weight for rare lexical overlap with changed paths/summaries.
    pub weight_lexical_change: f32,
    /// Ranking weight for graph-connected tests that sit near, or share
    /// rare terms with, the changed files.
    pub weight_test_coverage: f32,
    /// Ranking weight for embedding similarity to the change (R8);
    /// applied to `ContextRankSignals::semantic_change_score`, which is
    /// zero in no-vector mode.
    pub weight_semantic_change: f32,
    /// Enable deterministic post-greedy pack repair for higher-value candidates
    /// that initially lost budget. Public eval ablations can disable this to
    /// measure optimizer value.
    pub enable_pack_repair: bool,
    /// Enable path-diverse pack compilation: first-ranked item per path gets
    /// a chance before repeated chunks consume budget.
    pub enable_pack_path_diversity: bool,
    /// Reserve a bounded slice of large pack budgets for skeleton fallbacks.
    pub enable_skeleton_reserve: bool,
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
            max_evidence_items: 8_000,
            max_pack_tokens: 12_000,
            max_query_results: 120,
            chunk_max_tokens: 400,
            max_chunks_per_file: 64,
            bm25_k1: 1.2,
            bm25_b: 0.75,
            rrf_k: 60.0,
            graph_max_hops: 2,
            graph_max_candidates_per_anchor: 64,
            co_change_commit_limit: 500,
            derived_cache_max_entries: 100_000,
            weight_changed_span: 0.25,
            weight_graph_proximity: 0.20,
            weight_co_change: 0.15,
            weight_path_proximity: 0.05,
            weight_lexical_change: 0.20,
            weight_test_coverage: 0.30,
            weight_semantic_change: 0.10,
            enable_pack_repair: true,
            enable_pack_path_diversity: true,
            enable_skeleton_reserve: true,
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
    /// Directory holding `model.onnx`/`model_quantized.onnx` and
    /// `tokenizer.json` for `LocalOnnx` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_onnx_model_dir: Option<String>,
    #[serde(default)]
    pub allow_restricted_hosted_inputs: bool,
    pub max_embedding_inputs: usize,
    /// Optional cross-encoder rerank stage over the fused candidates (R8).
    #[serde(default)]
    pub rerank: ContextRerankConfig,
}

impl Default for ContextSemanticConfig {
    fn default() -> Self {
        Self {
            mode: ContextSemanticMode::NoVector,
            provider: None,
            hosted_base_url: None,
            hosted_model: None,
            hosted_credential_ref: None,
            local_onnx_model_dir: None,
            allow_restricted_hosted_inputs: false,
            max_embedding_inputs: 0,
            rerank: ContextRerankConfig::default(),
        }
    }
}

/// Cross-encoder rerank stage over the fused top candidates (R8).
///
/// Speaks the Cohere-style `/rerank` contract
/// (`{model, query, documents, top_n}` -> `{results: [{index,
/// relevance_score}]}`), which Cohere, Jina, and self-hosted servers
/// (vLLM, Infinity) all serve, so in-house rerankers plug in via
/// `base_url` with no special mode. Reranking is always a network call:
/// restricted evidence never reaches it unless
/// `allow_restricted_hosted_inputs` is set, the same policy as hosted
/// embeddings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextRerankConfig {
    /// Off by default; rerank-off output is exactly the RRF fusion order.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Credential reference (`env:NAME`). Optional so in-house rerankers
    /// behind trusted networks can run without auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Fused candidates sent to the reranker.
    #[serde(default = "default_rerank_top_n")]
    pub top_n: usize,
}

fn default_rerank_top_n() -> usize {
    50
}

impl Default for ContextRerankConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: None,
            model: None,
            credential_ref: None,
            top_n: default_rerank_top_n(),
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextSemanticMode {
    NoVector,
    Local,
    /// Local transformer embeddings via ONNX Runtime (R8 evaluation
    /// tier): real semantic quality with no data leaving the host.
    LocalOnnx,
    Hosted,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextEmbeddingProviderKind {
    Local,
    LocalOnnx,
    Hosted,
}

impl Default for ContextEngineConfig {
    fn default() -> Self {
        Self::disabled()
    }
}
