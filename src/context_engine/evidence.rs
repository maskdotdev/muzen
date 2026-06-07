use serde::{Deserialize, Serialize};

use crate::runtime::contracts::{ArtifactId, EvidenceId, RepoPath, ToolCallId};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextEvidenceKind {
    Diff,
    FileSpan,
    Symbol,
    Test,
    Config,
    Doc,
    RepositoryRule,
    OrganizationRule,
    Ticket,
    HistoricalPr,
    PriorFinding,
    CiFailure,
    Dependency,
    CrossRepoContract,
    ToolOutput,
    PackSummary,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextEvidenceSource {
    Snapshot,
    Host,
    History,
    Memory,
    Tool,
    External,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextTrust {
    Kernel,
    HostTrusted,
    OrganizationTrusted,
    RepositoryUntrusted,
    UserUntrusted,
    ExternalUntrusted,
    ToolProvider,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextSensitivity {
    Public,
    Private,
    SecretRedacted,
    Restricted,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    Run,
    Snapshot,
    Workspace,
    Repository,
    Organization,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextRevision(pub String);

impl ContextRevision {
    pub fn head() -> Self {
        Self("head".to_string())
    }

    pub fn base() -> Self {
        Self("base".to_string())
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextRange {
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextProvenance {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextEvidence {
    pub id: EvidenceId,
    pub kind: ContextEvidenceKind,
    pub source: ContextEvidenceSource,
    pub trust: ContextTrust,
    pub sensitivity: ContextSensitivity,
    pub scope: ContextScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RepoPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<ContextRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ContextRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub token_estimate: usize,
    pub provenance: ContextProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_utc: Option<String>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextRelationshipKind {
    Imports,
    Calls,
    Implements,
    Tests,
    Configures,
    Documents,
    DependsOn,
    SameSymbol,
    SimilarHistory,
    ViolatesRule,
    SatisfiesTicket,
    Contradicts,
    CrossRepoContract,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextRelationship {
    pub from: EvidenceId,
    pub to: EvidenceId,
    pub kind: ContextRelationshipKind,
    pub confidence: f32,
    pub reason: String,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextOmissionReason {
    BudgetExhausted,
    Duplicate,
    LowRelevance,
    LowerTrust,
    GeneratedFile,
    BinaryFile,
    SecretRedacted,
    OutsideScope,
    SupersededBySummary,
    RequiresUngrantedCapability,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OmittedContextCandidate {
    pub evidence_id: EvidenceId,
    pub kind: ContextEvidenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RepoPath>,
    pub score: f32,
    pub token_estimate: usize,
    pub reason: ContextOmissionReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextFindingEvidence {
    pub finding_id: String,
    pub primary_evidence: Vec<EvidenceId>,
    pub supporting_evidence: Vec<EvidenceId>,
    pub contradicted_by: Vec<EvidenceId>,
    pub sufficiency: ContextSufficiencyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<ArtifactId>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextSufficiencyStatus {
    Sufficient,
    ProbablySufficient,
    Insufficient,
}
