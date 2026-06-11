use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::review_plan::ReviewPlan;
use crate::runtime::contracts::{
    stable_id, ArtifactId, EvidenceId, RepoPath, RuntimeError, RuntimeResult, SessionInstruction,
    SnapshotCaptureStatus, SnapshotId,
};
use crate::runtime::repo::{FileMeta, RepoSnapshot};

use super::chunking::{
    body_elision_map, chunk_file, diff_hunk_ranges, estimate_tokens, range_overlaps, skeleton_view,
    FileChunk, SkeletonView,
};
use super::derived::{
    derived_file_key, derived_vector_key, ContextDerivedCache, DerivedFileData,
    InMemoryContextDerivedCache,
};
use super::graph::{co_change_stats, expand_from_changes, GraphCandidate, ReferenceGraph};
use super::syntax::{parse_symbols, ParsedSymbols};
use super::{
    context_embedding_text, redact_context_content, validate_embedding_batch, ContextEngineConfig,
    ContextEvidence, ContextEvidenceKind, ContextEvidenceRepresentation, ContextEvidenceSource,
    ContextIndexId, ContextProvenance, ContextRange, ContextRankSignals, ContextRelationship,
    ContextRevision, ContextScope, ContextSemanticConfig, ContextSemanticMode, ContextSensitivity,
    ContextSymbolGraph, ContextTrust, EmbeddingInput, EmbeddingProvider, HostedEmbeddingProvider,
    InMemoryVectorIndex, LocalHashEmbeddingProvider, VectorIndex,
};

pub const CONTEXT_ENGINE_VERSION: &str = "0.1.0";
pub const CONTEXT_MANIFEST_SCHEMA_VERSION: &str = "muzen.context_manifest.v1";

#[derive(Debug, Clone)]
pub struct ContextIndexRequest {
    pub run_id: Option<String>,
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) review_plan: Option<ReviewPlan>,
    pub instructions: Vec<SessionInstruction>,
    pub host_metadata: BTreeMap<String, serde_json::Value>,
    pub cross_repo_contracts: Vec<CrossRepoContractCandidate>,
    pub allowed_cross_repo_resources: BTreeSet<String>,
    pub include_host_context: bool,
    pub semantic: ContextSemanticConfig,
    pub limits: ContextLimits,
    /// Content-hash keyed cache of per-file derived data and embedding
    /// vectors (R9). The engine injects its own (possibly durable)
    /// cache; standalone builds get a fresh in-memory one.
    pub derived_cache: Arc<dyn ContextDerivedCache>,
}

impl ContextIndexRequest {
    pub(crate) fn for_snapshot(snapshot: Arc<RepoSnapshot>, config: &ContextEngineConfig) -> Self {
        Self {
            run_id: None,
            snapshot,
            review_plan: None,
            instructions: Vec::new(),
            host_metadata: BTreeMap::new(),
            cross_repo_contracts: Vec::new(),
            allowed_cross_repo_resources: BTreeSet::new(),
            include_host_context: config.include_host_context,
            semantic: config.semantic.clone(),
            limits: ContextLimits::from_config(config),
            derived_cache: Arc::new(InMemoryContextDerivedCache::new()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CrossRepoContractCandidate {
    pub resource_id: String,
    pub repository: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextLimits {
    pub max_indexed_files: usize,
    pub max_indexed_bytes: usize,
    pub max_evidence_items: usize,
    pub max_pack_tokens: usize,
    pub max_query_results: usize,
    pub chunk_max_tokens: usize,
    pub max_chunks_per_file: usize,
    pub graph_max_hops: usize,
    pub graph_max_candidates_per_anchor: usize,
    pub co_change_commit_limit: usize,
}

impl ContextLimits {
    pub fn from_config(config: &ContextEngineConfig) -> Self {
        Self {
            max_indexed_files: config.max_indexed_files,
            max_indexed_bytes: config.max_indexed_bytes,
            max_evidence_items: config.max_evidence_items,
            max_pack_tokens: config.max_pack_tokens,
            max_query_results: config.max_query_results,
            chunk_max_tokens: config.chunk_max_tokens,
            max_chunks_per_file: config.max_chunks_per_file,
            graph_max_hops: config.graph_max_hops,
            graph_max_candidates_per_anchor: config.graph_max_candidates_per_anchor,
            co_change_commit_limit: config.co_change_commit_limit,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexReport {
    pub index_id: ContextIndexId,
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub context_engine_version: String,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub indexed_bytes: u64,
    pub indexed_changed_files: usize,
    pub rule_count: usize,
    pub diff_hunk_count: usize,
    pub evidence_count: usize,
    pub elapsed_ms: u64,
    /// Files whose derived data (chunks, skeletons, symbols) came from
    /// the content-hash cache (R9).
    pub derived_cache_hits: usize,
    /// Files whose derived data was recomputed this build.
    pub derived_cache_misses: usize,
    /// Embedding vectors computed by a provider this build.
    pub embeddings_computed: usize,
    /// Embedding vectors served from the content-hash cache.
    pub embeddings_cached: usize,
    pub warnings: Vec<ContextIndexWarning>,
    pub artifacts: Vec<ArtifactId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexWarning {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RepoPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextManifestArtifact {
    pub schema_version: String,
    pub context_engine_version: String,
    pub index_id: ContextIndexId,
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub path_policy_hash: String,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub indexed_bytes: u64,
    pub evidence_count: usize,
    pub rule_count: usize,
    pub diff_hunk_count: usize,
    pub skips: Vec<ContextIndexSkip>,
    pub warnings: Vec<ContextIndexWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexSkip {
    pub path: RepoPath,
    pub reason: ContextIndexSkipReason,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextIndexSkipReason {
    BinaryFile,
    Unavailable,
    BudgetExceeded,
    /// Some of the file's chunks were dropped to respect chunk or
    /// evidence-count budgets.
    ChunkBudgetExceeded,
}

#[derive(Debug, Clone)]
pub struct ContextIndex {
    pub index_id: ContextIndexId,
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub evidence: Vec<ContextEvidence>,
    pub file_contents: BTreeMap<RepoPath, String>,
    pub lexical: super::LexicalIndex,
    pub symbol_graph: ContextSymbolGraph,
    pub graph: ReferenceGraph,
    pub graph_candidates: Vec<GraphCandidate>,
    pub relationships: Vec<ContextRelationship>,
    /// Diff hunk ranges by changed file path (new-side line spans).
    pub hunk_ranges: BTreeMap<String, Vec<ContextRange>>,
    /// Signatures-only skeleton twin per chunk evidence id (R7). Not
    /// part of `evidence` (skeletons are not retrieval candidates); the
    /// pack compiler downgrades a candidate to its twin when the full
    /// content exceeds the remaining budget.
    pub skeletons: BTreeMap<String, ContextEvidence>,
    pub semantic: ContextSemanticConfig,
    pub semantic_vectors: Option<InMemoryVectorIndex>,
    pub denied_cross_repo_contracts: usize,
    pub skips: Vec<ContextIndexSkip>,
    pub report: ContextIndexReport,
    pub manifest_artifact: ContextManifestArtifact,
}

impl ContextIndex {
    pub async fn build(request: ContextIndexRequest) -> RuntimeResult<Self> {
        let started = std::time::Instant::now();
        let _review_plan = &request.review_plan;
        let snapshot = Arc::clone(&request.snapshot);
        let derived_cache = Arc::clone(&request.derived_cache);
        let mut derived_cache_hits = 0usize;
        let mut derived_cache_misses = 0usize;
        let mut evidence = Vec::new();
        let mut skeletons: BTreeMap<String, ContextEvidence> = BTreeMap::new();
        let mut body_terms_by_id: BTreeMap<String, BTreeMap<String, f32>> = BTreeMap::new();
        let mut file_contents = BTreeMap::new();
        let mut symbol_graph = ContextSymbolGraph::default();
        let mut parsed_by_file: BTreeMap<RepoPath, ParsedSymbols> = BTreeMap::new();
        let mut denied_cross_repo_contracts = 0usize;
        let mut skips = Vec::new();
        let mut indexed_files = 0usize;
        let mut indexed_bytes = 0u64;
        let mut indexed_changed_files = 0usize;
        let mut rule_count = 0usize;
        let diff_hunk_count = count_diff_hunks(&snapshot.diff.content);

        let hunk_ranges = diff_hunk_ranges(&snapshot.diff.content);
        for file in &snapshot.manifest.files {
            if indexed_files >= request.limits.max_indexed_files
                || indexed_bytes as usize >= request.limits.max_indexed_bytes
                || evidence.len() >= request.limits.max_evidence_items
            {
                skips.push(ContextIndexSkip {
                    path: file.rel_path.clone(),
                    reason: ContextIndexSkipReason::BudgetExceeded,
                });
                continue;
            }
            match file.capture_status {
                SnapshotCaptureStatus::Captured => {
                    indexed_files += 1;
                    indexed_bytes = indexed_bytes.saturating_add(file.size);
                    if file.is_changed {
                        indexed_changed_files += 1;
                    }
                    let kind = evidence_kind_for_file(file);
                    if kind == ContextEvidenceKind::RepositoryRule {
                        rule_count += 1;
                    }
                    let file_hunks = hunk_ranges
                        .get(&file.rel_path.display())
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    let content = snapshot
                        .read_bounded(file.file_id, request.limits.max_indexed_bytes)
                        .ok()
                        .and_then(|(bytes, _truncated)| String::from_utf8(bytes).ok());
                    match content {
                        Some(content) => {
                            let chunked = chunked_kind(kind, &content);
                            let derived = derived_for_file(
                                derived_cache.as_ref(),
                                file,
                                &content,
                                chunked,
                                request.limits.chunk_max_tokens,
                                &mut derived_cache_hits,
                                &mut derived_cache_misses,
                            );
                            if chunked {
                                let mut emitted = 0usize;
                                let mut truncated = false;
                                for ((chunk, view), terms) in derived
                                    .chunks
                                    .iter()
                                    .zip(derived.skeletons.iter())
                                    .zip(derived.chunk_terms.into_iter())
                                {
                                    if emitted >= request.limits.max_chunks_per_file
                                        || evidence.len() >= request.limits.max_evidence_items
                                    {
                                        truncated = true;
                                        break;
                                    }
                                    let item =
                                        chunk_evidence(&snapshot, file, kind, chunk, file_hunks);
                                    if let Some(view) = view {
                                        skeletons.insert(
                                            item.id.0.clone(),
                                            skeleton_evidence(&snapshot, file, &item, view),
                                        );
                                    }
                                    body_terms_by_id.insert(item.id.0.clone(), terms);
                                    evidence.push(item);
                                    emitted += 1;
                                }
                                if truncated {
                                    skips.push(ContextIndexSkip {
                                        path: file.rel_path.clone(),
                                        reason: ContextIndexSkipReason::ChunkBudgetExceeded,
                                    });
                                }
                            } else {
                                let item = file_evidence(&snapshot, file, kind, Some(&content));
                                body_terms_by_id.insert(item.id.0.clone(), derived.file_terms);
                                evidence.push(item);
                            }
                            index_symbols(
                                &snapshot,
                                file,
                                &derived.parsed,
                                file_hunks,
                                &mut symbol_graph,
                                &mut evidence,
                                request.limits.max_evidence_items,
                            );
                            parsed_by_file.insert(file.rel_path.clone(), derived.parsed);
                            file_contents.insert(file.rel_path.clone(), content);
                        }
                        None => evidence.push(file_evidence(&snapshot, file, kind, None)),
                    }
                }
                SnapshotCaptureStatus::NotTextCandidate => skips.push(ContextIndexSkip {
                    path: file.rel_path.clone(),
                    reason: ContextIndexSkipReason::BinaryFile,
                }),
                SnapshotCaptureStatus::SkippedMemoryLimit
                | SnapshotCaptureStatus::SkippedUnreadable => skips.push(ContextIndexSkip {
                    path: file.rel_path.clone(),
                    reason: ContextIndexSkipReason::Unavailable,
                }),
            }
        }

        if !snapshot.diff.content.is_empty() && evidence.len() < request.limits.max_evidence_items {
            evidence.push(ContextEvidence {
                id: EvidenceId(stable_id(&[
                    &snapshot.snapshot_id.0,
                    "diff",
                    &snapshot.diff.content_hash,
                ])),
                kind: ContextEvidenceKind::Diff,
                source: ContextEvidenceSource::Snapshot,
                trust: ContextTrust::Kernel,
                sensitivity: ContextSensitivity::Private,
                scope: ContextScope::Snapshot,
                path: None,
                revision: None,
                range: None,
                content_hash: Some(snapshot.diff.content_hash.clone()),
                summary: Some("Review diff manifest".to_string()),
                is_changed_span: true,
                representation: ContextEvidenceRepresentation::FullContent,
                skeleton_text: None,
                signals: ContextRankSignals::default(),
                token_estimate: estimate_tokens(snapshot.diff.content.len()),
                provenance: ContextProvenance {
                    provider: "snapshot_diff".to_string(),
                    query: None,
                    tool_call_id: None,
                    snapshot_id: Some(snapshot.snapshot_id.0.clone()),
                    original_url: None,
                },
                created_at_utc: None,
                expires_at_utc: None,
            });
        }

        if request.include_host_context && evidence.len() < request.limits.max_evidence_items {
            for instruction in &request.instructions {
                if evidence.len() >= request.limits.max_evidence_items {
                    break;
                }
                if let Some(item) =
                    host_instruction_evidence(&snapshot, instruction, request.run_id.as_deref())
                {
                    evidence.push(item);
                }
            }
            for (key, value) in &request.host_metadata {
                if evidence.len() >= request.limits.max_evidence_items {
                    break;
                }
                if let Some(item) =
                    host_metadata_evidence(&snapshot, key, value, request.run_id.as_deref())
                {
                    evidence.push(item);
                }
            }
        }
        for candidate in &request.cross_repo_contracts {
            if evidence.len() >= request.limits.max_evidence_items {
                break;
            }
            if request
                .allowed_cross_repo_resources
                .contains(&candidate.resource_id)
            {
                evidence.push(cross_repo_contract_evidence(
                    &snapshot,
                    candidate,
                    request.run_id.as_deref(),
                ));
            } else {
                denied_cross_repo_contracts = denied_cross_repo_contracts.saturating_add(1);
            }
        }

        // Change-rooted graph: the diff anchors expansion into the blast
        // radius of the change (callers, callees, tests, co-changes).
        let changed_paths: BTreeSet<RepoPath> = snapshot
            .manifest
            .files
            .iter()
            .filter(|file| file.is_changed)
            .map(|file| file.rel_path.clone())
            .collect();
        let mut graph = ReferenceGraph::build(&parsed_by_file);
        graph.co_change = co_change_stats(
            &snapshot.source_root,
            &changed_paths,
            request.limits.co_change_commit_limit,
        );
        let (graph_candidates, graph_overflow) = expand_from_changes(
            &graph,
            &changed_paths,
            request.limits.graph_max_hops,
            request.limits.graph_max_candidates_per_anchor,
        );
        let relationships = build_relationships(&evidence, &graph_candidates);
        apply_rank_signals(&mut evidence, &graph, &graph_candidates, &changed_paths);
        // A skeleton twin carries the same structural signals as the
        // full chunk it stands in for, so explanations stay truthful.
        for item in &evidence {
            if let Some(skeleton) = skeletons.get_mut(&item.id.0) {
                skeleton.signals = item.signals;
            }
        }

        let lexical = super::LexicalIndex::build(&evidence, &file_contents, &body_terms_by_id);
        let (semantic_vectors, embeddings_computed, embeddings_cached) = build_semantic_vectors(
            &request.semantic,
            &evidence,
            &file_contents,
            derived_cache.as_ref(),
        )
        .await?;

        let index_id = ContextIndexId(stable_id(&[
            &snapshot.snapshot_id.0,
            &snapshot.manifest_hash,
            CONTEXT_ENGINE_VERSION,
            &evidence.len().to_string(),
        ]));
        let mut warnings = Vec::new();
        if graph_overflow > 0 {
            warnings.push(ContextIndexWarning {
                code: "graph_candidates_truncated".to_string(),
                message: format!(
                    "graph expansion dropped {graph_overflow} candidates over per-anchor budget"
                ),
                path: None,
            });
        }
        if derived_cache.recovered_from_corruption() {
            warnings.push(ContextIndexWarning {
                code: "derived_cache_recovered".to_string(),
                message: "derived-data cache was unreadable; rebuilt all derived data from scratch"
                    .to_string(),
                path: None,
            });
        }
        if let Err(error) = derived_cache.flush() {
            warnings.push(ContextIndexWarning {
                code: "derived_cache_flush_failed".to_string(),
                message: format!("derived-data cache was not persisted: {error}"),
                path: None,
            });
        }
        let report = ContextIndexReport {
            index_id: index_id.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            context_engine_version: CONTEXT_ENGINE_VERSION.to_string(),
            indexed_files,
            skipped_files: skips.len(),
            indexed_bytes,
            indexed_changed_files,
            rule_count,
            diff_hunk_count,
            evidence_count: evidence.len(),
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            derived_cache_hits,
            derived_cache_misses,
            embeddings_computed,
            embeddings_cached,
            warnings: warnings.clone(),
            artifacts: Vec::new(),
        };
        let manifest_artifact = ContextManifestArtifact {
            schema_version: CONTEXT_MANIFEST_SCHEMA_VERSION.to_string(),
            context_engine_version: CONTEXT_ENGINE_VERSION.to_string(),
            index_id: index_id.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            path_policy_hash: snapshot.path_policy_hash.clone(),
            indexed_files,
            skipped_files: skips.len(),
            indexed_bytes,
            evidence_count: evidence.len(),
            rule_count,
            diff_hunk_count,
            skips: skips.clone(),
            warnings,
        };
        Ok(Self {
            index_id,
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            evidence,
            file_contents,
            lexical,
            symbol_graph,
            graph,
            graph_candidates,
            relationships,
            hunk_ranges,
            skeletons,
            semantic: request.semantic,
            semantic_vectors,
            denied_cross_repo_contracts,
            skips,
            report,
            manifest_artifact,
        })
    }
}

/// Fetch one file's derived data from the cache or recompute it. The
/// derivation is a pure function of the key inputs (path, content hash,
/// chunk budget) under the current derivation version, so cached and
/// recomputed builds are indistinguishable.
fn derived_for_file(
    cache: &dyn ContextDerivedCache,
    file: &FileMeta,
    content: &str,
    chunked: bool,
    chunk_max_tokens: usize,
    hits: &mut usize,
    misses: &mut usize,
) -> DerivedFileData {
    let path = file.rel_path.display();
    let content_hash = file
        .content_hash
        .clone()
        .unwrap_or_else(|| stable_id(&[content]));
    let key = derived_file_key(&path, &content_hash, chunk_max_tokens);
    if let Some(data) = cache.get_file(&key) {
        *hits += 1;
        return data;
    }
    *misses += 1;
    let data = compute_derived_file_data(&path, content, chunked, chunk_max_tokens);
    cache.put_file(&key, data.clone());
    data
}

fn compute_derived_file_data(
    path: &str,
    content: &str,
    chunked: bool,
    chunk_max_tokens: usize,
) -> DerivedFileData {
    use super::chunking::slice_evidence_lines;
    use super::lexical::body_term_counts;
    let (chunks, skeletons, chunk_terms, file_terms) = if chunked {
        let chunks = chunk_file(path, content, chunk_max_tokens);
        let elision = body_elision_map(path, content);
        let lines = content.lines().collect::<Vec<_>>();
        let skeletons = chunks
            .iter()
            .map(|chunk| {
                elision
                    .as_deref()
                    .and_then(|elided| skeleton_view(&lines, chunk.range(), elided))
            })
            .collect();
        // Token over the same slice the lexical index would take, so
        // cached postings contributions are byte-identical to fresh ones.
        let chunk_terms = chunks
            .iter()
            .map(|chunk| body_term_counts(&slice_evidence_lines(content, Some(&chunk.range()))))
            .collect();
        (chunks, skeletons, chunk_terms, BTreeMap::new())
    } else {
        let file_terms = body_term_counts(&slice_evidence_lines(content, None));
        (Vec::new(), Vec::new(), Vec::new(), file_terms)
    };
    DerivedFileData {
        chunks,
        skeletons,
        chunk_terms,
        file_terms,
        parsed: parse_symbols(path, content),
    }
}

async fn build_semantic_vectors(
    semantic: &ContextSemanticConfig,
    evidence: &[ContextEvidence],
    file_contents: &BTreeMap<RepoPath, String>,
    cache: &dyn ContextDerivedCache,
) -> RuntimeResult<(Option<InMemoryVectorIndex>, usize, usize)> {
    if semantic.mode == ContextSemanticMode::NoVector {
        return Ok((None, 0, 0));
    }
    let max_inputs = semantic.max_embedding_inputs.min(evidence.len());
    let inputs = evidence
        .iter()
        .take(max_inputs)
        .map(|evidence| {
            let content = evidence
                .path
                .as_ref()
                .and_then(|path| file_contents.get(path))
                .map(|content| {
                    super::chunking::slice_evidence_lines(content, evidence.range.as_ref())
                });
            EmbeddingInput {
                id: evidence.id.0.clone(),
                text: context_embedding_text(evidence, content.as_deref()),
                sensitivity: evidence.sensitivity,
            }
        })
        .collect::<Vec<_>>();
    validate_embedding_batch(
        &ContextEngineConfig {
            semantic: semantic.clone(),
            ..ContextEngineConfig::snapshot_v0()
        },
        &inputs,
    )?;
    // Vectors cache per (provider identity, embedded text): only inputs
    // absent from the cache reach a provider, so warm re-index spends
    // nothing on embeddings.
    let provider_tag = match semantic.mode {
        ContextSemanticMode::NoVector => unreachable!("handled above"),
        ContextSemanticMode::Local => "local_hash_256".to_string(),
        ContextSemanticMode::Hosted => format!(
            "hosted:{}",
            semantic.hosted_model.as_deref().unwrap_or("default")
        ),
    };
    let keys = inputs
        .iter()
        .map(|input| derived_vector_key(&provider_tag, &input.text))
        .collect::<Vec<_>>();
    let mut vectors = keys
        .iter()
        .map(|key| cache.get_vector(key))
        .collect::<Vec<_>>();
    let missing = vectors
        .iter()
        .enumerate()
        .filter(|(_, vector)| vector.is_none())
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    let embeddings_cached = inputs.len() - missing.len();
    let embeddings_computed = missing.len();
    if !missing.is_empty() {
        let batch = missing
            .iter()
            .map(|&position| inputs[position].clone())
            .collect::<Vec<_>>();
        let computed = match semantic.mode {
            ContextSemanticMode::NoVector => Vec::new(),
            ContextSemanticMode::Local => {
                let provider = LocalHashEmbeddingProvider::new(256)?;
                provider.embed(batch).await?
            }
            ContextSemanticMode::Hosted => {
                let provider = HostedEmbeddingProvider::from_config(semantic)?;
                provider.embed(batch).await?
            }
        };
        if computed.len() != missing.len() {
            return Err(RuntimeError::ProviderMessage {
                status: None,
                retryable: false,
                message: "context embedding provider returned an unexpected vector count"
                    .to_string(),
            });
        }
        for (&position, vector) in missing.iter().zip(computed) {
            cache.put_vector(&keys[position], vector.values.clone());
            vectors[position] = Some(vector.values);
        }
    }
    let mut index = InMemoryVectorIndex::new();
    for (input, values) in inputs.into_iter().zip(vectors) {
        // Cached values were normalized by the provider before storage.
        let vector = super::EmbeddingVector {
            values: values.expect("every vector is cached or computed"),
        };
        index.put(input.id, vector)?;
    }
    Ok((Some(index), embeddings_computed, embeddings_cached))
}

fn cross_repo_contract_evidence(
    snapshot: &RepoSnapshot,
    candidate: &CrossRepoContractCandidate,
    run_id: Option<&str>,
) -> ContextEvidence {
    let text = format!("{}: {}", candidate.repository, candidate.summary);
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "cross_repo_contract",
            &candidate.resource_id,
            &text,
            run_id.unwrap_or("standalone"),
        ])),
        kind: ContextEvidenceKind::CrossRepoContract,
        source: ContextEvidenceSource::External,
        trust: ContextTrust::ToolProvider,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::External,
        path: None,
        revision: None,
        range: None,
        content_hash: Some(stable_id(&[&text])),
        summary: Some(format!(
            "cross-repo contract {}: {}",
            candidate.repository,
            concise_text(&candidate.summary)
        )),
        is_changed_span: false,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
        token_estimate: estimate_tokens(text.len()),
        provenance: ContextProvenance {
            provider: "cross_repo_provider".to_string(),
            query: Some(candidate.resource_id.clone()),
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: candidate.original_url.clone(),
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

fn host_instruction_evidence(
    snapshot: &RepoSnapshot,
    instruction: &SessionInstruction,
    run_id: Option<&str>,
) -> Option<ContextEvidence> {
    let text = instruction.text.trim();
    if text.is_empty() {
        return None;
    }
    let kind = host_instruction_kind(&instruction.kind);
    Some(ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "host_instruction",
            &instruction.kind,
            text,
            run_id.unwrap_or("standalone"),
        ])),
        kind,
        source: ContextEvidenceSource::Host,
        trust: if instruction.trusted {
            ContextTrust::HostTrusted
        } else {
            ContextTrust::UserUntrusted
        },
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Run,
        path: None,
        revision: None,
        range: None,
        content_hash: Some(stable_id(&[text])),
        summary: Some(format!("host {}: {}", instruction.kind, concise_text(text))),
        is_changed_span: false,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
        token_estimate: estimate_tokens(text.len()),
        provenance: ContextProvenance {
            provider: "host_instruction".to_string(),
            query: Some(instruction.kind.clone()),
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    })
}

fn host_metadata_evidence(
    snapshot: &RepoSnapshot,
    key: &str,
    value: &serde_json::Value,
    run_id: Option<&str>,
) -> Option<ContextEvidence> {
    let text = match value {
        serde_json::Value::String(value) => value.trim().to_string(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => value.to_string(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => value.to_string(),
        serde_json::Value::Null => String::new(),
    };
    if text.is_empty() {
        return None;
    }
    Some(ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "host_metadata",
            key,
            &text,
            run_id.unwrap_or("standalone"),
        ])),
        kind: host_metadata_kind(key),
        source: ContextEvidenceSource::Host,
        trust: ContextTrust::HostTrusted,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Run,
        path: None,
        revision: None,
        range: None,
        content_hash: Some(stable_id(&[&text])),
        summary: Some(format!("host metadata {key}: {}", concise_text(&text))),
        is_changed_span: false,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
        token_estimate: estimate_tokens(text.len()),
        provenance: ContextProvenance {
            provider: "host_metadata".to_string(),
            query: Some(key.to_string()),
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    })
}

fn host_instruction_kind(kind: &str) -> ContextEvidenceKind {
    let lower = kind.to_ascii_lowercase();
    if lower.contains("ticket")
        || lower.contains("issue")
        || lower.contains("acceptance")
        || lower.contains("requirement")
        || lower.contains("scope")
    {
        ContextEvidenceKind::Ticket
    } else {
        ContextEvidenceKind::OrganizationRule
    }
}

fn host_metadata_kind(key: &str) -> ContextEvidenceKind {
    let lower = key.to_ascii_lowercase();
    if lower.contains("cross_repo")
        || lower.contains("crossrepo")
        || lower.contains("linked_repo")
        || lower.contains("linkedrepo")
        || lower.contains("contract")
        || lower.contains("consumer")
    {
        ContextEvidenceKind::CrossRepoContract
    } else if lower.contains("ticket")
        || lower.contains("issue")
        || lower.contains("acceptance")
        || lower.contains("requirement")
        || lower.contains("label")
        || lower.contains("release")
    {
        ContextEvidenceKind::Ticket
    } else {
        ContextEvidenceKind::OrganizationRule
    }
}

fn concise_text(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut output = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if output.len() > MAX_CHARS {
        output.truncate(MAX_CHARS);
        output.push_str("...");
    }
    output
}

/// Kinds whose retrieval unit is the AST chunk. Repository guidance is
/// always a whole-file unit; configs and docs stay whole-file while small.
fn chunked_kind(kind: ContextEvidenceKind, content: &str) -> bool {
    const WHOLE_FILE_MAX_TOKENS: usize = 1_600;
    match kind {
        ContextEvidenceKind::FileSpan | ContextEvidenceKind::Test => true,
        ContextEvidenceKind::Config | ContextEvidenceKind::Doc => {
            estimate_tokens(content.len()) > WHOLE_FILE_MAX_TOKENS
        }
        _ => false,
    }
}

/// Fill structural ranking signals (R5) on every evidence item once the
/// reference graph, expansion candidates, and co-change stats exist.
fn apply_rank_signals(
    evidence: &mut [ContextEvidence],
    graph: &ReferenceGraph,
    candidates: &[GraphCandidate],
    changed: &BTreeSet<RepoPath>,
) {
    let mut min_hop: BTreeMap<&RepoPath, u8> = BTreeMap::new();
    for candidate in candidates {
        let entry = min_hop.entry(&candidate.path).or_insert(candidate.hop);
        *entry = (*entry).min(candidate.hop);
    }
    for item in evidence {
        let Some(path) = item.path.clone() else {
            // Pathless evidence (diff manifest, host context): the diff
            // itself is the change anchor.
            item.signals.graph_distance = item.is_changed_span.then_some(0);
            continue;
        };
        item.signals.graph_distance = if item.is_changed_span {
            Some(0)
        } else if changed.contains(&path) {
            // Unchanged span in a changed file: adjacent to the change.
            Some(1)
        } else {
            min_hop.get(&path).copied()
        };
        item.signals.co_change_score = graph
            .co_change
            .get(&path)
            .map(|stat| stat.weight)
            .unwrap_or(0.0);
        item.signals.path_proximity = path_proximity(&path, changed);
    }
}

/// Directory proximity to the nearest changed file: shared directory
/// prefix depth over the deeper of the two paths, in [0, 1].
fn path_proximity(path: &RepoPath, changed: &BTreeSet<RepoPath>) -> f32 {
    changed
        .iter()
        .map(|other| shared_dir_ratio(&path.display(), &other.display()))
        .fold(0.0, f32::max)
}

fn shared_dir_ratio(left: &str, right: &str) -> f32 {
    let dirs = |text: &str| -> Vec<String> {
        match text.rsplit_once('/') {
            Some((dir, _file)) => dir.split('/').map(str::to_string).collect(),
            None => Vec::new(),
        }
    };
    let left_dirs = dirs(left);
    let right_dirs = dirs(right);
    let deepest = left_dirs.len().max(right_dirs.len());
    if deepest == 0 {
        // Both files sit in the repository root.
        return 1.0;
    }
    let shared = left_dirs
        .iter()
        .zip(right_dirs.iter())
        .take_while(|(a, b)| a == b)
        .count();
    shared as f32 / deepest as f32
}

/// Map graph expansion candidates and changed chunks to typed,
/// evidence-id-level relationships for packs and explanations.
fn build_relationships(
    evidence: &[ContextEvidence],
    candidates: &[GraphCandidate],
) -> Vec<ContextRelationship> {
    use super::ContextRelationshipKind;
    let mut by_path: BTreeMap<&RepoPath, &ContextEvidence> = BTreeMap::new();
    let mut changed_by_path: BTreeMap<&RepoPath, &ContextEvidence> = BTreeMap::new();
    for item in evidence {
        if let Some(path) = &item.path {
            by_path.entry(path).or_insert(item);
            if item.is_changed_span {
                changed_by_path.entry(path).or_insert(item);
            }
        }
    }
    let mut relationships = Vec::new();
    if let Some(diff) = evidence
        .iter()
        .find(|item| item.kind == ContextEvidenceKind::Diff)
    {
        for item in evidence {
            if item.is_changed_span && item.path.is_some() && item.range.is_some() {
                relationships.push(ContextRelationship {
                    from: item.id.clone(),
                    to: diff.id.clone(),
                    kind: ContextRelationshipKind::EnclosesHunk,
                    confidence: 1.0,
                    reason: "definition-aligned chunk encloses changed lines".to_string(),
                });
            }
        }
    }
    for candidate in candidates {
        let Some(from) = changed_by_path
            .get(&candidate.anchor)
            .or_else(|| by_path.get(&candidate.anchor))
        else {
            continue;
        };
        let Some(to) = by_path.get(&candidate.path) else {
            continue;
        };
        relationships.push(ContextRelationship {
            from: from.id.clone(),
            to: to.id.clone(),
            kind: candidate.kind,
            confidence: candidate.confidence,
            reason: candidate.reason.clone(),
        });
    }
    relationships
}

#[allow(clippy::too_many_arguments)]
fn index_symbols(
    snapshot: &RepoSnapshot,
    file: &FileMeta,
    parsed_symbols: &ParsedSymbols,
    file_hunks: &[ContextRange],
    symbol_graph: &mut ContextSymbolGraph,
    evidence: &mut Vec<ContextEvidence>,
    max_evidence_items: usize,
) {
    symbol_graph.add_parsed(file.rel_path.clone(), parsed_symbols);
    if !file.is_changed {
        return;
    }
    for symbol in &parsed_symbols.definitions {
        if evidence.len() >= max_evidence_items {
            break;
        }
        let range = parsed_symbols.definition_ranges.get(symbol).copied();
        let is_changed_span = range
            .map(|range| file_hunks.iter().any(|hunk| range_overlaps(&range, hunk)))
            .unwrap_or(file.is_changed);
        evidence.push(symbol_evidence(
            snapshot,
            file,
            symbol,
            range,
            is_changed_span,
        ));
    }
}

fn chunk_evidence(
    snapshot: &RepoSnapshot,
    file: &FileMeta,
    kind: ContextEvidenceKind,
    chunk: &FileChunk,
    file_hunks: &[ContextRange],
) -> ContextEvidence {
    let content_hash = stable_id(&[&chunk.text]);
    let range = chunk.range();
    let is_changed_span = file_hunks.iter().any(|hunk| range_overlaps(&range, hunk));
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "chunk",
            &file.rel_path.display(),
            &chunk.start_line.to_string(),
            &content_hash,
        ])),
        kind,
        source: ContextEvidenceSource::Snapshot,
        trust: ContextTrust::Kernel,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Snapshot,
        path: Some(file.rel_path.clone()),
        revision: Some(ContextRevision::head()),
        range: Some(range),
        content_hash: Some(content_hash),
        summary: Some(chunk_summary(file, kind, chunk)),
        is_changed_span,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
        token_estimate: chunk.token_estimate(),
        provenance: ContextProvenance {
            provider: "snapshot_chunk_v1".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

/// Skeleton twin (R7): a signatures-only stand-in for one chunk whose
/// full content exceeds the remaining pack budget. The text ships on
/// the evidence (skeleton views exist nowhere on disk) and passes the
/// same redaction as full content. Path, range, kind, and changed-span
/// status mirror the full chunk; the token estimate describes the view.
fn skeleton_evidence(
    snapshot: &RepoSnapshot,
    file: &FileMeta,
    chunk: &ContextEvidence,
    view: &SkeletonView,
) -> ContextEvidence {
    let text = redact_context_content(&view.text);
    let content_hash = stable_id(&[&text]);
    let range = chunk.range.expect("chunk evidence carries a range");
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "skeleton",
            &file.rel_path.display(),
            &range.start_line.to_string(),
            &content_hash,
        ])),
        kind: chunk.kind,
        source: ContextEvidenceSource::Snapshot,
        trust: ContextTrust::Kernel,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Snapshot,
        path: Some(file.rel_path.clone()),
        revision: Some(ContextRevision::head()),
        range: Some(range),
        content_hash: Some(content_hash),
        summary: Some(format!(
            "skeleton of {} (lines {}-{}; signatures only, bodies elided)",
            file.rel_path.display(),
            range.start_line,
            range.end_line
        )),
        is_changed_span: chunk.is_changed_span,
        representation: ContextEvidenceRepresentation::Skeleton,
        skeleton_text: Some(text.clone()),
        signals: ContextRankSignals::default(),
        token_estimate: estimate_tokens(text.len()),
        provenance: ContextProvenance {
            provider: "snapshot_skeleton_v1".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

fn chunk_summary(file: &FileMeta, kind: ContextEvidenceKind, chunk: &FileChunk) -> String {
    let mut summary = match &chunk.symbol_path {
        Some(symbol_path) => format!(
            "{symbol_path} in {} (lines {}-{})",
            file.rel_path.display(),
            chunk.start_line,
            chunk.end_line
        ),
        None => format!(
            "{kind:?} chunk of {} (lines {}-{})",
            file.rel_path.display(),
            chunk.start_line,
            chunk.end_line
        ),
    };
    if let Some(doc_line) = chunk.doc_line() {
        summary.push_str(": ");
        summary.push_str(doc_line);
    }
    summary
}

fn symbol_evidence(
    snapshot: &RepoSnapshot,
    file: &FileMeta,
    symbol: &str,
    range: Option<super::ContextRange>,
    is_changed_span: bool,
) -> ContextEvidence {
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "symbol",
            &file.rel_path.display(),
            symbol,
        ])),
        kind: ContextEvidenceKind::Symbol,
        source: ContextEvidenceSource::Snapshot,
        trust: ContextTrust::Kernel,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Snapshot,
        path: Some(file.rel_path.clone()),
        revision: Some(ContextRevision::head()),
        range,
        content_hash: file.content_hash.clone(),
        summary: Some(format!("symbol {symbol} in {}", file.rel_path.display())),
        is_changed_span,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
        token_estimate: estimate_tokens(symbol.len()),
        provenance: ContextProvenance {
            provider: "snapshot_symbol_graph_v1".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

fn file_evidence(
    snapshot: &RepoSnapshot,
    file: &FileMeta,
    kind: ContextEvidenceKind,
    content: Option<&str>,
) -> ContextEvidence {
    let summary = file_summary(file, kind);
    // Honest token accounting (R7): budget math uses the indexed content
    // length. When content is unavailable (unreadable as UTF-8), only
    // the summary can ever enter a pack.
    let token_estimate = match content {
        Some(content) => estimate_tokens(content.len()),
        None => estimate_tokens(summary.len()),
    };
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "file",
            &file.rel_path.display(),
            file.content_hash.as_deref().unwrap_or(""),
        ])),
        kind,
        source: ContextEvidenceSource::Snapshot,
        trust: if kind == ContextEvidenceKind::RepositoryRule {
            ContextTrust::RepositoryUntrusted
        } else {
            ContextTrust::Kernel
        },
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Snapshot,
        path: Some(file.rel_path.clone()),
        revision: Some(ContextRevision::head()),
        range: None,
        content_hash: file.content_hash.clone(),
        summary: Some(summary),
        is_changed_span: file.is_changed,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
        token_estimate,
        provenance: ContextProvenance {
            provider: "repo_snapshot".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

fn evidence_kind_for_file(file: &FileMeta) -> ContextEvidenceKind {
    let path = file.rel_path.display();
    let lower = path.to_ascii_lowercase();
    if is_repository_guidance(&path) {
        ContextEvidenceKind::RepositoryRule
    } else if lower.contains("test") || lower.contains("spec") {
        ContextEvidenceKind::Test
    } else if is_config_path(&lower) {
        ContextEvidenceKind::Config
    } else if lower.ends_with(".md") || lower.ends_with(".mdx") || lower.ends_with(".rst") {
        ContextEvidenceKind::Doc
    } else {
        ContextEvidenceKind::FileSpan
    }
}

fn file_summary(file: &FileMeta, kind: ContextEvidenceKind) -> String {
    format!("{kind:?} file {}", file.rel_path.display())
}

fn is_repository_guidance(path: &str) -> bool {
    path == "CONTEXT.md"
        || path == "AGENTS.md"
        || path.ends_with("/AGENTS.md")
        || path == ".cursorrules"
        || path == ".github/copilot-instructions.md"
}

fn is_config_path(lower: &str) -> bool {
    lower.ends_with("cargo.toml")
        || lower.ends_with("package.json")
        || lower.ends_with("tsconfig.json")
        || lower.ends_with("pyproject.toml")
        || lower.ends_with(".yml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".json")
        || lower.ends_with(".toml")
}

fn count_diff_hunks(diff: &str) -> usize {
    diff.lines().filter(|line| line.starts_with("@@")).count()
}
