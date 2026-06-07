use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::runtime::contracts::{stable_id, EvidenceId, RuntimeError, RuntimeResult, SnapshotId};

use super::semantic_score_for_purpose;
use super::ContextLearningStore;
use super::{
    ContextBudgetUsage, ContextEngineConfig, ContextEngineMode, ContextEvidence,
    ContextEvidenceKind, ContextFeedback, ContextFeedbackReceipt, ContextIndex, ContextIndexReport,
    ContextIndexRequest, ContextIndexStore, ContextLearning, ContextLearningApproval,
    ContextLearningApprovalReceipt, ContextLearningScope, ContextLearningSource,
    ContextLearningStatus, ContextOmissionReason, ContextPack, ContextPackId, ContextPackPurpose,
    ContextPackRequest, ContextQuery, ContextQueryKind, ContextQueryResult, ContextSufficiency,
    ContextSufficiencyStatus, FileContextLearningStore, InMemoryContextIndexStore,
    InMemoryContextLearningStore, OmittedContextCandidate, CONTEXT_ENGINE_VERSION,
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
}

impl Clone for SnapshotContextEngine {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            store: Arc::clone(&self.store),
            packs: Arc::clone(&self.packs),
            learnings: Arc::clone(&self.learnings),
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
        request: ContextIndexRequest,
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
        let index = ContextIndex::build(request)?;
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
        let mut used_tokens = 0usize;
        let mut selected = Vec::new();
        let mut omitted_candidates = Vec::new();
        for (score, evidence) in ranked.drain(..) {
            if used_tokens.saturating_add(evidence.token_estimate) <= request.max_tokens {
                used_tokens = used_tokens.saturating_add(evidence.token_estimate);
                selected.push(evidence);
            } else {
                omitted_candidates.push(OmittedContextCandidate {
                    evidence_id: evidence.id,
                    kind: evidence.kind,
                    path: evidence.path,
                    score,
                    token_estimate: evidence.token_estimate,
                    reason: ContextOmissionReason::BudgetExhausted,
                });
            }
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
            relationships: Vec::new(),
            omitted_candidates,
            budget: ContextBudgetUsage {
                max_tokens: request.max_tokens,
                used_tokens,
            },
            sufficiency: ContextSufficiency::probably_sufficient(),
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
                let matches = search_evidence(&index.evidence, &index.file_contents, &query, limit);
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence: matches,
                    sufficiency: None,
                    data: None,
                    omitted: index.evidence.len().saturating_sub(limit),
                })
            }
            ContextQueryKind::RelatedTests => {
                let path = string_arg(&request.arguments, "path").unwrap_or_default();
                let path_stem = path_stem(&path);
                let evidence = index
                    .evidence
                    .iter()
                    .filter(|evidence| evidence.kind == ContextEvidenceKind::Test)
                    .filter(|evidence| {
                        path_stem.is_empty()
                            || evidence
                                .path
                                .as_ref()
                                .map(|path| path.display().contains(&path_stem))
                                .unwrap_or(false)
                            || evidence
                                .summary
                                .as_ref()
                                .map(|summary| summary.contains(&path_stem))
                                .unwrap_or(false)
                    })
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence,
                    sufficiency: None,
                    data: None,
                    omitted: 0,
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
                            &index.symbol_graph,
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
                            "message": "cross-repo contracts require host-provided evidence or an explicitly granted network/provider capability"
                        }]
                    }))
                } else {
                    Some(serde_json::json!({
                        "omissions": []
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
                let evidence = index
                    .evidence
                    .iter()
                    .find(|evidence| evidence.path.as_ref() == Some(&repo_path))
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
                let status = if request.current_evidence.is_empty() {
                    ContextSufficiencyStatus::Insufficient
                } else {
                    ContextSufficiencyStatus::ProbablySufficient
                };
                let missing = if status == ContextSufficiencyStatus::Insufficient {
                    vec!["primary source evidence".to_string()]
                } else {
                    Vec::new()
                };
                Ok(ContextQueryResult {
                    kind: request.kind,
                    evidence: evidence_by_id(&index.evidence, &request.current_evidence),
                    sufficiency: Some(ContextSufficiency { status, missing }),
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
                        serde_json::json!({
                            "evidenceId": evidence.id.0,
                            "score": score_for_purpose(evidence, pack.purpose, &self.config),
                            "why": explain_selected_evidence(evidence, pack.purpose),
                        })
                    })
                    .collect::<Vec<_>>();
                let omitted = include_omitted.then(|| {
                    pack.omitted_candidates
                        .iter()
                        .map(|candidate| {
                            serde_json::json!({
                                "evidenceId": candidate.evidence_id.0,
                                "score": candidate.score,
                                "reason": candidate.reason,
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

fn trust_rank(trust: super::ContextTrust) -> u8 {
    match trust {
        super::ContextTrust::Kernel => 6,
        super::ContextTrust::HostTrusted => 5,
        super::ContextTrust::OrganizationTrusted => 4,
        super::ContextTrust::ToolProvider => 3,
        super::ContextTrust::RepositoryUntrusted => 2,
        super::ContextTrust::UserUntrusted => 1,
        super::ContextTrust::ExternalUntrusted => 0,
    }
}

fn rank_for_purpose(
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

fn score_for_purpose(
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

fn token_efficiency_bonus(tokens: usize) -> f32 {
    if tokens <= 250 {
        0.15
    } else if tokens <= 1_000 {
        0.08
    } else {
        0.0
    }
}

fn explain_selected_evidence(
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

fn purpose_name(purpose: ContextPackPurpose) -> &'static str {
    purpose.as_str()
}

fn unix_timestamp_string() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!("{seconds}")
}

fn learning_is_expired(learning: &ContextLearning) -> bool {
    let Some(expires_at) = &learning.expires_at_utc else {
        return false;
    };
    let Ok(expires_at) = expires_at.parse::<u64>() else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    expires_at <= now
}

fn string_arg(arguments: &Value, key: &str) -> RuntimeResult<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("context query requires {key}")))
}

fn usize_arg(arguments: &Value, key: &str) -> RuntimeResult<usize> {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| RuntimeError::InvalidInput(format!("context query requires {key}")))
}

fn search_evidence(
    evidence: &[ContextEvidence],
    file_contents: &std::collections::BTreeMap<crate::runtime::contracts::RepoPath, String>,
    query: &str,
    limit: usize,
) -> Vec<ContextEvidence> {
    let terms = query
        .split('|')
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return Vec::new();
    }
    evidence
        .iter()
        .filter(|evidence| {
            let haystack = format!(
                "{} {} {}",
                evidence
                    .path
                    .as_ref()
                    .map(|path| path.display())
                    .unwrap_or_default(),
                evidence.summary.as_deref().unwrap_or(""),
                evidence
                    .path
                    .as_ref()
                    .and_then(|path| file_contents.get(path))
                    .map(String::as_str)
                    .unwrap_or("")
            )
            .to_ascii_lowercase();
            terms.iter().any(|term| haystack.contains(term))
        })
        .take(limit)
        .cloned()
        .collect()
}

fn read_line_span(content: &str, start_line: usize, end_line: usize) -> RuntimeResult<String> {
    if start_line == 0 || end_line < start_line {
        return Err(RuntimeError::InvalidInput(
            "invalid context read_span line range".to_string(),
        ));
    }
    let selected = content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line_number = index + 1;
            (line_number >= start_line && line_number <= end_line).then_some(line)
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "context read_span range did not match content".to_string(),
        ));
    }
    Ok(selected.join("\n"))
}

fn redact_context_content(content: &str) -> String {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"AKIA[0-9A-Z]{16}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            r"ghp_[A-Za-z0-9_]{20,}",
            r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        ]
        .into_iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    });
    patterns
        .iter()
        .fold(content.to_string(), |redacted, pattern| {
            pattern.replace_all(&redacted, "[REDACTED]").into_owned()
        })
}

fn evidence_by_id(evidence: &[ContextEvidence], ids: &[EvidenceId]) -> Vec<ContextEvidence> {
    evidence
        .iter()
        .filter(|candidate| ids.iter().any(|id| id == &candidate.id))
        .cloned()
        .collect()
}

fn path_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_string()
}

fn related_symbol_terms(
    evidence: &[ContextEvidence],
    path: &str,
    explicit_symbol: Option<String>,
) -> Vec<String> {
    let mut terms = std::collections::BTreeSet::new();
    if let Some(symbol) = explicit_symbol.filter(|symbol| !symbol.is_empty()) {
        terms.insert(symbol);
    }
    let stem = path_stem(path);
    if !stem.is_empty() {
        terms.insert(stem);
    }
    for candidate in evidence
        .iter()
        .filter(|candidate| candidate.kind == ContextEvidenceKind::Symbol)
        .filter(|candidate| {
            candidate
                .path
                .as_ref()
                .map(|candidate_path| candidate_path.display() == path)
                .unwrap_or(false)
        })
    {
        if let Some(summary) = &candidate.summary {
            if let Some(symbol) = summary
                .strip_prefix("symbol ")
                .and_then(|rest| rest.split_once(" in "))
                .map(|(symbol, _)| symbol)
                .filter(|symbol| !symbol.is_empty())
            {
                terms.insert(symbol.to_string());
            }
        }
    }
    terms.into_iter().collect()
}

fn related_symbol_score(
    evidence: &ContextEvidence,
    file_contents: &std::collections::BTreeMap<crate::runtime::contracts::RepoPath, String>,
    symbol_graph: &crate::context_engine::ContextSymbolGraph,
    path: &str,
    terms: &[String],
) -> Option<usize> {
    let evidence_path = evidence.path.as_ref()?;
    let evidence_path_text = evidence_path.display();
    if evidence_path_text == path {
        return Some(100);
    }
    let mut score = 0usize;
    if let Ok(query_path) = crate::runtime::contracts::RepoPath::parse(path) {
        if symbol_graph
            .related_importers(&query_path)
            .contains(evidence_path)
        {
            score = score.saturating_add(90);
        }
    }
    let summary = evidence
        .summary
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let content = file_contents
        .get(evidence_path)
        .map(|content| content.to_ascii_lowercase())
        .unwrap_or_default();
    for term in terms {
        let term = term.to_ascii_lowercase();
        if term.is_empty() {
            continue;
        }
        if summary.contains(&term) {
            score = score.saturating_add(60);
        }
        if content.contains(&term) {
            score = score.saturating_add(35);
        }
    }
    let import_hint = import_hint(path).to_ascii_lowercase();
    if !import_hint.is_empty() && content.contains(&import_hint) {
        score = score.saturating_add(45);
    }
    (score > 0).then_some(score)
}

fn import_hint(path: &str) -> String {
    path.strip_suffix(".rs")
        .or_else(|| path.strip_suffix(".ts"))
        .or_else(|| path.strip_suffix(".tsx"))
        .or_else(|| path.strip_suffix(".js"))
        .or_else(|| path.strip_suffix(".jsx"))
        .or_else(|| path.strip_suffix(".py"))
        .unwrap_or(path)
        .replace(['/', '\\'], "::")
}
