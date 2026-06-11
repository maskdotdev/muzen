use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::runtime::contracts::{stable_id, RuntimeError, RuntimeResult, SnapshotId};

use super::unix_timestamp_string;
use super::ContextLearningStore;
use super::{
    evidence_by_id, fused_search, read_line_span, string_arg, trust_rank,
    usize_arg,
};
use super::{explain_selected_evidence, purpose_name, rank_for_purpose, score_for_purpose};
use super::{learning_is_expired, redact_context_content};
use super::{path_stem, related_symbol_score, related_symbol_terms};
use super::{
    ContextBudgetUsage, ContextEngineConfig, ContextEngineMode, ContextEvidence,
    ContextEvidenceKind, ContextFeedback, ContextFeedbackReceipt, ContextIndex, ContextIndexReport,
    ContextIndexRequest, ContextIndexStore, ContextLearning, ContextLearningApproval,
    ContextLearningApprovalReceipt, ContextLearningScope, ContextLearningSource,
    ContextLearningStatus, ContextOmissionReason, ContextPack, ContextPackId, ContextPackRequest,
    ContextQuery, ContextQueryKind, ContextQueryResult, ContextRange, ContextSufficiency,
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
            relationships,
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
                let outcome = fused_search(
                    &index,
                    &query,
                    limit,
                    self.config.bm25_k1,
                    self.config.bm25_b,
                    self.config.rrf_k,
                )
                .await?;
                let data = Some(serde_json::json!({
                    "fusion": outcome.fusion,
                    "fusionOmissions": outcome.omissions,
                }));
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
                // Tests connected through the resolved reference graph rank
                // above path-stem matches.
                let graph_test_paths: std::collections::BTreeSet<_> =
                    crate::runtime::contracts::RepoPath::parse(&path)
                        .map(|query_path| {
                            index
                                .graph
                                .referencers(&query_path)
                                .filter(|edge| super::is_test_path(&edge.from.display()))
                                .map(|edge| edge.from.clone())
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
