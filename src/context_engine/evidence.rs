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

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
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

/// Structural ranking signals (R5), computed in an index post-pass once
/// the Context Graph and co-change history exist. Typed inputs to
/// `score_for_purpose`; never parsed from display strings.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextRankSignals {
    /// Hops from the nearest change anchor in the Context Graph:
    /// 0 = overlaps changed lines, 1 = same file or direct reference,
    /// 2 = two hops out. `None` = unconnected to the change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_distance: Option<u8>,
    /// Recency-decayed co-change weight with the review's changed files.
    #[serde(default)]
    pub co_change_score: f32,
    /// Directory proximity to the nearest changed file, in [0, 1].
    #[serde(default)]
    pub path_proximity: f32,
    /// Rare path/summary token overlap with changed evidence, in [0, 1].
    #[serde(default)]
    pub lexical_change_score: f32,
    /// Embedding similarity to the nearest change anchor, in [0, 1]
    /// (R8). Zero in no-vector mode, for changed spans themselves, and
    /// for evidence outside the embedded window, so the deterministic
    /// default ranking is untouched.
    #[serde(default)]
    pub semantic_change_score: f32,
}

/// How evidence content enters a pack (R7): the full chunk text, or a
/// signatures-only skeleton when the full content does not fit the
/// remaining budget. Token estimates always describe the active
/// representation.
#[derive(Debug, Copy, Clone, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextEvidenceRepresentation {
    #[default]
    FullContent,
    Skeleton,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// True when this evidence overlaps a diff hunk in the change under
    /// review. Structured replacement for "changed" markers in summaries.
    #[serde(default)]
    pub is_changed_span: bool,
    #[serde(default)]
    pub representation: ContextEvidenceRepresentation,
    /// Skeleton view text (bodies elided, line numbers preserved).
    /// Present exactly when `representation == Skeleton`: skeletons are
    /// synthesized views that exist nowhere on disk, so the pack carries
    /// their text. Full-content evidence resolves through (path, range).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skeleton_text: Option<String>,
    #[serde(default)]
    pub signals: ContextRankSignals,
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
    CalledBy,
    EnclosesHunk,
    CoChanged,
    SameModule,
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
    /// The full content did not fit the remaining pack budget, but its
    /// file's signatures-only skeleton did and was included instead.
    DowngradedToSkeleton,
}

impl ContextOmissionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BudgetExhausted => "budget_exhausted",
            Self::Duplicate => "duplicate",
            Self::LowRelevance => "low_relevance",
            Self::LowerTrust => "lower_trust",
            Self::GeneratedFile => "generated_file",
            Self::BinaryFile => "binary_file",
            Self::SecretRedacted => "secret_redacted",
            Self::OutsideScope => "outside_scope",
            Self::SupersededBySummary => "superseded_by_summary",
            Self::RequiresUngrantedCapability => "requires_ungranted_capability",
            Self::DowngradedToSkeleton => "downgraded_to_skeleton",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OmittedContextCandidate {
    pub evidence_id: EvidenceId,
    pub kind: ContextEvidenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RepoPath>,
    pub score: f32,
    pub rank_index: usize,
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
