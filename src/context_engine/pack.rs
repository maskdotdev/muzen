use serde::{Deserialize, Serialize};

use crate::contracts::Role;
use crate::runtime::contracts::{EvidenceId, SessionId, SnapshotId};

use super::{
    ContextEvidence, ContextRelationship, ContextSufficiencyStatus, OmittedContextCandidate,
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
