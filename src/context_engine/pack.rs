use serde::{Deserialize, Serialize};

use crate::contracts::Role;
use crate::runtime::contracts::{EvidenceId, SessionId, SnapshotId};

use super::{
    ContextEngineConfig, ContextEvidence, ContextEvidenceKind, ContextRelationship,
    ContextSufficiencyStatus, OmittedContextCandidate,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextSufficiency {
    pub status: ContextSufficiencyStatus,
    pub missing: Vec<String>,
    /// Per-hunk coverage gaps with ready-to-run queries (R6).
    #[serde(default)]
    pub gaps: Vec<super::ContextSufficiencyGap>,
}

impl ContextSufficiency {
    pub fn probably_sufficient() -> Self {
        Self {
            status: ContextSufficiencyStatus::ProbablySufficient,
            missing: Vec::new(),
            gaps: Vec::new(),
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

/// Deterministic, explainable ranking from typed structural signals.
/// Weights live in `ContextEngineConfig`; no ranking input is parsed
/// from a display string.
pub(crate) fn score_for_purpose(
    evidence: &ContextEvidence,
    purpose: ContextPackPurpose,
    config: &ContextEngineConfig,
) -> f32 {
    let signals = &evidence.signals;
    let changed_bonus = if evidence.is_changed_span {
        config.weight_changed_span
    } else {
        0.0
    };
    let graph_bonus = match signals.graph_distance {
        // Distance 0 is the changed span itself, already credited above.
        Some(distance) if distance >= 1 => config.weight_graph_proximity / distance as f32,
        _ => 0.0,
    };
    let co_change_bonus =
        config.weight_co_change * (signals.co_change_score / (1.0 + signals.co_change_score));
    let proximity_bonus = config.weight_path_proximity * signals.path_proximity;
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
    let semantic_bonus = config.weight_semantic_change * signals.semantic_change_score;
    changed_bonus
        + graph_bonus
        + co_change_bonus
        + proximity_bonus
        + kind_bonus
        + token_efficiency_bonus(evidence.token_estimate)
        + semantic_bonus
}

/// Explain a selection by citing the structural signals that scored it.
pub(crate) fn explain_selected_evidence(
    evidence: &ContextEvidence,
    purpose: ContextPackPurpose,
) -> Vec<String> {
    let signals = &evidence.signals;
    let mut why: Vec<String> = Vec::new();
    if evidence.representation == super::ContextEvidenceRepresentation::Skeleton {
        why.push(
            "included as a signatures-only skeleton: the full content did not fit the remaining budget"
                .to_string(),
        );
    }
    if evidence.is_changed_span {
        why.push("encloses changed lines under review".to_string());
    }
    match signals.graph_distance {
        Some(1) if !evidence.is_changed_span => {
            why.push("directly references changed code in the Context Graph".to_string());
        }
        Some(distance) if distance >= 2 => {
            why.push(format!(
                "{distance} hops from changed code in the Context Graph"
            ));
        }
        _ => {}
    }
    if signals.co_change_score > 0.0 {
        why.push(format!(
            "co-changed with the files under review (recency-weighted score {:.1})",
            signals.co_change_score
        ));
    }
    if signals.path_proximity >= 0.5 && !evidence.is_changed_span {
        why.push("sits near the changed files in the directory tree".to_string());
    }
    if signals.semantic_change_score > 0.0 {
        why.push(format!(
            "semantically similar to the change (embedding similarity {:.2})",
            signals.semantic_change_score
        ));
    }
    match (purpose, evidence.kind) {
        (ContextPackPurpose::Security, ContextEvidenceKind::RepositoryRule) => {
            why.push("security pack prioritizes repository guidance".to_string())
        }
        (ContextPackPurpose::Security, ContextEvidenceKind::Config) => {
            why.push("security pack prioritizes configuration".to_string())
        }
        (ContextPackPurpose::Tests, ContextEvidenceKind::Test) => {
            why.push("tests pack prioritizes related tests".to_string())
        }
        (ContextPackPurpose::Architecture, ContextEvidenceKind::Doc) => {
            why.push("architecture pack prioritizes documentation".to_string())
        }
        (ContextPackPurpose::Architecture, ContextEvidenceKind::RepositoryRule) => {
            why.push("architecture pack prioritizes repository guidance".to_string())
        }
        (_, ContextEvidenceKind::Diff) => {
            why.push("diff evidence supports changed behavior".to_string())
        }
        _ => {}
    }
    if evidence.token_estimate <= 250 {
        why.push("small enough to include within budget".to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_engine::{
        ContextEvidenceSource, ContextProvenance, ContextRankSignals, ContextScope,
        ContextSensitivity, ContextTrust,
    };

    fn evidence(id: &str, signals: ContextRankSignals, is_changed_span: bool) -> ContextEvidence {
        ContextEvidence {
            id: crate::runtime::contracts::EvidenceId(id.to_string()),
            kind: ContextEvidenceKind::FileSpan,
            source: ContextEvidenceSource::Snapshot,
            trust: ContextTrust::Kernel,
            sensitivity: ContextSensitivity::Private,
            scope: ContextScope::Snapshot,
            path: None,
            revision: None,
            range: None,
            content_hash: None,
            summary: None,
            is_changed_span,
            representation: crate::context_engine::ContextEvidenceRepresentation::FullContent,
            skeleton_text: None,
            signals,
            token_estimate: 100,
            provenance: ContextProvenance {
                provider: "test".to_string(),
                query: None,
                tool_call_id: None,
                snapshot_id: None,
                original_url: None,
            },
            created_at_utc: None,
            expires_at_utc: None,
        }
    }

    #[test]
    fn enclosing_chunk_outranks_unrelated_same_kind_chunk() {
        let config = ContextEngineConfig::snapshot_v0();
        let enclosing = evidence(
            "a",
            ContextRankSignals {
                graph_distance: Some(0),
                ..Default::default()
            },
            true,
        );
        let unrelated = evidence("b", ContextRankSignals::default(), false);
        assert!(
            score_for_purpose(&enclosing, ContextPackPurpose::Correctness, &config)
                > score_for_purpose(&unrelated, ContextPackPurpose::Correctness, &config)
        );
    }

    #[test]
    fn co_changed_file_outranks_historically_unrelated_file() {
        let config = ContextEngineConfig::snapshot_v0();
        let co_changed = evidence(
            "a",
            ContextRankSignals {
                co_change_score: 5.0,
                ..Default::default()
            },
            false,
        );
        let unrelated = evidence("b", ContextRankSignals::default(), false);
        assert!(
            score_for_purpose(&co_changed, ContextPackPurpose::Correctness, &config)
                > score_for_purpose(&unrelated, ContextPackPurpose::Correctness, &config)
        );
    }

    #[test]
    fn graph_proximity_decays_with_distance() {
        let config = ContextEngineConfig::snapshot_v0();
        let near = evidence(
            "a",
            ContextRankSignals {
                graph_distance: Some(1),
                ..Default::default()
            },
            false,
        );
        let far = evidence(
            "b",
            ContextRankSignals {
                graph_distance: Some(2),
                ..Default::default()
            },
            false,
        );
        assert!(
            score_for_purpose(&near, ContextPackPurpose::Correctness, &config)
                > score_for_purpose(&far, ContextPackPurpose::Correctness, &config)
        );
    }

    #[test]
    fn weight_changes_reorder_candidates_predictably() {
        let mut config = ContextEngineConfig::snapshot_v0();
        let co_changed = evidence(
            "a",
            ContextRankSignals {
                co_change_score: 10.0,
                ..Default::default()
            },
            false,
        );
        let near_in_tree = evidence(
            "b",
            ContextRankSignals {
                path_proximity: 1.0,
                ..Default::default()
            },
            false,
        );
        let purpose = ContextPackPurpose::Correctness;
        assert!(
            score_for_purpose(&co_changed, purpose, &config)
                > score_for_purpose(&near_in_tree, purpose, &config),
            "default weights favor strong co-change history"
        );
        config.weight_co_change = 0.0;
        config.weight_path_proximity = 0.30;
        assert!(
            score_for_purpose(&co_changed, purpose, &config)
                < score_for_purpose(&near_in_tree, purpose, &config),
            "reweighting flips the ordering"
        );
    }

    #[test]
    fn explanations_cite_structural_signals() {
        let item = evidence(
            "a",
            ContextRankSignals {
                graph_distance: Some(1),
                co_change_score: 4.2,
                path_proximity: 1.0,
                ..Default::default()
            },
            false,
        );
        let why = explain_selected_evidence(&item, ContextPackPurpose::Correctness);
        assert!(why.iter().any(|reason| reason.contains("Context Graph")));
        assert!(why.iter().any(|reason| reason.contains("co-changed")));
        assert!(why.iter().any(|reason| reason.contains("directory tree")));
        assert!(
            !why.iter().any(|reason| reason.contains("V0")),
            "explanations cite signals, not V0 heuristics"
        );
    }
}
