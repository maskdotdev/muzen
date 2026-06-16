use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeResult};
use crate::reviewer_kernel::system::resolve_credential_ref;

use super::{
    ContextEngineConfig, ContextEvidence, ContextSemanticConfig, ContextSemanticMode,
    ContextSensitivity,
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

/// Default hosted embedding model when none is configured.
pub const DEFAULT_HOSTED_EMBEDDING_MODEL: &str = "text-embedding-3-small";

/// Stable identity of the embedding provider behind a vector: provider
/// kind plus the exact model id. Recorded in the index report/manifest as
/// provenance and used as the embedding-cache key prefix, so switching
/// models can never serve another model's vectors.
pub fn semantic_provider_tag(semantic: &ContextSemanticConfig) -> Option<String> {
    match semantic.mode {
        ContextSemanticMode::NoVector => None,
        ContextSemanticMode::Local => Some("local_hash_256".to_string()),
        ContextSemanticMode::LocalOnnx => Some(format!(
            "local_onnx:{}",
            semantic
                .local_onnx_model_dir
                .as_deref()
                .map(|dir| {
                    std::path::Path::new(dir)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| dir.to_string())
                })
                .unwrap_or_else(|| "unconfigured".to_string())
        )),
        ContextSemanticMode::Hosted => Some(format!(
            "hosted:{}",
            semantic
                .hosted_model
                .as_deref()
                .unwrap_or(DEFAULT_HOSTED_EMBEDDING_MODEL)
        )),
    }
}

/// Hosted embedding models cap tokens per input (8191 for OpenAI
/// `text-embedding-3-*`); ~30KB of text stays inside that at the 4
/// bytes/token estimate. The vector cache keys on the truncated text, so
/// a cache key always describes exactly what was embedded.
pub const MAX_EMBEDDING_TEXT_BYTES: usize = 30_000;

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
    let mut text = parts.join("\n");
    if text.len() > MAX_EMBEDDING_TEXT_BYTES {
        let mut end = MAX_EMBEDDING_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[async_trait]
impl EmbeddingProvider for LocalHashEmbeddingProvider {
    async fn embed(&self, inputs: Vec<EmbeddingInput>) -> RuntimeResult<Vec<EmbeddingVector>> {
        Ok(inputs
            .into_iter()
            .map(|input| self.embed_text(&input.text))
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct HostedEmbeddingProvider {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl HostedEmbeddingProvider {
    pub fn from_config(config: &ContextSemanticConfig) -> RuntimeResult<Self> {
        let credential_ref = config
            .hosted_credential_ref
            .as_deref()
            .unwrap_or("env:OPENAI_API_KEY");
        let api_key = resolve_credential_ref(credential_ref).map_err(|_| {
            RuntimeError::InvalidInput("hosted context embedding credential is unavailable".into())
        })?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| RuntimeError::Invariant("failed to build async HTTP client"))?;
        Ok(Self {
            http,
            base_url: config
                .hosted_base_url
                .as_deref()
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/')
                .to_string(),
            model: config
                .hosted_model
                .as_deref()
                .unwrap_or(DEFAULT_HOSTED_EMBEDDING_MODEL)
                .to_string(),
            api_key,
        })
    }
}

#[derive(Debug, Serialize)]
struct HostedEmbeddingRequest {
    model: String,
    input: Vec<String>,
    encoding_format: &'static str,
}

#[derive(Debug, Deserialize)]
struct HostedEmbeddingResponse {
    data: Vec<HostedEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct HostedEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

/// Inputs per `/embeddings` request. Hosted providers cap both the input
/// array (OpenAI: 2048) and tokens per request; 128 chunk-sized inputs
/// stays far inside both.
const HOSTED_EMBEDDING_REQUEST_BATCH: usize = 128;

#[async_trait]
impl EmbeddingProvider for HostedEmbeddingProvider {
    async fn embed(&self, inputs: Vec<EmbeddingInput>) -> RuntimeResult<Vec<EmbeddingVector>> {
        let mut vectors = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(HOSTED_EMBEDDING_REQUEST_BATCH) {
            vectors.extend(self.embed_request(batch).await?);
        }
        Ok(vectors)
    }
}

impl HostedEmbeddingProvider {
    async fn embed_request(
        &self,
        inputs: &[EmbeddingInput],
    ) -> RuntimeResult<Vec<EmbeddingVector>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let request = HostedEmbeddingRequest {
            model: self.model.clone(),
            input: inputs.iter().map(|input| input.text.clone()).collect(),
            encoding_format: "float",
        };
        let response = self
            .http
            .post(format!("{}/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(|error| RuntimeError::ProviderMessage {
                status: None,
                retryable: error.is_timeout() || error.is_connect(),
                message: error.to_string(),
            })?;
        let status = response.status();
        if !status.is_success() {
            let retryable = status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT;
            let message = response.text().await.unwrap_or_default();
            return Err(RuntimeError::ProviderMessage {
                status: Some(status.as_u16()),
                retryable,
                message,
            });
        }
        let mut data = response
            .json::<HostedEmbeddingResponse>()
            .await
            .map_err(|error| RuntimeError::ProviderMessage {
                status: Some(status.as_u16()),
                retryable: false,
                message: error.to_string(),
            })?
            .data;
        data.sort_by_key(|item| item.index);
        Ok(data
            .into_iter()
            .map(|item| EmbeddingVector::normalized(item.embedding))
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

    pub fn get(&self, id: &str) -> Option<&EmbeddingVector> {
        self.vectors.get(id)
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
#[cfg(test)]
pub enum SemanticInputDecision {
    Allowed,
    SkippedNoVector,
    SkippedRestrictedHosted,
}

#[cfg(test)]
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
        ContextSemanticMode::Local
        | ContextSemanticMode::LocalOnnx
        | ContextSemanticMode::Hosted => SemanticInputDecision::Allowed,
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

pub(crate) fn cosine_similarity(left: &EmbeddingVector, right: &EmbeddingVector) -> f32 {
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
