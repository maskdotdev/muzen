use async_trait::async_trait;

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

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn kind(&self) -> ContextEmbeddingProviderKind;

    async fn embed(&self, inputs: Vec<EmbeddingInput>) -> RuntimeResult<Vec<EmbeddingVector>>;
}

pub trait VectorIndex: Send + Sync {
    fn put(&mut self, id: String, vector: EmbeddingVector) -> RuntimeResult<()>;
    fn search(&self, vector: &EmbeddingVector, limit: usize) -> RuntimeResult<Vec<(String, f32)>>;
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
