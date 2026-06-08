use serde::{Deserialize, Serialize};

use crate::contracts::Role;
use crate::runtime::contracts::{EvidenceId, SessionId, SnapshotId};

use super::{
    semantic_score_for_purpose, ContextEngineConfig, ContextEvidence, ContextEvidenceKind,
    ContextRelationship, ContextSufficiencyStatus, OmittedContextCandidate,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextIndexId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextPackId(pub String);

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextPackPurpose {
    GeneralReview,
    Correctness,
    Security,
    Tests,
    Architecture,
    Performance,
    Validator,
    StandaloneQuery,
}

impl ContextPackPurpose {
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::Security => Self::Security,
            Role::Performance => Self::Performance,
            Role::Correctness => Self::Correctness,
            Role::Architecture | Role::Maintainability => Self::Architecture,
            Role::Validator => Self::Validator,
            Role::Generalist => Self::GeneralReview,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::GeneralReview => "general_review",
            Self::Correctness => "correctness",
            Self::Security => "security",
            Self::Tests => "tests",
            Self::Architecture => "architecture",
            Self::Performance => "performance",
            Self::Validator => "validator",
            Self::StandaloneQuery => "standalone_query",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextBudgetUsage {
    pub max_tokens: usize,
    pub used_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextSufficiency {
    pub status: ContextSufficiencyStatus,
    pub missing: Vec<String>,
}

impl ContextSufficiency {
    pub fn probably_sufficient() -> Self {
        Self {
            status: ContextSufficiencyStatus::ProbablySufficient,
            missing: Vec::new(),
        }
    }
}

impl ContextSufficiencyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sufficient => "sufficient",
            Self::ProbablySufficient => "probably_sufficient",
            Self::Insufficient => "insufficient",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextPack {
    pub id: ContextPackId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub snapshot_id: SnapshotId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub purpose: ContextPackPurpose,
    pub evidence: Vec<ContextEvidence>,
    pub relationships: Vec<ContextRelationship>,
    pub omitted_candidates: Vec<OmittedContextCandidate>,
    pub budget: ContextBudgetUsage,
    pub sufficiency: ContextSufficiency,
    pub compiler_version: String,
    pub created_at_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextPackRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub snapshot_id: SnapshotId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub purpose: ContextPackPurpose,
    pub max_tokens: usize,
    pub seed_evidence: Vec<EvidenceId>,
}

pub(crate) fn rank_for_purpose(
    evidence: &[ContextEvidence],
    purpose: ContextPackPurpose,
    config: &ContextEngineConfig,
) -> Vec<(f32, ContextEvidence)> {
    let mut ranked = evidence
        .iter()
        .cloned()
        .map(|evidence| (score_for_purpose(&evidence, purpose, config), evidence))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .partial_cmp(left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
    ranked
}

pub(crate) fn score_for_purpose(
    evidence: &ContextEvidence,
    purpose: ContextPackPurpose,
    config: &ContextEngineConfig,
) -> f32 {
    let changed_bonus = evidence
        .summary
        .as_ref()
        .map(|summary| summary.contains("changed"))
        .unwrap_or(false) as u8 as f32
        * 0.25;
    let kind_bonus = match (purpose, evidence.kind) {
        (ContextPackPurpose::Security, ContextEvidenceKind::RepositoryRule) => 0.35,
        (ContextPackPurpose::Security, ContextEvidenceKind::Config) => 0.25,
        (ContextPackPurpose::Tests, ContextEvidenceKind::Test) => 0.45,
        (ContextPackPurpose::Tests, ContextEvidenceKind::Config) => 0.15,
        (ContextPackPurpose::Architecture, ContextEvidenceKind::Doc) => 0.25,
        (ContextPackPurpose::Architecture, ContextEvidenceKind::RepositoryRule) => 0.35,
        (ContextPackPurpose::Performance, ContextEvidenceKind::Config) => 0.15,
        (_, ContextEvidenceKind::Diff) => 0.4,
        (_, ContextEvidenceKind::FileSpan) => 0.2,
        _ => 0.05,
    };
    changed_bonus
        + kind_bonus
        + token_efficiency_bonus(evidence.token_estimate)
        + semantic_score_for_purpose(config, evidence, purpose)
}

pub(crate) fn explain_selected_evidence(
    evidence: &ContextEvidence,
    purpose: ContextPackPurpose,
) -> Vec<&'static str> {
    let mut why = Vec::new();
    if evidence
        .summary
        .as_ref()
        .map(|summary| summary.contains("changed"))
        .unwrap_or(false)
    {
        why.push("changed-file evidence");
    }
    match (purpose, evidence.kind) {
        (ContextPackPurpose::Security, ContextEvidenceKind::RepositoryRule) => {
            why.push("security pack prioritizes repository guidance")
        }
        (ContextPackPurpose::Security, ContextEvidenceKind::Config) => {
            why.push("security pack prioritizes configuration")
        }
        (ContextPackPurpose::Tests, ContextEvidenceKind::Test) => {
            why.push("tests pack prioritizes related tests")
        }
        (ContextPackPurpose::Architecture, ContextEvidenceKind::Doc) => {
            why.push("architecture pack prioritizes documentation")
        }
        (ContextPackPurpose::Architecture, ContextEvidenceKind::RepositoryRule) => {
            why.push("architecture pack prioritizes repository guidance")
        }
        (_, ContextEvidenceKind::Diff) => why.push("diff evidence supports changed behavior"),
        (_, ContextEvidenceKind::FileSpan) => why.push("file span is directly inspectable"),
        _ => why.push("ranked by deterministic V0 context heuristics"),
    }
    if evidence.token_estimate <= 250 {
        why.push("small enough to include within budget");
    }
    why
}

pub(crate) fn purpose_name(purpose: ContextPackPurpose) -> &'static str {
    purpose.as_str()
}

fn token_efficiency_bonus(tokens: usize) -> f32 {
    if tokens <= 250 {
        0.15
    } else if tokens <= 1_000 {
        0.08
    } else {
        0.0
    }
}
