use serde_json::Value;

use crate::runtime::contracts::{EvidenceId, RepoPath, RuntimeError, RuntimeResult};

use super::{
    context_embedding_text, ContextEvidence, ContextSemanticConfig, ContextSemanticMode,
    ContextSensitivity, ContextTrust, EmbeddingInput, EmbeddingProvider, HostedEmbeddingProvider,
    InMemoryVectorIndex, LocalHashEmbeddingProvider, VectorIndex,
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

/// Rank evidence for a text query with BM25 over the lexical index.
/// Returns scored, rank-ordered evidence (highest first).
pub(crate) fn search_evidence(
    index: &super::ContextIndex,
    query: &str,
    limit: usize,
    bm25_k1: f32,
    bm25_b: f32,
) -> Vec<ContextEvidence> {
    let ranked = index.lexical.search(query, limit, bm25_k1, bm25_b);
    ranked
        .into_iter()
        .filter_map(|(id, _score)| {
            index
                .evidence
                .iter()
                .find(|candidate| candidate.id == id)
                .cloned()
        })
        .collect()
}

pub(crate) async fn merge_semantic_search(
    mut lexical: Vec<ContextEvidence>,
    evidence: &[ContextEvidence],
    file_contents: &std::collections::BTreeMap<RepoPath, String>,
    semantic: &ContextSemanticConfig,
    semantic_vectors: Option<&InMemoryVectorIndex>,
    query: &str,
    limit: usize,
) -> RuntimeResult<Vec<ContextEvidence>> {
    let Some(semantic_vectors) = semantic_vectors else {
        return Ok(lexical);
    };
    let query_vector = embed_query(semantic, query).await?;
    let semantic_hits = semantic_vectors.search(&query_vector, limit)?;
    for (id, score) in semantic_hits {
        if score <= 0.0 || lexical.iter().any(|candidate| candidate.id.0 == id) {
            continue;
        }
        if let Some(candidate) = evidence.iter().find(|candidate| candidate.id.0 == id) {
            let content = candidate
                .path
                .as_ref()
                .and_then(|path| file_contents.get(path))
                .map(|content| {
                    super::chunking::slice_evidence_lines(content, candidate.range.as_ref())
                });
            let candidate_text =
                context_embedding_text(candidate, content.as_deref()).to_ascii_lowercase();
            if query
                .split_whitespace()
                .any(|term| candidate_text.contains(&term.to_ascii_lowercase()))
            {
                lexical.push(candidate.clone());
            }
        }
        if lexical.len() >= limit {
            break;
        }
    }
    Ok(lexical)
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
