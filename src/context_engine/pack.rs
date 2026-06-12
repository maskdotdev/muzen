use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::contracts::Role;
use crate::runtime::contracts::{SessionId, SnapshotId};

use super::{
    ContextEngineConfig, ContextEvidence, ContextEvidenceKind, ContextEvidenceSource,
    ContextRelationship, ContextScope, ContextSufficiencyStatus, ContextTrust,
    OmittedContextCandidate,
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
    pub selected_candidates: Vec<SelectedContextCandidate>,
    pub relationships: Vec<ContextRelationship>,
    pub omitted_candidates: Vec<OmittedContextCandidate>,
    pub budget: ContextBudgetUsage,
    pub sufficiency: ContextSufficiency,
    pub compiler_version: String,
    pub created_at_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelectedContextCandidate {
    pub evidence_id: crate::runtime::contracts::EvidenceId,
    pub score: f32,
    pub rank_index: usize,
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
    ranked.sort_by(rank_candidate_order);
    if config.enable_rank_diversity {
        apply_rank_diversity(&mut ranked, purpose);
    }
    ranked
}

fn apply_rank_diversity(ranked: &mut [(f32, ContextEvidence)], purpose: ContextPackPurpose) {
    let mut seen_by_path: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen_tests = 0usize;
    for (score, evidence) in ranked.iter_mut() {
        if let Some(path) = evidence.path.as_ref() {
            let seen = seen_by_path.entry(path.display()).or_insert(0);
            *score -= path_diversity_penalty(*seen, evidence.is_changed_span);
            *seen = seen.saturating_add(1);
        }
        if evidence.kind == ContextEvidenceKind::Test {
            *score -= test_density_penalty(seen_tests, purpose);
            seen_tests = seen_tests.saturating_add(1);
        }
    }
    ranked.sort_by(rank_candidate_order);
}

fn rank_candidate_order(
    (left_score, left): &(f32, ContextEvidence),
    (right_score, right): &(f32, ContextEvidence),
) -> std::cmp::Ordering {
    right_score
        .partial_cmp(left_score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| left.token_estimate.cmp(&right.token_estimate))
        .then_with(|| {
            left.path
                .as_ref()
                .map(|path| path.display())
                .cmp(&right.path.as_ref().map(|path| path.display()))
        })
        .then_with(|| left.id.0.cmp(&right.id.0))
}

fn path_diversity_penalty(seen: usize, changed_span: bool) -> f32 {
    if seen == 0 {
        return 0.0;
    }
    let base = if changed_span { 0.08 } else { 0.12 };
    base * (seen.min(5) as f32)
}

fn test_density_penalty(seen_tests: usize, purpose: ContextPackPurpose) -> f32 {
    if !matches!(
        purpose,
        ContextPackPurpose::GeneralReview
            | ContextPackPurpose::Correctness
            | ContextPackPurpose::Validator
    ) || seen_tests < 6
    {
        return 0.0;
    }
    0.04 * (seen_tests.saturating_sub(5).min(10) as f32)
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
    let lexical_bonus = config.weight_lexical_change * signals.lexical_change_score;
    let test_coverage_bonus = related_test_coverage_bonus(evidence, purpose, config);
    let run_context_bonus = trusted_run_context_bonus(evidence, purpose);
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
    let token_efficiency_bonus = if config.enable_token_efficiency_bonus {
        token_efficiency_bonus(evidence.token_estimate)
    } else {
        0.0
    };
    changed_bonus
        + graph_bonus
        + co_change_bonus
        + proximity_bonus
        + lexical_bonus
        + test_coverage_bonus
        + run_context_bonus
        + kind_bonus
        + token_efficiency_bonus
        + semantic_bonus
}

fn trusted_run_context_bonus(evidence: &ContextEvidence, purpose: ContextPackPurpose) -> f32 {
    if evidence.source != ContextEvidenceSource::Host
        || evidence.trust != ContextTrust::HostTrusted
        || evidence.scope != ContextScope::Run
    {
        return 0.0;
    }
    match (purpose, evidence.kind) {
        (
            ContextPackPurpose::GeneralReview
            | ContextPackPurpose::Correctness
            | ContextPackPurpose::Validator,
            ContextEvidenceKind::Ticket,
        ) => 0.44,
        (
            ContextPackPurpose::GeneralReview
            | ContextPackPurpose::Correctness
            | ContextPackPurpose::Tests
            | ContextPackPurpose::Validator,
            ContextEvidenceKind::CiFailure,
        ) => 0.39,
        (
            ContextPackPurpose::GeneralReview
            | ContextPackPurpose::Correctness
            | ContextPackPurpose::Architecture
            | ContextPackPurpose::Validator,
            ContextEvidenceKind::CrossRepoContract,
        ) => 0.41,
        (
            ContextPackPurpose::GeneralReview
            | ContextPackPurpose::Correctness
            | ContextPackPurpose::Security
            | ContextPackPurpose::Validator,
            ContextEvidenceKind::PriorFinding,
        ) => 0.35,
        (_, ContextEvidenceKind::Ticket) => 0.24,
        (_, ContextEvidenceKind::CiFailure) => 0.22,
        (_, ContextEvidenceKind::CrossRepoContract) => 0.24,
        (_, ContextEvidenceKind::PriorFinding) => 0.22,
        (_, ContextEvidenceKind::OrganizationRule) => 0.20,
        _ => 0.0,
    }
}

fn related_test_coverage_bonus(
    evidence: &ContextEvidence,
    purpose: ContextPackPurpose,
    config: &ContextEngineConfig,
) -> f32 {
    if evidence.kind != ContextEvidenceKind::Test {
        return 0.0;
    }
    if !matches!(
        purpose,
        ContextPackPurpose::GeneralReview
            | ContextPackPurpose::Correctness
            | ContextPackPurpose::Tests
            | ContextPackPurpose::Validator
    ) {
        return 0.0;
    }
    let Some(distance) = evidence.signals.graph_distance else {
        return 0.0;
    };
    if distance == 0 || distance > 2 {
        return 0.0;
    }
    let locally_relevant =
        evidence.signals.path_proximity >= 0.5 || evidence.signals.lexical_change_score > 0.0;
    if !locally_relevant {
        return 0.0;
    }
    config.weight_test_coverage / f32::from(distance)
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
    if signals.lexical_change_score > 0.0 {
        why.push(format!(
            "shares rare change terms with the files under review ({:.2})",
            signals.lexical_change_score
        ));
    }
    if related_test_coverage_bonus(
        evidence,
        purpose,
        &ContextEngineConfig {
            weight_test_coverage: 1.0,
            ..ContextEngineConfig::snapshot_v0()
        },
    ) > 0.0
    {
        why.push("related test coverage for graph-near changed code".to_string());
    }
    if signals.semantic_change_score > 0.0 {
        why.push(format!(
            "semantically similar to the change (embedding similarity {:.2})",
            signals.semantic_change_score
        ));
    }
    if trusted_run_context_bonus(evidence, purpose) > 0.0 {
        why.push("trusted run-scoped host context".to_string());
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
        0.045
    } else if tokens <= 1_000 {
        0.0225
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
    use crate::runtime::contracts::RepoPath;

    fn evidence(id: &str, signals: ContextRankSignals, is_changed_span: bool) -> ContextEvidence {
        evidence_with_kind(id, ContextEvidenceKind::FileSpan, signals, is_changed_span)
    }

    fn evidence_with_kind(
        id: &str,
        kind: ContextEvidenceKind,
        signals: ContextRankSignals,
        is_changed_span: bool,
    ) -> ContextEvidence {
        ContextEvidence {
            id: crate::runtime::contracts::EvidenceId(id.to_string()),
            kind,
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

    fn evidence_with_path(
        id: &str,
        kind: ContextEvidenceKind,
        signals: ContextRankSignals,
        path: &str,
    ) -> ContextEvidence {
        let mut evidence = evidence_with_kind(id, kind, signals, false);
        evidence.path = Some(RepoPath::parse(path).expect("test path"));
        evidence
    }

    fn trusted_run_context(id: &str, kind: ContextEvidenceKind) -> ContextEvidence {
        let mut evidence = evidence_with_kind(id, kind, ContextRankSignals::default(), false);
        evidence.source = ContextEvidenceSource::Host;
        evidence.trust = ContextTrust::HostTrusted;
        evidence.scope = ContextScope::Run;
        evidence
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
    fn graph_near_tests_are_first_class_general_review_context() {
        let config = ContextEngineConfig::snapshot_v0();
        let signals = ContextRankSignals {
            graph_distance: Some(1),
            path_proximity: 1.0,
            lexical_change_score: 1.0,
            ..Default::default()
        };
        let related_test = evidence_with_kind("test", ContextEvidenceKind::Test, signals, false);
        let implementation =
            evidence_with_kind("impl", ContextEvidenceKind::FileSpan, signals, false);

        assert!(
            score_for_purpose(&related_test, ContextPackPurpose::GeneralReview, &config)
                > score_for_purpose(&implementation, ContextPackPurpose::GeneralReview, &config),
            "general review packs should not bury graph-connected nearby tests behind same-signal implementation spans"
        );
    }

    #[test]
    fn equal_score_candidates_prefer_cheaper_evidence() {
        let config = ContextEngineConfig::snapshot_v0();
        let mut expensive = evidence_with_path(
            "a-expensive",
            ContextEvidenceKind::FileSpan,
            ContextRankSignals::default(),
            "src/a.rs",
        );
        expensive.token_estimate = 900;
        let mut cheap = evidence_with_path(
            "z-cheap",
            ContextEvidenceKind::FileSpan,
            ContextRankSignals::default(),
            "src/z.rs",
        );
        cheap.token_estimate = 300;

        let ranked = rank_for_purpose(
            &[expensive.clone(), cheap.clone()],
            ContextPackPurpose::GeneralReview,
            &config,
        );

        assert_eq!(ranked[0].1.id, cheap.id);
        assert_eq!(ranked[1].1.id, expensive.id);
    }

    #[test]
    fn token_efficiency_bonus_can_be_disabled_for_ablation() {
        let config = ContextEngineConfig::snapshot_v0();
        let mut no_token_bonus = config.clone();
        no_token_bonus.enable_token_efficiency_bonus = false;
        let mut large = evidence_with_path(
            "large",
            ContextEvidenceKind::FileSpan,
            ContextRankSignals::default(),
            "src/large.rs",
        );
        large.token_estimate = 2_000;
        let mut small = evidence_with_path(
            "small",
            ContextEvidenceKind::FileSpan,
            ContextRankSignals::default(),
            "src/small.rs",
        );
        small.token_estimate = 200;

        assert!(
            score_for_purpose(&small, ContextPackPurpose::GeneralReview, &config)
                > score_for_purpose(&large, ContextPackPurpose::GeneralReview, &config)
        );
        assert_eq!(
            score_for_purpose(&small, ContextPackPurpose::GeneralReview, &no_token_bonus),
            score_for_purpose(&large, ContextPackPurpose::GeneralReview, &no_token_bonus)
        );
    }

    #[test]
    fn token_efficiency_bonus_is_bounded_below_structural_signals() {
        let config = ContextEngineConfig::snapshot_v0();

        assert_eq!(token_efficiency_bonus(250), 0.045);
        assert_eq!(token_efficiency_bonus(1_000), 0.0225);
        assert_eq!(token_efficiency_bonus(1_001), 0.0);
        assert!(token_efficiency_bonus(250) < config.weight_graph_proximity);
        assert!(token_efficiency_bonus(250) < config.weight_lexical_change);
        assert!(token_efficiency_bonus(250) < config.weight_changed_span);
    }

    #[test]
    fn trusted_run_ticket_prioritized_without_beating_changed_code() {
        let config = ContextEngineConfig::snapshot_v0();
        let ticket = trusted_run_context("ticket", ContextEvidenceKind::Ticket);
        let unrelated = evidence_with_kind(
            "unrelated",
            ContextEvidenceKind::FileSpan,
            ContextRankSignals::default(),
            false,
        );
        let changed = evidence_with_path(
            "changed",
            ContextEvidenceKind::FileSpan,
            ContextRankSignals {
                graph_distance: Some(0),
                path_proximity: 1.0,
                ..Default::default()
            },
            "src/lib.rs",
        );
        let mut changed = changed;
        changed.is_changed_span = true;

        let purpose = ContextPackPurpose::GeneralReview;
        assert!(
            score_for_purpose(&ticket, purpose, &config)
                > score_for_purpose(&unrelated, purpose, &config),
            "trusted run ticket context should outrank generic unrelated file spans"
        );
        assert!(
            score_for_purpose(&changed, purpose, &config)
                > score_for_purpose(&ticket, purpose, &config),
            "changed code remains the strongest evidence"
        );
    }

    #[test]
    fn untrusted_host_ticket_receives_no_run_context_bonus() {
        let config = ContextEngineConfig::snapshot_v0();
        let trusted = trusted_run_context("trusted", ContextEvidenceKind::Ticket);
        let mut untrusted = trusted_run_context("untrusted", ContextEvidenceKind::Ticket);
        untrusted.trust = ContextTrust::UserUntrusted;

        assert!(
            score_for_purpose(&trusted, ContextPackPurpose::Correctness, &config)
                > score_for_purpose(&untrusted, ContextPackPurpose::Correctness, &config),
            "only host-trusted run context receives priority"
        );
    }

    #[test]
    fn explanations_cite_trusted_run_context() {
        let why = explain_selected_evidence(
            &trusted_run_context("ticket", ContextEvidenceKind::Ticket),
            ContextPackPurpose::Correctness,
        );

        assert!(why
            .iter()
            .any(|reason| reason == "trusted run-scoped host context"));
    }

    #[test]
    fn general_review_test_density_preserves_implementation_after_test_frontier() {
        let config = ContextEngineConfig::snapshot_v0();
        let mut no_rank_diversity = config.clone();
        no_rank_diversity.enable_rank_diversity = false;
        let signals = ContextRankSignals {
            graph_distance: Some(1),
            path_proximity: 1.0,
            lexical_change_score: 1.0,
            ..Default::default()
        };
        let mut evidence = (0..12)
            .map(|index| {
                evidence_with_path(
                    &format!("test-{index:02}"),
                    ContextEvidenceKind::Test,
                    signals,
                    &format!("src/feature/case-{index}.test.ts"),
                )
            })
            .collect::<Vec<_>>();
        evidence.push(evidence_with_path(
            "impl",
            ContextEvidenceKind::FileSpan,
            signals,
            "src/feature/implementation.ts",
        ));

        let ranked = rank_for_purpose(&evidence, ContextPackPurpose::GeneralReview, &config);
        let impl_position = ranked
            .iter()
            .position(|(_, evidence)| evidence.id.0 == "impl")
            .expect("implementation ranked");
        let tenth_test_position = ranked
            .iter()
            .position(|(_, evidence)| evidence.id.0 == "test-09")
            .expect("test ranked");

        assert!(
            impl_position < tenth_test_position,
            "generic review packs should keep graph-near tests first-class without letting test density bury implementation context"
        );

        let no_diversity_ranked = rank_for_purpose(
            &evidence,
            ContextPackPurpose::GeneralReview,
            &no_rank_diversity,
        );
        let no_diversity_impl_position = no_diversity_ranked
            .iter()
            .position(|(_, evidence)| evidence.id.0 == "impl")
            .expect("implementation ranked");
        assert_eq!(
            no_diversity_impl_position, 12,
            "rank-diversity ablation disables test-density frontier"
        );
    }

    #[test]
    fn tests_pack_does_not_apply_generic_test_density_penalty() {
        let config = ContextEngineConfig::snapshot_v0();
        let signals = ContextRankSignals {
            graph_distance: Some(1),
            path_proximity: 1.0,
            lexical_change_score: 1.0,
            ..Default::default()
        };
        let mut evidence = (0..12)
            .map(|index| {
                evidence_with_path(
                    &format!("test-{index:02}"),
                    ContextEvidenceKind::Test,
                    signals,
                    &format!("src/feature/case-{index}.test.ts"),
                )
            })
            .collect::<Vec<_>>();
        evidence.push(evidence_with_path(
            "impl",
            ContextEvidenceKind::FileSpan,
            signals,
            "src/feature/implementation.ts",
        ));

        let ranked = rank_for_purpose(&evidence, ContextPackPurpose::Tests, &config);
        let impl_position = ranked
            .iter()
            .position(|(_, evidence)| evidence.id.0 == "impl")
            .expect("implementation ranked");

        assert_eq!(
            impl_position, 12,
            "tests packs keep test evidence ahead of same-signal implementation spans"
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

    #[test]
    fn explanations_cite_related_test_coverage() {
        let item = evidence_with_kind(
            "test",
            ContextEvidenceKind::Test,
            ContextRankSignals {
                graph_distance: Some(1),
                path_proximity: 1.0,
                ..Default::default()
            },
            false,
        );
        let why = explain_selected_evidence(&item, ContextPackPurpose::GeneralReview);
        assert!(why
            .iter()
            .any(|reason| reason.contains("related test coverage")));
    }
}
