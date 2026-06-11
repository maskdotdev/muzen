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
}

/// Evidence excluded by the post-fusion sensitivity filter, with the reason
/// recorded instead of a silent drop.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FusionOmission {
    pub evidence_id: String,
    pub reason: &'static str,
}

pub(crate) struct FusedSearch {
    pub evidence: Vec<ContextEvidence>,
    pub fusion: Vec<FusionTrace>,
    pub omissions: Vec<FusionOmission>,
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

/// Hybrid text search: BM25 and vector candidates fused by rank, then
/// filtered by sensitivity (restricted items become recorded omissions),
/// then truncated to `limit`.
pub(crate) async fn fused_search(
    index: &super::ContextIndex,
    query: &str,
    limit: usize,
    bm25_k1: f32,
    bm25_b: f32,
    rrf_k: f32,
) -> RuntimeResult<FusedSearch> {
    let pool = limit.saturating_mul(2).max(limit);
    let lexical_ranked: Vec<String> = index
        .lexical
        .search(query, pool, bm25_k1, bm25_b)
        .into_iter()
        .map(|(id, _score)| id.0)
        .collect();
    let semantic_ranked: Vec<String> = match index.semantic_vectors.as_ref() {
        None => Vec::new(),
        Some(vectors) => {
            let query_vector = embed_query(&index.semantic, query).await?;
            vectors
                .search(&query_vector, pool)?
                .into_iter()
                .filter(|(_, score)| *score > 0.0)
                .map(|(id, _score)| id)
                .collect()
        }
    };
    let fused = rrf_fuse(&lexical_ranked, &semantic_ranked, rrf_k);
    let mut evidence = Vec::new();
    let mut fusion = Vec::new();
    let mut omissions = Vec::new();
    for trace in fused {
        if evidence.len() >= limit {
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
    Ok(FusedSearch {
        evidence,
        fusion,
        omissions,
    })
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
