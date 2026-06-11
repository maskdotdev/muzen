use serde_json::Value;

use crate::runtime::contracts::{EvidenceId, RuntimeError, RuntimeResult};

use super::{
    ContextEvidence, ContextSemanticConfig, ContextSemanticMode, ContextSensitivity, ContextTrust,
    EmbeddingInput, EmbeddingProvider, HostedEmbeddingProvider, LocalHashEmbeddingProvider,
    VectorIndex,
};

pub(crate) fn trust_rank(trust: ContextTrust) -> u8 {
    match trust {
        ContextTrust::Kernel => 6,
        ContextTrust::HostTrusted => 5,
        ContextTrust::OrganizationTrusted => 4,
        ContextTrust::ToolProvider => 3,
        ContextTrust::RepositoryUntrusted => 2,
        ContextTrust::UserUntrusted => 1,
        ContextTrust::ExternalUntrusted => 0,
    }
}

pub(crate) fn string_arg(arguments: &Value, key: &str) -> RuntimeResult<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("context query requires {key}")))
}

pub(crate) fn usize_arg(arguments: &Value, key: &str) -> RuntimeResult<usize> {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| RuntimeError::InvalidInput(format!("context query requires {key}")))
}

/// One fused search hit with the rank it held in each source list.
/// Ranks are 1-based; `None` means the source did not return the item.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FusionTrace {
    pub evidence_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lexical_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_rank: Option<usize>,
    pub score: f32,
    /// 1-based rank assigned by the rerank stage; `None` when reranking
    /// is off or the item fell outside the reranked window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
}

/// Evidence excluded by the post-fusion sensitivity filter, with the reason
/// recorded instead of a silent drop.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FusionOmission {
    pub evidence_id: String,
    pub reason: &'static str,
}

/// A retrieval stage that failed and was skipped instead of failing the
/// query: results degrade (lexical-only, or fused order without rerank)
/// and the failure is recorded, never silent.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FusionDegradation {
    pub stage: &'static str,
    pub message: String,
}

pub(crate) struct FusedSearch {
    pub evidence: Vec<ContextEvidence>,
    pub fusion: Vec<FusionTrace>,
    pub omissions: Vec<FusionOmission>,
    pub degraded: Vec<FusionDegradation>,
}

/// Reciprocal Rank Fusion over the lexical and semantic candidate lists:
/// `score(d) = sum over lists of 1 / (rrf_k + rank_in_list(d))`.
///
/// Single-list fusion preserves the source order exactly (1/(k+rank) is
/// strictly decreasing), so no-vector mode is a pure BM25 passthrough.
/// Ties break on evidence id for determinism.
pub(crate) fn rrf_fuse(lexical: &[String], semantic: &[String], rrf_k: f32) -> Vec<FusionTrace> {
    fn upsert(traces: &mut Vec<FusionTrace>, id: &String) -> usize {
        if let Some(position) = traces.iter().position(|trace| &trace.evidence_id == id) {
            position
        } else {
            traces.push(FusionTrace {
                evidence_id: id.clone(),
                lexical_rank: None,
                semantic_rank: None,
                score: 0.0,
                rerank_rank: None,
                rerank_score: None,
            });
            traces.len() - 1
        }
    }
    let mut traces: Vec<FusionTrace> = Vec::new();
    for (index, id) in lexical.iter().enumerate() {
        let position = upsert(&mut traces, id);
        traces[position].lexical_rank = Some(index + 1);
        traces[position].score += 1.0 / (rrf_k + (index + 1) as f32);
    }
    for (index, id) in semantic.iter().enumerate() {
        let position = upsert(&mut traces, id);
        traces[position].semantic_rank = Some(index + 1);
        traces[position].score += 1.0 / (rrf_k + (index + 1) as f32);
    }
    traces.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.evidence_id.cmp(&right.evidence_id))
    });
    traces
}

/// Hybrid text search: BM25 and vector candidates fused by rank, filtered
/// by sensitivity (restricted items become recorded omissions), optionally
/// reranked by a cross-encoder over the fused top candidates, then
/// truncated to `limit`.
///
/// Provider failures in the semantic or rerank stages degrade the result
/// (lexical-only fusion, or fused order without rerank) with a recorded
/// `FusionDegradation` instead of failing the query.
pub(crate) async fn fused_search(
    index: &super::ContextIndex,
    query: &str,
    limit: usize,
    bm25_k1: f32,
    bm25_b: f32,
    rrf_k: f32,
) -> RuntimeResult<FusedSearch> {
    let rerank_config = &index.semantic.rerank;
    // Reranking needs the full candidate window before truncation.
    let window = if rerank_config.enabled {
        limit.max(rerank_config.top_n)
    } else {
        limit
    };
    let pool = window.saturating_mul(2).max(window);
    let mut degraded = Vec::new();
    let lexical_ranked: Vec<String> = index
        .lexical
        .search(query, pool, bm25_k1, bm25_b)
        .into_iter()
        .map(|(id, _score)| id.0)
        .collect();
    let semantic_ranked: Vec<String> = match index.semantic_vectors.as_ref() {
        None => Vec::new(),
        Some(vectors) => match embed_query(&index.semantic, query).await {
            Ok(query_vector) => vectors
                .search(&query_vector, pool)?
                .into_iter()
                .filter(|(_, score)| *score > 0.0)
                .map(|(id, _score)| id)
                .collect(),
            Err(RuntimeError::ProviderMessage {
                status, message, ..
            }) => {
                let status = status.map_or(String::new(), |code| format!(" (status {code})"));
                degraded.push(FusionDegradation {
                    stage: "semantic",
                    message: format!(
                        "query embedding provider failed{status}; results are lexical-only: {message}"
                    ),
                });
                Vec::new()
            }
            Err(error) => return Err(error),
        },
    };
    let fused = rrf_fuse(&lexical_ranked, &semantic_ranked, rrf_k);
    let mut evidence = Vec::new();
    let mut fusion = Vec::new();
    let mut omissions = Vec::new();
    for trace in fused {
        if evidence.len() >= window {
            break;
        }
        let Some(candidate) = index
            .evidence
            .iter()
            .find(|candidate| candidate.id.0 == trace.evidence_id)
        else {
            continue;
        };
        if candidate.sensitivity == ContextSensitivity::Restricted {
            omissions.push(FusionOmission {
                evidence_id: trace.evidence_id.clone(),
                reason: "restricted",
            });
            continue;
        }
        evidence.push(candidate.clone());
        fusion.push(trace);
    }
    if rerank_config.enabled {
        if let Some(degradation) =
            rerank_fused_candidates(index, query, &mut evidence, &mut fusion).await?
        {
            degraded.push(degradation);
        }
    }
    evidence.truncate(limit);
    fusion.truncate(limit);
    Ok(FusedSearch {
        evidence,
        fusion,
        omissions,
        degraded,
    })
}

/// Rerank the fused top candidates in place. The reranked window reorders
/// by cross-encoder relevance (ties keep fused order for determinism);
/// candidates beyond the window keep their fused positions. Returns the
/// degradation record when the provider fails; configuration errors
/// propagate.
async fn rerank_fused_candidates(
    index: &super::ContextIndex,
    query: &str,
    evidence: &mut [ContextEvidence],
    fusion: &mut [FusionTrace],
) -> RuntimeResult<Option<FusionDegradation>> {
    let window = index.semantic.rerank.top_n.min(evidence.len());
    if window == 0 {
        return Ok(None);
    }
    let candidates = evidence[..window]
        .iter()
        .map(|item| super::RerankCandidate {
            id: item.id.0.clone(),
            text: rerank_text(index, item),
            sensitivity: item.sensitivity,
        })
        .collect::<Vec<_>>();
    // The post-fusion sensitivity filter already excluded restricted
    // evidence; this guard keeps the policy explicit and load-bearing
    // even if the pipeline above changes.
    super::validate_rerank_batch(index.semantic.allow_restricted_hosted_inputs, &candidates)?;
    let reranker = super::HostedReranker::from_config(&index.semantic.rerank)?;
    let scored = match reranker.rerank(query, &candidates).await {
        Ok(scored) => scored,
        Err(RuntimeError::ProviderMessage {
            status, message, ..
        }) => {
            let status = status.map_or(String::new(), |code| format!(" (status {code})"));
            return Ok(Some(FusionDegradation {
                stage: "rerank",
                message: format!(
                    "rerank provider failed{status}; results keep the fused order: {message}"
                ),
            }));
        }
        Err(error) => return Err(error),
    };
    let mut score_by_position = vec![None; window];
    for (position, score) in scored {
        score_by_position[position] = Some(score);
    }
    // Sort window positions by reranker score (descending); unscored
    // candidates and ties keep their fused order.
    let mut order: Vec<usize> = (0..window).collect();
    order.sort_by(|&left, &right| {
        match (score_by_position[left], score_by_position[right]) {
            (Some(left_score), Some(right_score)) => right_score
                .total_cmp(&left_score)
                .then_with(|| left.cmp(&right)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.cmp(&right),
        }
    });
    let reordered_evidence = order
        .iter()
        .map(|&position| evidence[position].clone())
        .collect::<Vec<_>>();
    let reordered_fusion = order
        .iter()
        .enumerate()
        .map(|(rank, &position)| {
            let mut trace = fusion[position].clone();
            trace.rerank_rank = Some(rank + 1);
            trace.rerank_score = score_by_position[position];
            trace
        })
        .collect::<Vec<_>>();
    evidence[..window].clone_from_slice(&reordered_evidence);
    fusion[..window].clone_from_slice(&reordered_fusion);
    Ok(None)
}

/// The reranker scores the same text surface the embedding provider
/// embeds: summary, path, and the evidence's content slice.
fn rerank_text(index: &super::ContextIndex, evidence: &ContextEvidence) -> String {
    let content = evidence
        .path
        .as_ref()
        .and_then(|path| index.file_contents.get(path))
        .map(|content| super::chunking::slice_evidence_lines(content, evidence.range.as_ref()));
    super::context_embedding_text(evidence, content.as_deref())
}

async fn embed_query(
    semantic: &ContextSemanticConfig,
    query: &str,
) -> RuntimeResult<super::EmbeddingVector> {
    match semantic.mode {
        ContextSemanticMode::NoVector => {
            LocalHashEmbeddingProvider::new(256)?.embed(vec![]).await?
        }
        ContextSemanticMode::Local => {
            let provider = LocalHashEmbeddingProvider::new(256)?;
            provider
                .embed(vec![EmbeddingInput {
                    id: "query".to_string(),
                    text: query.to_string(),
                    sensitivity: ContextSensitivity::Private,
                }])
                .await?
        }
        ContextSemanticMode::LocalOnnx => {
            let provider = super::index::local_onnx_provider(semantic)?;
            provider
                .embed(vec![EmbeddingInput {
                    id: "query".to_string(),
                    text: query.to_string(),
                    sensitivity: ContextSensitivity::Private,
                }])
                .await?
        }
        ContextSemanticMode::Hosted => {
            let provider = HostedEmbeddingProvider::from_config(semantic)?;
            provider
                .embed(vec![EmbeddingInput {
                    id: "query".to_string(),
                    text: query.to_string(),
                    sensitivity: ContextSensitivity::Private,
                }])
                .await?
        }
    }
    .into_iter()
    .next()
    .ok_or(RuntimeError::Invariant(
        "query embedding provider returned no vectors",
    ))
}

pub(crate) fn read_line_span(
    content: &str,
    start_line: usize,
    end_line: usize,
) -> RuntimeResult<String> {
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

pub(crate) fn evidence_by_id(
    evidence: &[ContextEvidence],
    ids: &[EvidenceId],
) -> Vec<ContextEvidence> {
    evidence
        .iter()
        .filter(|candidate| ids.iter().any(|id| id == &candidate.id))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::rrf_fuse;

    fn ids(traces: &[super::FusionTrace]) -> Vec<&str> {
        traces
            .iter()
            .map(|trace| trace.evidence_id.as_str())
            .collect()
    }

    #[test]
    fn single_list_fusion_preserves_source_order() {
        let lexical = vec!["b".to_string(), "a".to_string(), "c".to_string()];
        let fused = rrf_fuse(&lexical, &[], 60.0);
        assert_eq!(ids(&fused), vec!["b", "a", "c"]);
        assert!(fused.iter().all(|trace| trace.semantic_rank.is_none()));
        assert_eq!(fused[0].lexical_rank, Some(1));
    }

    #[test]
    fn dual_membership_outranks_single_list_at_similar_positions() {
        let lexical = vec!["solo".to_string(), "both".to_string()];
        let semantic = vec!["other".to_string(), "both".to_string()];
        let fused = rrf_fuse(&lexical, &semantic, 60.0);
        assert_eq!(fused[0].evidence_id, "both");
        assert_eq!(fused[0].lexical_rank, Some(2));
        assert_eq!(fused[0].semantic_rank, Some(2));
        assert!(fused[0].score > fused[1].score);
    }

    #[test]
    fn equal_scores_tie_break_on_evidence_id() {
        let lexical = vec!["zeta".to_string()];
        let semantic = vec!["alpha".to_string()];
        let fused = rrf_fuse(&lexical, &semantic, 60.0);
        assert_eq!(ids(&fused), vec!["alpha", "zeta"]);
    }
}
