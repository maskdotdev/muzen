use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::runtime::contracts::{stable_id, RuntimeError, RuntimeResult, SnapshotId};

use super::unix_timestamp_string;
use super::ContextLearningStore;
use super::{evidence_by_id, fused_search, read_line_span, string_arg, trust_rank, usize_arg};
use super::{explain_selected_evidence, purpose_name, rank_for_purpose, score_for_purpose};
use super::{learning_is_expired, redact_context_content};
use super::{path_stem, related_symbol_score, related_symbol_terms};
use super::{
    ContextBudgetUsage, ContextCandidateGraphPath, ContextDerivedCache, ContextEngineConfig,
    ContextEngineMode, ContextEvidence, ContextEvidenceKind, ContextEvidenceRepresentation,
    ContextFeedback, ContextFeedbackReceipt, ContextIndex, ContextIndexReport, ContextIndexRequest,
    ContextIndexStore, ContextLearning, ContextLearningApproval, ContextLearningApprovalReceipt,
    ContextLearningScope, ContextLearningSource, ContextLearningStatus, ContextOmissionReason,
    ContextPack, ContextPackId, ContextPackPurpose, ContextPackRequest, ContextQuery,
    ContextQueryKind, ContextQueryResult, ContextRange, ContextRelationship,
    ContextSufficiencyStatus, FileContextDerivedCache, FileContextLearningStore,
    InMemoryContextDerivedCache, InMemoryContextIndexStore, InMemoryContextLearningStore,
    OmittedContextCandidate, SelectedContextCandidate, CONTEXT_ENGINE_VERSION,
};

#[async_trait]
pub trait ContextEngine: Send + Sync {
    fn config(&self) -> ContextEngineConfig;

    fn get_index(&self, _snapshot_id: &SnapshotId) -> Option<Arc<ContextIndex>> {
        None
    }

    async fn index_snapshot(
        &self,
        request: ContextIndexRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextIndexReport>;

    async fn build_pack(
        &self,
        request: ContextPackRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextPack>;

    async fn query(
        &self,
        request: ContextQuery,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextQueryResult>;

    async fn record_feedback(
        &self,
        feedback: ContextFeedback,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextFeedbackReceipt>;

    async fn approve_learning(
        &self,
        approval: ContextLearningApproval,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextLearningApprovalReceipt>;
}

#[derive(Debug, Default)]
pub struct NoopContextEngine;

#[async_trait]
impl ContextEngine for NoopContextEngine {
    fn config(&self) -> ContextEngineConfig {
        ContextEngineConfig::disabled()
    }

    async fn index_snapshot(
        &self,
        _request: ContextIndexRequest,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ContextIndexReport> {
        Err(RuntimeError::InvalidInput(
            "context engine is disabled".to_string(),
        ))
    }

    async fn build_pack(
        &self,
        _request: ContextPackRequest,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ContextPack> {
        Err(RuntimeError::InvalidInput(
            "context engine is disabled".to_string(),
        ))
    }

    async fn query(
        &self,
        _request: ContextQuery,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ContextQueryResult> {
        Err(RuntimeError::InvalidInput(
            "context engine is disabled".to_string(),
        ))
    }

    async fn record_feedback(
        &self,
        _feedback: ContextFeedback,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ContextFeedbackReceipt> {
        Err(RuntimeError::InvalidInput(
            "context engine is disabled".to_string(),
        ))
    }

    async fn approve_learning(
        &self,
        _approval: ContextLearningApproval,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ContextLearningApprovalReceipt> {
        Err(RuntimeError::InvalidInput(
            "context engine is disabled".to_string(),
        ))
    }
}

pub struct SnapshotContextEngine {
    config: ContextEngineConfig,
    store: Arc<dyn ContextIndexStore>,
    packs: Arc<Mutex<BTreeMap<String, ContextPack>>>,
    learnings: Arc<dyn ContextLearningStore>,
    derived_cache: Arc<dyn ContextDerivedCache>,
}

impl Clone for SnapshotContextEngine {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            store: Arc::clone(&self.store),
            packs: Arc::clone(&self.packs),
            learnings: Arc::clone(&self.learnings),
            derived_cache: Arc::clone(&self.derived_cache),
        }
    }
}

impl SnapshotContextEngine {
    pub fn new(config: ContextEngineConfig) -> Self {
        Self {
            config,
            store: Arc::new(InMemoryContextIndexStore::new()),
            packs: Arc::new(Mutex::new(BTreeMap::new())),
            learnings: Arc::new(InMemoryContextLearningStore::new()),
            derived_cache: Arc::new(InMemoryContextDerivedCache::new()),
        }
    }

    pub fn with_store(config: ContextEngineConfig, store: Arc<dyn ContextIndexStore>) -> Self {
        Self::with_stores(config, store, Arc::new(InMemoryContextLearningStore::new()))
    }

    pub fn with_stores(
        config: ContextEngineConfig,
        store: Arc<dyn ContextIndexStore>,
        learnings: Arc<dyn ContextLearningStore>,
    ) -> Self {
        Self {
            config,
            store,
            packs: Arc::new(Mutex::new(BTreeMap::new())),
            learnings,
            derived_cache: Arc::new(InMemoryContextDerivedCache::new()),
        }
    }

    pub fn with_learning_store_file(
        config: ContextEngineConfig,
        path: impl AsRef<std::path::Path>,
    ) -> RuntimeResult<Self> {
        Ok(Self::with_stores(
            config,
            Arc::new(InMemoryContextIndexStore::new()),
            Arc::new(FileContextLearningStore::open(path)?),
        ))
    }

    /// Replace the derived-data cache, e.g. with a durable file-backed
    /// one (R9). Every index built by this engine reuses it.
    pub fn with_derived_cache(mut self, cache: Arc<dyn ContextDerivedCache>) -> Self {
        self.derived_cache = cache;
        self
    }

    /// Attach a durable derived-data cache at `path` (R9). Unreadable
    /// cache content degrades to a full rebuild with a warning on the
    /// next index report.
    pub fn with_derived_cache_file(self, path: impl AsRef<std::path::Path>) -> Self {
        let max_entries = self.config.derived_cache_max_entries;
        self.with_derived_cache(Arc::new(FileContextDerivedCache::open(path, max_entries)))
    }

    pub fn config_ref(&self) -> &ContextEngineConfig {
        &self.config
    }

    pub fn store(&self) -> Arc<dyn ContextIndexStore> {
        Arc::clone(&self.store)
    }
}

impl std::fmt::Debug for SnapshotContextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotContextEngine")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn pack_path_selection_limit(
    purpose: ContextPackPurpose,
    enable_pack_path_diversity: bool,
) -> Option<usize> {
    if !enable_pack_path_diversity {
        return None;
    }
    match purpose {
        ContextPackPurpose::GeneralReview
        | ContextPackPurpose::Correctness
        | ContextPackPurpose::Validator => Some(2),
        _ => None,
    }
}

fn pack_evidence_path_selection_limit(
    purpose: ContextPackPurpose,
    evidence: &ContextEvidence,
    enable_pack_path_diversity: bool,
) -> Option<usize> {
    let limit = pack_path_selection_limit(purpose, enable_pack_path_diversity)?;
    if evidence.kind == ContextEvidenceKind::Test {
        Some(1)
    } else {
        Some(limit)
    }
}

fn path_selection_limit_exceeded(
    selected_by_path: &BTreeMap<String, usize>,
    evidence: &ContextEvidence,
    limit: usize,
) -> bool {
    if !pack_selection_limit_applies(evidence) {
        return false;
    }
    evidence
        .path
        .as_ref()
        .and_then(|path| selected_by_path.get(&path.display()))
        .is_some_and(|selected| *selected >= limit)
}

fn repeated_path_should_wait_for_first_pass(
    selected_by_path: &BTreeMap<String, usize>,
    purpose: ContextPackPurpose,
    evidence: &ContextEvidence,
    enable_pack_path_diversity: bool,
) -> bool {
    pack_evidence_path_selection_limit(purpose, evidence, enable_pack_path_diversity).is_some()
        && pack_selection_limit_applies(evidence)
        && evidence
            .path
            .as_ref()
            .and_then(|path| selected_by_path.get(&path.display()))
            .is_some_and(|selected| *selected >= 1)
}

fn pack_selection_limit_applies(evidence: &ContextEvidence) -> bool {
    !evidence.is_changed_span
        && evidence.kind != ContextEvidenceKind::Diff
        && evidence.representation == ContextEvidenceRepresentation::FullContent
}

fn skeleton_reserve_tokens(max_tokens: usize, enable_skeleton_reserve: bool) -> usize {
    if !enable_skeleton_reserve {
        return 0;
    }
    if max_tokens < 4_000 {
        0
    } else {
        (max_tokens / 5).min(2_500)
    }
}

fn full_content_budget_limit(
    max_tokens: usize,
    evidence: &ContextEvidence,
    enable_skeleton_reserve: bool,
) -> usize {
    if pack_selection_limit_applies(evidence) {
        max_tokens.saturating_sub(skeleton_reserve_tokens(max_tokens, enable_skeleton_reserve))
    } else {
        max_tokens
    }
}

fn record_selected_path(
    selected_by_path: &mut BTreeMap<String, usize>,
    evidence: &ContextEvidence,
) {
    let Some(path) = evidence.path.as_ref() else {
        return;
    };
    *selected_by_path.entry(path.display()).or_insert(0) += 1;
}

fn unrecord_selected_path(
    selected_by_path: &mut BTreeMap<String, usize>,
    evidence: &ContextEvidence,
) {
    let Some(path) = evidence.path.as_ref() else {
        return;
    };
    let path = path.display();
    if let Some(selected) = selected_by_path.get_mut(&path) {
        *selected = selected.saturating_sub(1);
        if *selected == 0 {
            selected_by_path.remove(&path);
        }
    }
}

#[derive(Clone)]
struct RankedPackCandidate {
    score: f32,
    rank_index: usize,
    evidence: ContextEvidence,
}

#[derive(Clone)]
struct SelectedPackCandidate {
    score: f32,
    rank_index: usize,
    evidence: ContextEvidence,
}

fn omitted_candidate(
    evidence: &ContextEvidence,
    score: f32,
    rank_index: usize,
    reason: ContextOmissionReason,
) -> OmittedContextCandidate {
    OmittedContextCandidate {
        evidence_id: evidence.id.clone(),
        kind: evidence.kind,
        path: evidence.path.clone(),
        signals: evidence.signals,
        score,
        rank_index,
        token_estimate: evidence.token_estimate,
        reason,
        graph_paths: Vec::new(),
        graph_paths_truncated: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn select_ranked_pack_candidate(
    purpose: ContextPackPurpose,
    max_tokens: usize,
    enable_pack_path_diversity: bool,
    enable_skeleton_reserve: bool,
    skeletons: &BTreeMap<String, ContextEvidence>,
    used_tokens: &mut usize,
    selected: &mut Vec<SelectedPackCandidate>,
    omitted_candidates: &mut Vec<OmittedContextCandidate>,
    budget_omitted_candidates: &mut Vec<RankedPackCandidate>,
    selected_by_path: &mut BTreeMap<String, usize>,
    candidate: RankedPackCandidate,
) {
    let score = candidate.score;
    let rank_index = candidate.rank_index;
    let evidence = candidate.evidence;
    if pack_evidence_path_selection_limit(purpose, &evidence, enable_pack_path_diversity)
        .is_some_and(|limit| path_selection_limit_exceeded(selected_by_path, &evidence, limit))
    {
        omitted_candidates.push(omitted_candidate(
            &evidence,
            score,
            rank_index,
            ContextOmissionReason::Duplicate,
        ));
        return;
    }
    let full_content_limit =
        full_content_budget_limit(max_tokens, &evidence, enable_skeleton_reserve);
    if used_tokens.saturating_add(evidence.token_estimate) <= full_content_limit {
        *used_tokens = used_tokens.saturating_add(evidence.token_estimate);
        record_selected_path(selected_by_path, &evidence);
        selected.push(SelectedPackCandidate {
            score,
            rank_index,
            evidence,
        });
        return;
    }
    let skeleton = skeletons
        .get(&evidence.id.0)
        .filter(|skeleton| used_tokens.saturating_add(skeleton.token_estimate) <= max_tokens);
    let reason = match skeleton {
        Some(skeleton) => {
            *used_tokens = used_tokens.saturating_add(skeleton.token_estimate);
            record_selected_path(selected_by_path, skeleton);
            selected.push(SelectedPackCandidate {
                score,
                rank_index,
                evidence: skeleton.clone(),
            });
            ContextOmissionReason::DowngradedToSkeleton
        }
        None => ContextOmissionReason::BudgetExhausted,
    };
    if reason == ContextOmissionReason::BudgetExhausted {
        budget_omitted_candidates.push(RankedPackCandidate {
            score,
            rank_index,
            evidence: evidence.clone(),
        });
    }
    omitted_candidates.push(omitted_candidate(&evidence, score, rank_index, reason));
}

fn selected_full_content_tokens(selected: &[SelectedPackCandidate]) -> usize {
    selected
        .iter()
        .filter(|candidate| pack_selection_limit_applies(&candidate.evidence))
        .map(|candidate| candidate.evidence.token_estimate)
        .sum()
}

fn pack_repair_evictable_score_ceiling() -> f32 {
    0.5
}

fn pack_repair_evictable_skeleton_score_ceiling() -> f32 {
    0.5
}

fn pack_repair_skeleton_min_score_margin() -> f32 {
    0.0
}

#[allow(clippy::too_many_arguments)]
fn repair_budget_exhausted_pack_candidates(
    purpose: ContextPackPurpose,
    max_tokens: usize,
    enable_pack_path_diversity: bool,
    enable_skeleton_reserve: bool,
    used_tokens: &mut usize,
    selected: &mut Vec<SelectedPackCandidate>,
    omitted_candidates: &mut Vec<OmittedContextCandidate>,
    budget_omitted_candidates: &[RankedPackCandidate],
    selected_by_path: &mut BTreeMap<String, usize>,
) {
    let mut repair_candidates = budget_omitted_candidates.to_vec();
    repair_candidates.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.rank_index.cmp(&right.rank_index))
    });

    for candidate in repair_candidates {
        if selected
            .iter()
            .any(|selected| selected.evidence.id == candidate.evidence.id)
        {
            continue;
        }
        if pack_evidence_path_selection_limit(
            purpose,
            &candidate.evidence,
            enable_pack_path_diversity,
        )
        .is_some_and(|limit| {
            path_selection_limit_exceeded(selected_by_path, &candidate.evidence, limit)
        }) {
            continue;
        }
        let full_content_limit =
            full_content_budget_limit(max_tokens, &candidate.evidence, enable_skeleton_reserve);
        let current_full_tokens = selected_full_content_tokens(selected);
        if current_full_tokens.saturating_add(candidate.evidence.token_estimate)
            <= full_content_limit
            && used_tokens.saturating_add(candidate.evidence.token_estimate) <= max_tokens
        {
            remove_omitted_candidate(omitted_candidates, &candidate.evidence);
            *used_tokens = used_tokens.saturating_add(candidate.evidence.token_estimate);
            record_selected_path(selected_by_path, &candidate.evidence);
            selected.push(SelectedPackCandidate {
                score: candidate.score,
                rank_index: candidate.rank_index,
                evidence: candidate.evidence,
            });
            selected.sort_by_key(|selected| selected.rank_index);
            continue;
        }

        let required_full_tokens = current_full_tokens
            .saturating_add(candidate.evidence.token_estimate)
            .saturating_sub(full_content_limit);
        let required_total_tokens = used_tokens
            .saturating_add(candidate.evidence.token_estimate)
            .saturating_sub(max_tokens);
        let required_tokens = required_full_tokens.max(required_total_tokens);

        if required_full_tokens == 0
            && required_total_tokens > 0
            && adds_new_selected_path(selected_by_path, &candidate.evidence)
        {
            if let Some(evictions) =
                repair_skeleton_evictions(selected, candidate.score, required_total_tokens)
            {
                let evicted_score: f32 = evictions.iter().map(|index| selected[*index].score).sum();
                if candidate.score >= evicted_score + pack_repair_skeleton_min_score_margin() {
                    apply_pack_repair_evictions(
                        used_tokens,
                        selected,
                        omitted_candidates,
                        selected_by_path,
                        candidate,
                        evictions,
                    );
                    continue;
                }
            }
        }

        let Some(evictions) = repair_evictions(selected, candidate.score, required_tokens) else {
            continue;
        };
        let evicted_score: f32 = evictions.iter().map(|index| selected[*index].score).sum();
        if candidate.score <= evicted_score {
            continue;
        }
        apply_pack_repair_evictions(
            used_tokens,
            selected,
            omitted_candidates,
            selected_by_path,
            candidate,
            evictions,
        );
    }
}

fn adds_new_selected_path(
    selected_by_path: &BTreeMap<String, usize>,
    evidence: &ContextEvidence,
) -> bool {
    let Some(path) = evidence.path.as_ref() else {
        return false;
    };
    !selected_by_path.contains_key(&path.display())
}

fn apply_pack_repair_evictions(
    used_tokens: &mut usize,
    selected: &mut Vec<SelectedPackCandidate>,
    omitted_candidates: &mut Vec<OmittedContextCandidate>,
    selected_by_path: &mut BTreeMap<String, usize>,
    candidate: RankedPackCandidate,
    mut evictions: Vec<usize>,
) {
    let mut evicted = Vec::new();
    evictions.sort_unstable();
    for index in evictions.into_iter().rev() {
        evicted.push(selected.remove(index));
    }
    for removed in &evicted {
        *used_tokens = used_tokens.saturating_sub(removed.evidence.token_estimate);
        unrecord_selected_path(selected_by_path, &removed.evidence);
        omitted_candidates.push(omitted_candidate(
            &removed.evidence,
            removed.score,
            removed.rank_index,
            ContextOmissionReason::BudgetExhausted,
        ));
    }
    remove_omitted_candidate(omitted_candidates, &candidate.evidence);
    *used_tokens = used_tokens.saturating_add(candidate.evidence.token_estimate);
    record_selected_path(selected_by_path, &candidate.evidence);
    selected.push(SelectedPackCandidate {
        score: candidate.score,
        rank_index: candidate.rank_index,
        evidence: candidate.evidence,
    });
    selected.sort_by_key(|selected| selected.rank_index);
}

fn repair_evictions(
    selected: &[SelectedPackCandidate],
    candidate_score: f32,
    required_tokens: usize,
) -> Option<Vec<usize>> {
    if required_tokens == 0 {
        return Some(Vec::new());
    }
    let mut removable = selected
        .iter()
        .enumerate()
        .filter(|(_, selected)| {
            selected.score < candidate_score
                && selected.score < pack_repair_evictable_score_ceiling()
                && pack_selection_limit_applies(&selected.evidence)
        })
        .collect::<Vec<_>>();
    removable.sort_by(|(_, left), (_, right)| {
        left.score
            .partial_cmp(&right.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                right
                    .evidence
                    .token_estimate
                    .cmp(&left.evidence.token_estimate)
            })
            .then_with(|| left.rank_index.cmp(&right.rank_index))
    });
    let mut freed = 0usize;
    let mut evictions = Vec::new();
    for (index, selected) in removable {
        freed = freed.saturating_add(selected.evidence.token_estimate);
        evictions.push(index);
        if freed >= required_tokens {
            return Some(evictions);
        }
    }
    None
}

fn repair_skeleton_evictions(
    selected: &[SelectedPackCandidate],
    candidate_score: f32,
    required_tokens: usize,
) -> Option<Vec<usize>> {
    let mut removable = selected
        .iter()
        .enumerate()
        .filter(|(_, selected)| {
            selected.score < candidate_score
                && selected.score < pack_repair_evictable_skeleton_score_ceiling()
                && selected.evidence.representation == ContextEvidenceRepresentation::Skeleton
        })
        .collect::<Vec<_>>();
    removable.sort_by(|(_, left), (_, right)| {
        score_per_token(left)
            .total_cmp(&score_per_token(right))
            .then_with(|| {
                left.score
                    .partial_cmp(&right.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                right
                    .evidence
                    .token_estimate
                    .cmp(&left.evidence.token_estimate)
            })
            .then_with(|| left.rank_index.cmp(&right.rank_index))
    });
    let mut freed = 0usize;
    let mut evictions = Vec::new();
    for (index, selected) in removable {
        freed = freed.saturating_add(selected.evidence.token_estimate);
        evictions.push(index);
        if freed >= required_tokens {
            return Some(evictions);
        }
    }
    None
}

fn score_per_token(selected: &SelectedPackCandidate) -> f32 {
    selected.score / selected.evidence.token_estimate.max(1) as f32
}

fn remove_omitted_candidate(
    omitted_candidates: &mut Vec<OmittedContextCandidate>,
    evidence: &ContextEvidence,
) {
    if let Some(index) = omitted_candidates
        .iter()
        .position(|candidate| candidate.evidence_id == evidence.id)
    {
        omitted_candidates.remove(index);
    }
}

const OMITTED_GRAPH_PATH_LIMIT: usize = 8;

fn graph_paths_for_omitted_candidate(
    relationships: &[ContextRelationship],
    evidence_paths_by_id: &BTreeMap<&str, &crate::runtime::contracts::RepoPath>,
    candidate: &OmittedContextCandidate,
) -> (Vec<ContextCandidateGraphPath>, usize) {
    let all_paths = relationships
        .iter()
        .filter(|relationship| {
            relationship_matches_omitted_candidate(relationship, evidence_paths_by_id, candidate)
        })
        .map(|relationship| ContextCandidateGraphPath {
            kind: relationship.kind,
            confidence: relationship.confidence,
            path: relationship.reason.clone(),
        })
        .collect::<Vec<_>>();
    let paths = all_paths
        .iter()
        .take(OMITTED_GRAPH_PATH_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    let truncated = all_paths.len().saturating_sub(paths.len());
    (paths, truncated)
}

fn relationship_matches_omitted_candidate(
    relationship: &ContextRelationship,
    evidence_paths_by_id: &BTreeMap<&str, &crate::runtime::contracts::RepoPath>,
    candidate: &OmittedContextCandidate,
) -> bool {
    if relationship.from == candidate.evidence_id || relationship.to == candidate.evidence_id {
        return true;
    }
    let Some(candidate_path) = candidate.path.as_ref() else {
        return false;
    };
    [relationship.from.0.as_str(), relationship.to.0.as_str()]
        .iter()
        .any(|evidence_id| {
            evidence_paths_by_id
                .get(evidence_id)
                .is_some_and(|path| *path == candidate_path)
        })
}

#[cfg(test)]
mod pack_selection_tests {
    use super::*;
    use crate::context_engine::{
        ContextEvidenceSource, ContextProvenance, ContextRankSignals, ContextScope,
        ContextSensitivity, ContextTrust,
    };
    use crate::runtime::contracts::{EvidenceId, RepoPath};

    fn evidence(id: &str, kind: ContextEvidenceKind, path: &str) -> ContextEvidence {
        ContextEvidence {
            id: EvidenceId(id.to_string()),
            kind,
            source: ContextEvidenceSource::Snapshot,
            trust: ContextTrust::Kernel,
            sensitivity: ContextSensitivity::Private,
            scope: ContextScope::Snapshot,
            path: Some(RepoPath::parse(path).expect("test path")),
            revision: None,
            range: None,
            content_hash: None,
            summary: None,
            is_changed_span: false,
            representation: ContextEvidenceRepresentation::FullContent,
            skeleton_text: None,
            signals: ContextRankSignals::default(),
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
    fn generic_pack_limits_repeated_nonchanged_full_content_paths() {
        let first = evidence("first", ContextEvidenceKind::FileSpan, "src/feature.ts");
        let second = evidence("second", ContextEvidenceKind::FileSpan, "src/feature.ts");
        let third = evidence("third", ContextEvidenceKind::FileSpan, "src/feature.ts");
        let mut selected_by_path = BTreeMap::new();
        record_selected_path(&mut selected_by_path, &first);
        record_selected_path(&mut selected_by_path, &second);

        assert_eq!(
            pack_path_selection_limit(ContextPackPurpose::GeneralReview, true),
            Some(2)
        );
        assert_eq!(
            pack_path_selection_limit(ContextPackPurpose::GeneralReview, false),
            None
        );
        assert!(path_selection_limit_exceeded(&selected_by_path, &third, 2));
    }

    #[test]
    fn generic_pack_limits_repeated_nonchanged_test_paths_to_one() {
        let first = evidence("first", ContextEvidenceKind::Test, "tests/feature.test.ts");
        let second = evidence("second", ContextEvidenceKind::Test, "tests/feature.test.ts");
        let mut selected_by_path = BTreeMap::new();
        record_selected_path(&mut selected_by_path, &first);

        assert_eq!(
            pack_evidence_path_selection_limit(ContextPackPurpose::GeneralReview, &second, true),
            Some(1)
        );
        assert!(path_selection_limit_exceeded(&selected_by_path, &second, 1));
    }

    #[test]
    fn path_limit_preserves_changed_diff_and_skeleton_evidence() {
        let existing = evidence("existing", ContextEvidenceKind::FileSpan, "src/feature.ts");
        let mut selected_by_path = BTreeMap::new();
        record_selected_path(&mut selected_by_path, &existing);
        record_selected_path(&mut selected_by_path, &existing);

        let mut changed = evidence("changed", ContextEvidenceKind::FileSpan, "src/feature.ts");
        changed.is_changed_span = true;
        let diff = evidence("diff", ContextEvidenceKind::Diff, "src/feature.ts");
        let mut skeleton = evidence("skeleton", ContextEvidenceKind::FileSpan, "src/feature.ts");
        skeleton.representation = ContextEvidenceRepresentation::Skeleton;

        assert!(!path_selection_limit_exceeded(
            &selected_by_path,
            &changed,
            2
        ));
        assert!(!path_selection_limit_exceeded(&selected_by_path, &diff, 2));
        assert!(!path_selection_limit_exceeded(
            &selected_by_path,
            &skeleton,
            2
        ));
    }

    #[test]
    fn repeated_path_full_content_waits_for_first_pass() {
        let existing = evidence("existing", ContextEvidenceKind::FileSpan, "src/feature.ts");
        let repeated = evidence("repeated", ContextEvidenceKind::FileSpan, "src/feature.ts");
        let other = evidence("other", ContextEvidenceKind::FileSpan, "src/other.ts");
        let diff = evidence("diff", ContextEvidenceKind::Diff, "src/feature.ts");
        let mut selected_by_path = BTreeMap::new();
        record_selected_path(&mut selected_by_path, &existing);

        assert!(repeated_path_should_wait_for_first_pass(
            &selected_by_path,
            ContextPackPurpose::GeneralReview,
            &repeated,
            true
        ));
        assert!(!repeated_path_should_wait_for_first_pass(
            &selected_by_path,
            ContextPackPurpose::GeneralReview,
            &repeated,
            false
        ));
        assert!(!repeated_path_should_wait_for_first_pass(
            &selected_by_path,
            ContextPackPurpose::GeneralReview,
            &other,
            true
        ));
        assert!(!repeated_path_should_wait_for_first_pass(
            &selected_by_path,
            ContextPackPurpose::GeneralReview,
            &diff,
            true
        ));
    }

    #[test]
    fn large_pack_keeps_tail_budget_for_skeletons() {
        let full = evidence("full", ContextEvidenceKind::FileSpan, "src/feature.ts");
        let mut changed = evidence("changed", ContextEvidenceKind::FileSpan, "src/feature.ts");
        changed.is_changed_span = true;

        assert_eq!(skeleton_reserve_tokens(12_000, true), 2_400);
        assert_eq!(skeleton_reserve_tokens(12_000, false), 0);
        assert_eq!(skeleton_reserve_tokens(3_999, true), 0);
        assert_eq!(full_content_budget_limit(12_000, &full, true), 9_600);
        assert_eq!(full_content_budget_limit(12_000, &full, false), 12_000);
        assert_eq!(full_content_budget_limit(12_000, &changed, true), 12_000);
    }

    #[test]
    fn repair_swaps_lower_score_tail_for_budget_exhausted_candidate() {
        let low = evidence("low", ContextEvidenceKind::FileSpan, "src/low.ts");
        let keep = evidence("keep", ContextEvidenceKind::FileSpan, "src/keep.ts");
        let candidate = evidence(
            "candidate",
            ContextEvidenceKind::FileSpan,
            "src/candidate.ts",
        );
        let mut selected = vec![
            SelectedPackCandidate {
                score: 0.10,
                rank_index: 2,
                evidence: low.clone(),
            },
            SelectedPackCandidate {
                score: 0.40,
                rank_index: 1,
                evidence: keep.clone(),
            },
        ];
        let mut used_tokens = selected
            .iter()
            .map(|item| item.evidence.token_estimate)
            .sum();
        let mut selected_by_path = BTreeMap::new();
        record_selected_path(&mut selected_by_path, &low);
        record_selected_path(&mut selected_by_path, &keep);
        let budget_candidate = RankedPackCandidate {
            score: 0.35,
            rank_index: 0,
            evidence: candidate.clone(),
        };
        let mut omitted_candidates = vec![omitted_candidate(
            &candidate,
            budget_candidate.score,
            budget_candidate.rank_index,
            ContextOmissionReason::BudgetExhausted,
        )];

        repair_budget_exhausted_pack_candidates(
            ContextPackPurpose::GeneralReview,
            200,
            true,
            true,
            &mut used_tokens,
            &mut selected,
            &mut omitted_candidates,
            &[budget_candidate],
            &mut selected_by_path,
        );

        let selected_ids = selected
            .iter()
            .map(|item| item.evidence.id.0.as_str())
            .collect::<Vec<_>>();
        assert_eq!(selected_ids, vec!["candidate", "keep"]);
        assert_eq!(used_tokens, 200);
        assert!(omitted_candidates
            .iter()
            .any(|candidate| candidate.evidence_id.0 == "low"));
        assert!(!omitted_candidates
            .iter()
            .any(|omitted| omitted.evidence_id.0 == "candidate"));
    }

    #[test]
    fn repair_does_not_evict_protected_or_higher_score_evidence() {
        let mut changed = evidence("changed", ContextEvidenceKind::FileSpan, "src/changed.ts");
        changed.is_changed_span = true;
        let higher = evidence("higher", ContextEvidenceKind::FileSpan, "src/higher.ts");
        let candidate = evidence(
            "candidate",
            ContextEvidenceKind::FileSpan,
            "src/candidate.ts",
        );
        let mut selected = vec![
            SelectedPackCandidate {
                score: 0.05,
                rank_index: 0,
                evidence: changed.clone(),
            },
            SelectedPackCandidate {
                score: 0.55,
                rank_index: 1,
                evidence: higher.clone(),
            },
        ];
        let mut used_tokens = selected
            .iter()
            .map(|item| item.evidence.token_estimate)
            .sum();
        let mut selected_by_path = BTreeMap::new();
        record_selected_path(&mut selected_by_path, &changed);
        record_selected_path(&mut selected_by_path, &higher);
        let budget_candidate = RankedPackCandidate {
            score: 0.60,
            rank_index: 2,
            evidence: candidate.clone(),
        };
        let mut omitted_candidates = vec![omitted_candidate(
            &candidate,
            budget_candidate.score,
            budget_candidate.rank_index,
            ContextOmissionReason::BudgetExhausted,
        )];

        repair_budget_exhausted_pack_candidates(
            ContextPackPurpose::GeneralReview,
            200,
            true,
            true,
            &mut used_tokens,
            &mut selected,
            &mut omitted_candidates,
            &[budget_candidate],
            &mut selected_by_path,
        );

        let selected_ids = selected
            .iter()
            .map(|item| item.evidence.id.0.as_str())
            .collect::<Vec<_>>();
        assert_eq!(selected_ids, vec!["changed", "higher"]);
        assert_eq!(used_tokens, 200);
        assert!(omitted_candidates
            .iter()
            .any(|omitted| omitted.evidence_id.0 == "candidate"));
    }

    #[test]
    fn repair_swaps_lower_score_skeleton_tail_for_full_content_candidate() {
        let mut keep = evidence("keep", ContextEvidenceKind::FileSpan, "src/keep.ts");
        keep.token_estimate = 180;
        let mut tail_skeleton = evidence(
            "tail-skeleton",
            ContextEvidenceKind::FileSpan,
            "src/tail.ts",
        );
        tail_skeleton.representation = ContextEvidenceRepresentation::Skeleton;
        tail_skeleton.token_estimate = 100;
        let mut candidate = evidence(
            "candidate",
            ContextEvidenceKind::FileSpan,
            "src/candidate.ts",
        );
        candidate.token_estimate = 80;
        let mut selected = vec![
            SelectedPackCandidate {
                score: 0.60,
                rank_index: 0,
                evidence: keep.clone(),
            },
            SelectedPackCandidate {
                score: 0.30,
                rank_index: 1,
                evidence: tail_skeleton.clone(),
            },
        ];
        for index in 0..8 {
            let mut tiny = evidence(
                &format!("tiny-{index}"),
                ContextEvidenceKind::FileSpan,
                &format!("src/tiny-{index}.ts"),
            );
            tiny.representation = ContextEvidenceRepresentation::Skeleton;
            tiny.token_estimate = 5;
            selected.push(SelectedPackCandidate {
                score: 0.08,
                rank_index: index + 2,
                evidence: tiny,
            });
        }
        let mut used_tokens = selected
            .iter()
            .map(|item| item.evidence.token_estimate)
            .sum();
        let mut selected_by_path = BTreeMap::new();
        for selected_candidate in &selected {
            record_selected_path(&mut selected_by_path, &selected_candidate.evidence);
        }
        let budget_candidate = RankedPackCandidate {
            score: 0.45,
            rank_index: 10,
            evidence: candidate.clone(),
        };
        let mut omitted_candidates = vec![omitted_candidate(
            &candidate,
            budget_candidate.score,
            budget_candidate.rank_index,
            ContextOmissionReason::BudgetExhausted,
        )];

        repair_budget_exhausted_pack_candidates(
            ContextPackPurpose::GeneralReview,
            360,
            true,
            true,
            &mut used_tokens,
            &mut selected,
            &mut omitted_candidates,
            &[budget_candidate],
            &mut selected_by_path,
        );

        let selected_ids = selected
            .iter()
            .map(|item| item.evidence.id.0.as_str())
            .collect::<Vec<_>>();
        assert!(!selected_ids.contains(&"tail-skeleton"));
        assert!(selected_ids.contains(&"candidate"));
        assert_eq!(used_tokens, 300);
        assert!(omitted_candidates
            .iter()
            .any(|omitted| omitted.evidence_id.0 == "tail-skeleton"));
        assert!(!omitted_candidates
            .iter()
            .any(|omitted| omitted.evidence_id.0 == "candidate"));
    }

    #[test]
    fn skeleton_tail_repair_requires_new_path_coverage() {
        let mut covered = evidence("covered", ContextEvidenceKind::FileSpan, "src/covered.ts");
        covered.token_estimate = 180;
        let mut tail_skeleton = evidence(
            "tail-skeleton",
            ContextEvidenceKind::FileSpan,
            "src/tail.ts",
        );
        tail_skeleton.representation = ContextEvidenceRepresentation::Skeleton;
        tail_skeleton.token_estimate = 100;
        let mut duplicate_candidate =
            evidence("candidate", ContextEvidenceKind::FileSpan, "src/covered.ts");
        duplicate_candidate.token_estimate = 80;
        let mut selected = vec![
            SelectedPackCandidate {
                score: 0.60,
                rank_index: 0,
                evidence: covered.clone(),
            },
            SelectedPackCandidate {
                score: 0.30,
                rank_index: 1,
                evidence: tail_skeleton.clone(),
            },
        ];
        let mut used_tokens = selected
            .iter()
            .map(|item| item.evidence.token_estimate)
            .sum();
        let mut selected_by_path = BTreeMap::new();
        for selected_candidate in &selected {
            record_selected_path(&mut selected_by_path, &selected_candidate.evidence);
        }
        let budget_candidate = RankedPackCandidate {
            score: 0.50,
            rank_index: 2,
            evidence: duplicate_candidate.clone(),
        };
        let mut omitted_candidates = vec![omitted_candidate(
            &duplicate_candidate,
            budget_candidate.score,
            budget_candidate.rank_index,
            ContextOmissionReason::BudgetExhausted,
        )];

        repair_budget_exhausted_pack_candidates(
            ContextPackPurpose::GeneralReview,
            300,
            true,
            true,
            &mut used_tokens,
            &mut selected,
            &mut omitted_candidates,
            &[budget_candidate],
            &mut selected_by_path,
        );

        let selected_ids = selected
            .iter()
            .map(|item| item.evidence.id.0.as_str())
            .collect::<Vec<_>>();
        assert_eq!(selected_ids, vec!["covered", "tail-skeleton"]);
        assert_eq!(used_tokens, 280);
        assert!(omitted_candidates
            .iter()
            .any(|omitted| omitted.evidence_id.0 == "candidate"));
    }

    #[test]
    fn skeleton_tail_repair_accepts_positive_score_swap_without_extra_margin() {
        let mut keep = evidence("keep", ContextEvidenceKind::FileSpan, "src/keep.ts");
        keep.token_estimate = 195;
        let mut tail_skeleton = evidence(
            "tail-skeleton",
            ContextEvidenceKind::FileSpan,
            "src/tail.ts",
        );
        tail_skeleton.representation = ContextEvidenceRepresentation::Skeleton;
        tail_skeleton.token_estimate = 5;
        let mut candidate = evidence(
            "candidate",
            ContextEvidenceKind::FileSpan,
            "src/candidate.ts",
        );
        candidate.token_estimate = 5;

        let mut selected = vec![
            SelectedPackCandidate {
                score: 0.60,
                rank_index: 0,
                evidence: keep.clone(),
            },
            SelectedPackCandidate {
                score: 0.37,
                rank_index: 1,
                evidence: tail_skeleton.clone(),
            },
        ];
        let mut used_tokens = selected
            .iter()
            .map(|item| item.evidence.token_estimate)
            .sum();
        let mut selected_by_path = BTreeMap::new();
        for selected_candidate in &selected {
            record_selected_path(&mut selected_by_path, &selected_candidate.evidence);
        }
        let budget_candidate = RankedPackCandidate {
            score: 0.38,
            rank_index: 2,
            evidence: candidate.clone(),
        };
        let mut omitted_candidates = vec![omitted_candidate(
            &candidate,
            budget_candidate.score,
            budget_candidate.rank_index,
            ContextOmissionReason::BudgetExhausted,
        )];

        repair_budget_exhausted_pack_candidates(
            ContextPackPurpose::GeneralReview,
            200,
            true,
            true,
            &mut used_tokens,
            &mut selected,
            &mut omitted_candidates,
            &[budget_candidate],
            &mut selected_by_path,
        );

        let selected_ids = selected
            .iter()
            .map(|item| item.evidence.id.0.as_str())
            .collect::<Vec<_>>();
        assert_eq!(selected_ids, vec!["keep", "candidate"]);
        assert_eq!(used_tokens, 200);
        assert!(omitted_candidates
            .iter()
            .any(|omitted| omitted.evidence_id.0 == "tail-skeleton"));
        assert!(!omitted_candidates
            .iter()
            .any(|omitted| omitted.evidence_id.0 == "candidate"));
    }
}

#[async_trait]
impl ContextEngine for SnapshotContextEngine {
    fn config(&self) -> ContextEngineConfig {
        self.config.clone()
    }

    fn get_index(&self, snapshot_id: &SnapshotId) -> Option<Arc<ContextIndex>> {
        self.store.get_index(snapshot_id)
    }

    async fn index_snapshot(
        &self,
        mut request: ContextIndexRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextIndexReport> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if self.config.mode == ContextEngineMode::Disabled {
            return Err(RuntimeError::InvalidInput(
                "context engine is disabled".to_string(),
            ));
        }
        request.derived_cache = Arc::clone(&self.derived_cache);
        let index = ContextIndex::build(request).await?;
        let report = index.report.clone();
        self.store.put_index(index)?;
        Ok(report)
    }

    async fn build_pack(
        &self,
        request: ContextPackRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextPack> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let index = self.store.get_index(&request.snapshot_id).ok_or_else(|| {
            RuntimeError::InvalidInput("context index not found for snapshot".to_string())
        })?;
        let mut ranked = rank_for_purpose(&index.evidence, request.purpose, &self.config);
        // Degradation ladder (R7), applied at each candidate's turn in
        // rank order: full content when it fits, the chunk's
        // signatures-only skeleton twin when only that fits, omission
        // otherwise. A candidate enters as exactly one representation,
        // so a chunk and its skeleton are never both included.
        let mut used_tokens = 0usize;
        let mut selected: Vec<SelectedPackCandidate> = Vec::new();
        let mut omitted_candidates = Vec::new();
        let mut budget_omitted_candidates = Vec::new();
        let mut selected_by_path: BTreeMap<String, usize> = BTreeMap::new();
        let mut deferred_repeated_paths = Vec::new();
        let mut rank_index = 0usize;
        for (score, evidence) in ranked.drain(..) {
            let candidate = RankedPackCandidate {
                score,
                rank_index,
                evidence,
            };
            rank_index = rank_index.saturating_add(1);
            if repeated_path_should_wait_for_first_pass(
                &selected_by_path,
                request.purpose,
                &candidate.evidence,
                self.config.enable_pack_path_diversity,
            ) {
                deferred_repeated_paths.push(candidate);
                continue;
            }
            select_ranked_pack_candidate(
                request.purpose,
                request.max_tokens,
                self.config.enable_pack_path_diversity,
                self.config.enable_skeleton_reserve,
                &index.skeletons,
                &mut used_tokens,
                &mut selected,
                &mut omitted_candidates,
                &mut budget_omitted_candidates,
                &mut selected_by_path,
                candidate,
            );
        }
        for candidate in deferred_repeated_paths {
            select_ranked_pack_candidate(
                request.purpose,
                request.max_tokens,
                self.config.enable_pack_path_diversity,
                self.config.enable_skeleton_reserve,
                &index.skeletons,
                &mut used_tokens,
                &mut selected,
                &mut omitted_candidates,
                &mut budget_omitted_candidates,
                &mut selected_by_path,
                candidate,
            );
        }
        if self.config.enable_pack_repair {
            repair_budget_exhausted_pack_candidates(
                request.purpose,
                request.max_tokens,
                self.config.enable_pack_path_diversity,
                self.config.enable_skeleton_reserve,
                &mut used_tokens,
                &mut selected,
                &mut omitted_candidates,
                &budget_omitted_candidates,
                &mut selected_by_path,
            );
        }
        let selected_candidates = selected
            .iter()
            .map(|selected| SelectedContextCandidate {
                evidence_id: selected.evidence.id.clone(),
                score: selected.score,
                rank_index: selected.rank_index,
            })
            .collect::<Vec<_>>();
        let selected = selected
            .into_iter()
            .map(|selected| selected.evidence)
            .collect::<Vec<_>>();
        let selected_ids: std::collections::BTreeSet<&str> = selected
            .iter()
            .map(|evidence| evidence.id.0.as_str())
            .collect();
        let relationships: Vec<_> = index
            .relationships
            .iter()
            .filter(|relationship| {
                selected_ids.contains(relationship.from.0.as_str())
                    && selected_ids.contains(relationship.to.0.as_str())
            })
            .cloned()
            .collect();
        let evidence_paths_by_id: BTreeMap<&str, &crate::runtime::contracts::RepoPath> = index
            .evidence
            .iter()
            .filter_map(|evidence| {
                evidence
                    .path
                    .as_ref()
                    .map(|path| (evidence.id.0.as_str(), path))
            })
            .collect();
        for omitted in &mut omitted_candidates {
            let (graph_paths, truncated) = graph_paths_for_omitted_candidate(
                &index.relationships,
                &evidence_paths_by_id,
                omitted,
            );
            omitted.graph_paths = graph_paths;
            omitted.graph_paths_truncated = truncated;
        }
        // Both reasons mean full content did not fit within budget.
        let budget_exhausted = omitted_candidates.iter().any(|candidate| {
            matches!(
                candidate.reason,
                ContextOmissionReason::BudgetExhausted
                    | ContextOmissionReason::DowngradedToSkeleton
            )
        });
        let mut sufficiency = super::evaluate_sufficiency(&index, &selected, budget_exhausted);
        if budget_exhausted && sufficiency.status != ContextSufficiencyStatus::Insufficient {
            sufficiency.status = ContextSufficiencyStatus::Insufficient;
            sufficiency.missing.push(
                "pack omitted ranked candidates under budget; context is incomplete".to_string(),
            );
        } else if !omitted_candidates.is_empty()
            && sufficiency.status == ContextSufficiencyStatus::Sufficient
        {
            sufficiency.status = ContextSufficiencyStatus::ProbablySufficient;
            sufficiency.missing.push(
                "pack omitted ranked candidates under budget; complete coverage is unproven"
                    .to_string(),
            );
        }
        let pack_id = ContextPackId(stable_id(&[
            &request.snapshot_id.0,
            request
                .session_id
                .as_ref()
                .map(|id| id.0.as_str())
                .unwrap_or("standalone"),
            purpose_name(request.purpose),
            &used_tokens.to_string(),
            CONTEXT_ENGINE_VERSION,
        ]));
        let pack = ContextPack {
            id: pack_id,
            run_id: request.run_id,
            snapshot_id: request.snapshot_id,
            session_id: request.session_id,
            purpose: request.purpose,
            evidence: selected,
            selected_candidates,
            relationships,
            omitted_candidates,
            budget: ContextBudgetUsage {
                max_tokens: request.max_tokens,
                used_tokens,
            },
            sufficiency,
            compiler_version: CONTEXT_ENGINE_VERSION.to_string(),
            created_at_utc: unix_timestamp_string(),
        };
        self.packs
            .lock()
            .expect("context pack store poisoned")
            .insert(pack.id.0.clone(), pack.clone());
        Ok(pack)
    }

    async fn query(
        &self,
        request: ContextQuery,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextQueryResult> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let index = self.store.get_index(&request.snapshot_id).ok_or_else(|| {
            RuntimeError::InvalidInput("context index not found for snapshot".to_string())
        })?;
        let limit = request.limits.max_results.max(1);
        match request.kind {
            ContextQueryKind::SearchText => {
                let query = string_arg(&request.arguments, "query")?;
                let outcome = fused_search(
                    &index,
                    &query,
                    limit,
                    self.config.bm25_k1,
                    self.config.bm25_b,
                    self.config.rrf_k,
                )
                .await?;
                let mut data_value = serde_json::json!({
                    "fusion": outcome.fusion,
                    "fusionOmissions": outcome.omissions,
                });
                if !outcome.degraded.is_empty() {
                    data_value["degraded"] = serde_json::json!(outcome.degraded);
                }
                let data = Some(data_value);
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence: outcome.evidence,
                    sufficiency: None,
                    data,
                    omitted: index.evidence.len().saturating_sub(limit),
                })
            }
            ContextQueryKind::RelatedTests => {
                let path = string_arg(&request.arguments, "path").unwrap_or_default();
                let path_stem = path_stem(&path);
                // Tests connected through the Context Graph's `Tests`
                // edges rank above path-stem matches.
                let graph_test_paths: std::collections::BTreeSet<_> =
                    crate::runtime::contracts::RepoPath::parse(&path)
                        .map(|query_path| {
                            index
                                .graph
                                .file_referencers(&query_path)
                                .filter(|edge| edge.kind == super::graph::ContextEdgeKind::Tests)
                                .filter_map(|edge| edge.from_path().cloned())
                                .collect()
                        })
                        .unwrap_or_default();
                let mut ranked = index
                    .evidence
                    .iter()
                    .filter(|evidence| evidence.kind == ContextEvidenceKind::Test)
                    .filter_map(|evidence| {
                        let in_graph = evidence
                            .path
                            .as_ref()
                            .map(|path| graph_test_paths.contains(path))
                            .unwrap_or(false);
                        let stem_match = !path_stem.is_empty()
                            && (evidence
                                .path
                                .as_ref()
                                .map(|path| path.display().contains(&path_stem))
                                .unwrap_or(false)
                                || evidence
                                    .summary
                                    .as_ref()
                                    .map(|summary| summary.contains(&path_stem))
                                    .unwrap_or(false));
                        let score = match (in_graph, stem_match) {
                            (true, true) => 3u8,
                            (true, false) => 2,
                            (false, true) => 1,
                            (false, false) => return path_stem.is_empty().then_some((0, evidence)),
                        };
                        Some((score, evidence))
                    })
                    .collect::<Vec<_>>();
                ranked.sort_by(|(left_score, left), (right_score, right)| {
                    right_score
                        .cmp(left_score)
                        .then_with(|| left.id.0.cmp(&right.id.0))
                });
                let omitted = ranked.len().saturating_sub(limit);
                let evidence = ranked
                    .into_iter()
                    .take(limit)
                    .map(|(_, evidence)| evidence.clone())
                    .collect::<Vec<_>>();
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence,
                    sufficiency: None,
                    data: None,
                    omitted,
                })
            }
            ContextQueryKind::RelatedSymbols => {
                let path = string_arg(&request.arguments, "path").unwrap_or_default();
                let explicit_symbol = request
                    .arguments
                    .get("symbol")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let terms = related_symbol_terms(&index.evidence, &path, explicit_symbol);
                let mut ranked = index
                    .evidence
                    .iter()
                    .filter(|evidence| {
                        evidence.kind == ContextEvidenceKind::Symbol
                            || evidence.kind == ContextEvidenceKind::FileSpan
                    })
                    .filter_map(|evidence| {
                        related_symbol_score(
                            evidence,
                            &index.file_contents,
                            &index.graph,
                            &path,
                            &terms,
                        )
                        .map(|score| (score, evidence.clone()))
                    })
                    .collect::<Vec<_>>();
                ranked.sort_by(|(left_score, left), (right_score, right)| {
                    right_score
                        .cmp(left_score)
                        .then_with(|| left.id.0.cmp(&right.id.0))
                });
                let omitted = ranked.len().saturating_sub(limit);
                let evidence = ranked
                    .into_iter()
                    .take(limit)
                    .map(|(_, evidence)| evidence)
                    .collect::<Vec<_>>();
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence,
                    sufficiency: None,
                    data: Some(serde_json::json!({
                        "path": path,
                        "terms": terms,
                    })),
                    omitted,
                })
            }
            ContextQueryKind::TicketRequirements => {
                let query = request
                    .arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let mut evidence = index
                    .evidence
                    .iter()
                    .filter(|evidence| evidence.kind == ContextEvidenceKind::Ticket)
                    .filter(|evidence| {
                        query.is_empty()
                            || evidence
                                .summary
                                .as_ref()
                                .map(|summary| summary.to_ascii_lowercase().contains(&query))
                                .unwrap_or(false)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                evidence.sort_by(|left, right| {
                    trust_rank(right.trust)
                        .cmp(&trust_rank(left.trust))
                        .then_with(|| left.id.0.cmp(&right.id.0))
                });
                let omitted = evidence.len().saturating_sub(limit);
                evidence.truncate(limit);
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence,
                    sufficiency: None,
                    data: None,
                    omitted,
                })
            }
            ContextQueryKind::HistorySimilar => {
                let query = request
                    .arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let learnings = self
                    .learnings
                    .list_learnings()
                    .into_iter()
                    .filter(|learning| {
                        learning.status == ContextLearningStatus::Approved
                            && !learning_is_expired(learning)
                    })
                    .filter(|learning| learning.snapshot_id == request.snapshot_id)
                    .filter(|learning| {
                        query.is_empty() || learning.summary.to_ascii_lowercase().contains(&query)
                    })
                    .take(limit)
                    .collect::<Vec<_>>();
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence: Vec::new(),
                    sufficiency: None,
                    data: Some(serde_json::json!({
                        "learnings": learnings,
                        "status": "approved_only"
                    })),
                    omitted: 0,
                })
            }
            ContextQueryKind::CrossRepoContracts => {
                let query = request
                    .arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let mut evidence = index
                    .evidence
                    .iter()
                    .filter(|evidence| evidence.kind == ContextEvidenceKind::CrossRepoContract)
                    .filter(|evidence| {
                        query.is_empty()
                            || evidence
                                .summary
                                .as_ref()
                                .map(|summary| summary.to_ascii_lowercase().contains(&query))
                                .unwrap_or(false)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                evidence.sort_by(|left, right| {
                    trust_rank(right.trust)
                        .cmp(&trust_rank(left.trust))
                        .then_with(|| left.id.0.cmp(&right.id.0))
                });
                let omitted = evidence.len().saturating_sub(limit);
                evidence.truncate(limit);
                let data = if evidence.is_empty() {
                    Some(serde_json::json!({
                        "omissions": [{
                            "reason": "requires_ungranted_capability",
                            "capability": "network_read",
                            "deniedCandidates": index.denied_cross_repo_contracts,
                            "message": "cross-repo contracts require host-provided evidence or an explicitly granted network/provider capability"
                        }]
                    }))
                } else {
                    Some(serde_json::json!({
                        "omissions": [],
                        "deniedCandidates": index.denied_cross_repo_contracts
                    }))
                };
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence,
                    sufficiency: None,
                    data,
                    omitted,
                })
            }
            ContextQueryKind::ReadSpan => {
                let path = string_arg(&request.arguments, "path")?;
                let start_line = usize_arg(&request.arguments, "startLine")
                    .or_else(|_| usize_arg(&request.arguments, "start_line"))?;
                let end_line = usize_arg(&request.arguments, "endLine")
                    .or_else(|_| usize_arg(&request.arguments, "end_line"))?;
                let repo_path = crate::runtime::contracts::RepoPath::parse(&path)?;
                let content = index.file_contents.get(&repo_path).ok_or_else(|| {
                    RuntimeError::InvalidInput("context read_span path not indexed".to_string())
                })?;
                let snippet =
                    redact_context_content(&read_line_span(content, start_line, end_line)?);
                let requested = ContextRange {
                    start_line: start_line.try_into().unwrap_or(u32::MAX),
                    end_line: end_line.try_into().unwrap_or(u32::MAX),
                };
                let by_path =
                    |evidence: &&ContextEvidence| evidence.path.as_ref() == Some(&repo_path);
                let evidence = index
                    .evidence
                    .iter()
                    .filter(by_path)
                    .find(|evidence| {
                        evidence.range.as_ref().is_some_and(|range| {
                            range.start_line <= requested.end_line
                                && requested.start_line <= range.end_line
                        })
                    })
                    .or_else(|| index.evidence.iter().find(by_path))
                    .cloned()
                    .into_iter()
                    .collect::<Vec<_>>();
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence,
                    sufficiency: None,
                    data: Some(serde_json::json!({
                        "path": path,
                        "startLine": start_line,
                        "endLine": end_line,
                        "content": snippet,
                    })),
                    omitted: 0,
                })
            }
            ContextQueryKind::SufficiencyCheck => {
                let mut evidence = evidence_by_id(&index.evidence, &request.current_evidence);
                // Packs can cite skeleton evidence (R7); resolve those
                // ids too so a check over pack ids sees the same set.
                evidence.extend(
                    index
                        .skeletons
                        .values()
                        .filter(|skeleton| {
                            request.current_evidence.iter().any(|id| id == &skeleton.id)
                        })
                        .cloned(),
                );
                let sufficiency = super::evaluate_sufficiency(&index, &evidence, false);
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence,
                    sufficiency: Some(sufficiency),
                    data: None,
                    omitted: 0,
                })
            }
            ContextQueryKind::ExplainPack => {
                let pack_id = string_arg(&request.arguments, "packId")
                    .or_else(|_| string_arg(&request.arguments, "pack_id"))?;
                let include_omitted = request
                    .arguments
                    .get("includeOmitted")
                    .or_else(|| request.arguments.get("include_omitted"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let pack = self
                    .packs
                    .lock()
                    .expect("context pack store poisoned")
                    .get(&pack_id)
                    .cloned()
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(format!("context pack {pack_id} not found"))
                    })?;
                let included = pack
                    .evidence
                    .iter()
                    .map(|evidence| {
                        let selected_candidate = pack
                            .selected_candidates
                            .iter()
                            .find(|candidate| candidate.evidence_id == evidence.id);
                        // Graph paths are the source of explainability:
                        // typed relationships carry the rendered Context
                        // Graph path that connected this evidence to the
                        // change.
                        let graph_paths = pack
                            .relationships
                            .iter()
                            .filter(|relationship| {
                                relationship.from == evidence.id || relationship.to == evidence.id
                            })
                            .map(|relationship| {
                                serde_json::json!({
                                    "kind": relationship.kind,
                                    "confidence": relationship.confidence,
                                    "path": relationship.reason,
                                })
                            })
                            .collect::<Vec<_>>();
                        serde_json::json!({
                            "evidenceId": evidence.id.0,
                            "kind": evidence.kind,
                            "path": evidence.path.as_ref().map(|path| path.display()),
                            "score": selected_candidate
                                .map(|candidate| candidate.score)
                                .unwrap_or_else(|| score_for_purpose(evidence, pack.purpose, &self.config)),
                            "rankIndex": selected_candidate.map(|candidate| candidate.rank_index),
                            "tokenEstimate": evidence.token_estimate,
                            "why": explain_selected_evidence(evidence, pack.purpose),
                            "graphPaths": graph_paths,
                        })
                    })
                    .collect::<Vec<_>>();
                let omitted = include_omitted.then(|| {
                    pack.omitted_candidates
                        .iter()
                        .map(|candidate| {
                            serde_json::json!({
                                "evidenceId": candidate.evidence_id.0,
                                "kind": candidate.kind,
                                "path": candidate.path.as_ref().map(|path| path.display()),
                                "score": candidate.score,
                                "rankIndex": candidate.rank_index,
                                "tokenEstimate": candidate.token_estimate,
                                "reason": candidate.reason,
                                "graphPaths": candidate.graph_paths,
                                "graphPathsTruncated": candidate.graph_paths_truncated,
                            })
                        })
                        .collect::<Vec<_>>()
                });
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence: pack.evidence.clone(),
                    sufficiency: Some(pack.sufficiency.clone()),
                    data: Some(serde_json::json!({
                        "packId": pack.id.0,
                        "purpose": pack.purpose,
                        "included": included,
                        "omitted": omitted.unwrap_or_default(),
                    })),
                    omitted: pack.omitted_candidates.len(),
                })
            }
        }
    }

    async fn record_feedback(
        &self,
        feedback: ContextFeedback,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ContextFeedbackReceipt> {
        if self.store.get_index(&feedback.snapshot_id).is_none() {
            return Err(RuntimeError::InvalidInput(
                "context index not found for feedback snapshot".to_string(),
            ));
        }
        let summary = feedback.feedback.trim();
        if summary.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "context feedback cannot be empty".to_string(),
            ));
        }
        let learning = ContextLearning {
            id: stable_id(&[
                &feedback.snapshot_id.0,
                "learning",
                summary,
                &feedback.evidence_ids.len().to_string(),
            ]),
            snapshot_id: feedback.snapshot_id,
            source: feedback
                .source
                .unwrap_or(ContextLearningSource::HumanFeedback),
            status: ContextLearningStatus::Proposed,
            scope: feedback.scope.unwrap_or(ContextLearningScope::Repository),
            evidence_ids: feedback.evidence_ids,
            summary: summary.to_string(),
            created_at_utc: unix_timestamp_string(),
            expires_at_utc: None,
        };
        self.learnings.put_learning(learning.clone())?;
        Ok(ContextFeedbackReceipt {
            accepted: true,
            message: "stored proposed context learning; approval required before retrieval"
                .to_string(),
            proposed_learning: Some(learning),
        })
    }

    async fn approve_learning(
        &self,
        approval: ContextLearningApproval,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextLearningApprovalReceipt> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let mut update = |learning: &mut ContextLearning| {
            if learning.status != ContextLearningStatus::Proposed {
                return Err(RuntimeError::InvalidInput(
                    "only proposed context learnings can be approved or rejected".to_string(),
                ));
            }
            learning.status = if approval.approve {
                ContextLearningStatus::Approved
            } else {
                ContextLearningStatus::Rejected
            };
            learning.expires_at_utc = approval.expires_at_utc.clone();
            Ok(())
        };
        let learning = self
            .learnings
            .update_learning(&approval.learning_id, &mut update)?;
        Ok(ContextLearningApprovalReceipt {
            accepted: true,
            learning,
        })
    }
}
