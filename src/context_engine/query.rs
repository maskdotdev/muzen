use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::contracts::{EvidenceId, SessionId, SnapshotId};

use super::{ContextEvidence, ContextPackPurpose, ContextSufficiency};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextQueryKind {
    SearchText,
    ReadSpan,
    ExplainPack,
    RelatedTests,
    RelatedSymbols,
    TicketRequirements,
    HistorySimilar,
    SufficiencyCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextQueryLimits {
    pub max_results: usize,
    pub max_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub snapshot_id: SnapshotId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<ContextPackPurpose>,
    pub kind: ContextQueryKind,
    pub arguments: Value,
    pub current_evidence: Vec<EvidenceId>,
    pub limits: ContextQueryLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextQueryResult {
    pub kind: ContextQueryKind,
    pub evidence: Vec<ContextEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sufficiency: Option<ContextSufficiency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub omitted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextFeedback {
    pub snapshot_id: SnapshotId,
    pub evidence_ids: Vec<EvidenceId>,
    pub feedback: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ContextLearningSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ContextLearningScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextFeedbackReceipt {
    pub accepted: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_learning: Option<ContextLearning>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextLearningApproval {
    pub learning_id: String,
    #[serde(default)]
    pub approve: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_utc: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextLearningApprovalReceipt {
    pub accepted: bool,
    pub learning: ContextLearning,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextLearningSource {
    AcceptedFinding,
    DismissedFinding,
    HumanFeedback,
    MergedPr,
    ManualRule,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextLearningStatus {
    Proposed,
    Approved,
    Expired,
    Rejected,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextLearningScope {
    Repository,
    Workspace,
    Organization,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextLearning {
    pub id: String,
    pub snapshot_id: SnapshotId,
    pub source: ContextLearningSource,
    pub status: ContextLearningStatus,
    pub scope: ContextLearningScope,
    pub evidence_ids: Vec<EvidenceId>,
    pub summary: String,
    pub created_at_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_utc: Option<String>,
}
