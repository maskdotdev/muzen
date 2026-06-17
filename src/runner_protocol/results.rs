use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::reviewer_kernel::kernel_types::ArtifactView;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerFileReview {
    pub path: String,
    pub verdict: String,
    #[serde(default)]
    pub coverage: String,
    #[serde(default)]
    pub review_verdict: String,
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
    pub quality_diagnostics: RunnerReviewQualityDiagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunnerReviewQualityDiagnostics {
    pub contract_risk_units: usize,
    pub contract_seed_count: usize,
    pub contract_pack_count: usize,
    #[serde(default)]
    pub omitted_contract_pack_candidates: Vec<String>,
    #[serde(default)]
    pub selected_contract_packs: Vec<String>,
    pub contract_evidence_failures: usize,
    #[serde(default)]
    pub coverage_counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub coverage_counts_by_lens: BTreeMap<String, BTreeMap<String, usize>>,
    #[serde(default)]
    pub high_risk_files_below_target: Vec<String>,
    #[serde(default)]
    pub challenge_status_counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub sessions_run: usize,
    #[serde(default)]
    pub budgets_used: BTreeMap<String, usize>,
    #[serde(default)]
    pub explicit_caller_cap_sessions: usize,
    pub candidate_findings: usize,
    pub rescued_candidates: usize,
    pub rejected_candidates: usize,
    #[serde(default)]
    pub rejection_reasons: BTreeMap<String, usize>,
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
    pub challenge_status: Option<String>,
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
    #[serde(default)]
    pub related_paths: Vec<String>,
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
