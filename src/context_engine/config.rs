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

impl Default for ContextEngineConfig {
    fn default() -> Self {
        Self::disabled()
    }
}
