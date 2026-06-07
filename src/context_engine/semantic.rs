use async_trait::async_trait;
use std::collections::HashMap;

use crate::runtime::contracts::{RuntimeError, RuntimeResult};

use super::{
    ContextEmbeddingProviderKind, ContextEngineConfig, ContextEvidence, ContextEvidenceKind,
    ContextPackPurpose, ContextSemanticMode, ContextSensitivity,
};

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingInput {
    pub id: String,
    pub text: String,
    pub sensitivity: ContextSensitivity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingVector {
    pub values: Vec<f32>,
}

impl EmbeddingVector {
    pub fn normalized(mut values: Vec<f32>) -> Self {
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut values {
                *value /= norm;
            }
        }
        Self { values }
    }
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn kind(&self) -> ContextEmbeddingProviderKind;

    async fn embed(&self, inputs: Vec<EmbeddingInput>) -> RuntimeResult<Vec<EmbeddingVector>>;
}

pub trait VectorIndex: Send + Sync {
    fn put(&mut self, id: String, vector: EmbeddingVector) -> RuntimeResult<()>;
    fn search(&self, vector: &EmbeddingVector, limit: usize) -> RuntimeResult<Vec<(String, f32)>>;
}

#[derive(Debug, Clone)]
pub struct LocalHashEmbeddingProvider {
    dimensions: usize,
}

impl LocalHashEmbeddingProvider {
    pub fn new(dimensions: usize) -> RuntimeResult<Self> {
        if dimensions == 0 {
            return Err(RuntimeError::InvalidInput(
                "local context embedding dimensions must be greater than zero".to_string(),
            ));
        }
        Ok(Self { dimensions })
    }

    pub fn embed_text(&self, text: &str) -> EmbeddingVector {
        let mut values = vec![0.0; self.dimensions];
        for token in semantic_tokens(text) {
            let index = stable_token_hash(token) as usize % self.dimensions;
            values[index] += 1.0;
        }
        EmbeddingVector::normalized(values)
    }
}

pub fn context_embedding_text(evidence: &ContextEvidence, file_content: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(summary) = &evidence.summary {
        parts.push(summary.clone());
    }
    if let Some(path) = &evidence.path {
        parts.push(path.display());
    }
    if let Some(content) = file_content {
        parts.push(content.to_string());
    }
    parts.join("\n")
}

#[async_trait]
impl EmbeddingProvider for LocalHashEmbeddingProvider {
    fn kind(&self) -> ContextEmbeddingProviderKind {
        ContextEmbeddingProviderKind::Local
    }

    async fn embed(&self, inputs: Vec<EmbeddingInput>) -> RuntimeResult<Vec<EmbeddingVector>> {
        Ok(inputs
            .into_iter()
            .map(|input| self.embed_text(&input.text))
            .collect())
    }
}

#[derive(Debug, Default, Clone)]
pub struct InMemoryVectorIndex {
    vectors: HashMap<String, EmbeddingVector>,
}

impl InMemoryVectorIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

impl VectorIndex for InMemoryVectorIndex {
    fn put(&mut self, id: String, vector: EmbeddingVector) -> RuntimeResult<()> {
        self.vectors.insert(id, vector);
        Ok(())
    }

    fn search(&self, vector: &EmbeddingVector, limit: usize) -> RuntimeResult<Vec<(String, f32)>> {
        let mut scored = self
            .vectors
            .iter()
            .map(|(id, candidate)| (id.clone(), cosine_similarity(vector, candidate)))
            .collect::<Vec<_>>();
        scored.sort_by(|(left_id, left_score), (right_id, right_score)| {
            right_score
                .partial_cmp(left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left_id.cmp(right_id))
        });
        scored.truncate(limit);
        Ok(scored)
    }
}

#[derive(Debug, Default)]
pub struct NoVectorIndex;

impl VectorIndex for NoVectorIndex {
    fn put(&mut self, _id: String, _vector: EmbeddingVector) -> RuntimeResult<()> {
        Ok(())
    }

    fn search(
        &self,
        _vector: &EmbeddingVector,
        _limit: usize,
    ) -> RuntimeResult<Vec<(String, f32)>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SemanticInputDecision {
    Allowed,
    SkippedNoVector,
    SkippedRestrictedHosted,
}

pub fn semantic_input_decision(
    config: &ContextEngineConfig,
    evidence: &ContextEvidence,
) -> SemanticInputDecision {
    match config.semantic.mode {
        ContextSemanticMode::NoVector => SemanticInputDecision::SkippedNoVector,
        ContextSemanticMode::Hosted
            if evidence.sensitivity == ContextSensitivity::Restricted
                && !config.semantic.allow_restricted_hosted_inputs =>
        {
            SemanticInputDecision::SkippedRestrictedHosted
        }
        ContextSemanticMode::Local | ContextSemanticMode::Hosted => SemanticInputDecision::Allowed,
    }
}

pub fn semantic_score_for_purpose(
    config: &ContextEngineConfig,
    evidence: &ContextEvidence,
    purpose: ContextPackPurpose,
) -> f32 {
    if semantic_input_decision(config, evidence) != SemanticInputDecision::Allowed {
        return 0.0;
    }
    match (purpose, evidence.kind) {
        (ContextPackPurpose::Architecture, ContextEvidenceKind::CrossRepoContract) => 0.08,
        (ContextPackPurpose::Architecture, ContextEvidenceKind::Doc) => 0.05,
        (ContextPackPurpose::Tests, ContextEvidenceKind::Test) => 0.05,
        (ContextPackPurpose::Security, ContextEvidenceKind::RepositoryRule) => 0.04,
        _ => 0.01,
    }
}

pub fn validate_embedding_batch(
    config: &ContextEngineConfig,
    inputs: &[EmbeddingInput],
) -> RuntimeResult<()> {
    if config.semantic.mode == ContextSemanticMode::NoVector {
        return Ok(());
    }
    if inputs.len() > config.semantic.max_embedding_inputs {
        return Err(RuntimeError::LimitExceeded {
            kind: "context_embedding_inputs",
        });
    }
    if config.semantic.mode == ContextSemanticMode::Hosted
        && !config.semantic.allow_restricted_hosted_inputs
        && inputs
            .iter()
            .any(|input| input.sensitivity == ContextSensitivity::Restricted)
    {
        return Err(RuntimeError::InvalidInput(
            "hosted context embeddings cannot receive restricted evidence without explicit opt-in"
                .to_string(),
        ));
    }
    Ok(())
}

fn semantic_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 2)
}

fn stable_token_hash(token: &str) -> u64 {
    token.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte.to_ascii_lowercase())).wrapping_mul(0x100000001b3)
    })
}

fn cosine_similarity(left: &EmbeddingVector, right: &EmbeddingVector) -> f32 {
    if left.values.len() != right.values.len() {
        return 0.0;
    }
    left.values
        .iter()
        .zip(&right.values)
        .map(|(left, right)| left * right)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_hash_embeddings_rank_similar_text() {
        let provider = LocalHashEmbeddingProvider::new(64).unwrap();
        let vectors = provider
            .embed(vec![
                EmbeddingInput {
                    id: "auth".to_string(),
                    text: "authorize token user request".to_string(),
                    sensitivity: ContextSensitivity::Private,
                },
                EmbeddingInput {
                    id: "billing".to_string(),
                    text: "invoice payment tax total".to_string(),
                    sensitivity: ContextSensitivity::Private,
                },
            ])
            .await
            .unwrap();
        let query = provider
            .embed(vec![EmbeddingInput {
                id: "query".to_string(),
                text: "token authorization request".to_string(),
                sensitivity: ContextSensitivity::Private,
            }])
            .await
            .unwrap()
            .remove(0);

        let mut index = InMemoryVectorIndex::new();
        index.put("auth".to_string(), vectors[0].clone()).unwrap();
        index
            .put("billing".to_string(), vectors[1].clone())
            .unwrap();

        let results = index.search(&query, 2).unwrap();
        assert_eq!(results[0].0, "auth");
        assert!(results[0].1 > results[1].1);
    }
}
