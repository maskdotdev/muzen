use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::runtime::contracts::{RuntimeError, RuntimeResult};
use crate::util::resolve_credential_ref;

use super::{ContextRerankConfig, ContextSensitivity};

/// One fused candidate offered to the reranker: the evidence id and the
/// same text surface the embedding provider sees.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankCandidate {
    pub id: String,
    pub text: String,
    pub sensitivity: ContextSensitivity,
}

/// Reranker speaking the Cohere-style `/rerank` contract. Works against
/// Cohere, Jina, and in-house servers (vLLM, Infinity, or any service
/// exposing the same shape) through `base_url`; the bearer credential is
/// optional for unauthenticated in-house deployments.
#[derive(Debug, Clone)]
pub struct HostedReranker {
    http: reqwest::Client,
    base_url: String,
    model: Option<String>,
    api_key: Option<String>,
}

impl HostedReranker {
    pub fn from_config(config: &ContextRerankConfig) -> RuntimeResult<Self> {
        let base_url = config.base_url.as_deref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "context rerank requires rerank base url when enabled".to_string(),
            )
        })?;
        let api_key = match config.credential_ref.as_deref() {
            None => None,
            Some(credential_ref) => Some(resolve_credential_ref(credential_ref).map_err(|_| {
                RuntimeError::InvalidInput("context rerank credential is unavailable".into())
            })?),
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| RuntimeError::Invariant("failed to build async HTTP client"))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key,
        })
    }

    /// Rank `candidates` against `query`. Returns `(candidate index,
    /// relevance score)` in descending relevance order as reported by the
    /// provider; ties and ordering quirks are normalized by the caller.
    pub async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
    ) -> RuntimeResult<Vec<(usize, f32)>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let request = HostedRerankRequest {
            model: self.model.clone(),
            query: query.to_string(),
            documents: candidates
                .iter()
                .map(|candidate| candidate.text.clone())
                .collect(),
            top_n: candidates.len(),
        };
        let mut builder = self.http.post(format!("{}/rerank", self.base_url));
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }
        let response = builder.json(&request).send().await.map_err(|error| {
            RuntimeError::ProviderMessage {
                status: None,
                retryable: error.is_timeout() || error.is_connect(),
                message: error.to_string(),
            }
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
        let results = response
            .json::<HostedRerankResponse>()
            .await
            .map_err(|error| RuntimeError::ProviderMessage {
                status: Some(status.as_u16()),
                retryable: false,
                message: error.to_string(),
            })?
            .results;
        if results
            .iter()
            .any(|result| result.index >= candidates.len())
        {
            return Err(RuntimeError::ProviderMessage {
                status: Some(status.as_u16()),
                retryable: false,
                message: "context reranker returned an out-of-range document index".to_string(),
            });
        }
        Ok(results
            .into_iter()
            .map(|result| (result.index, result.relevance_score))
            .collect())
    }
}

#[derive(Debug, Serialize)]
struct HostedRerankRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    query: String,
    documents: Vec<String>,
    top_n: usize,
}

#[derive(Debug, Deserialize)]
struct HostedRerankResponse {
    results: Vec<HostedRerankResult>,
}

#[derive(Debug, Deserialize)]
struct HostedRerankResult {
    index: usize,
    relevance_score: f32,
}

/// Reranking is a hosted call: restricted evidence cannot be sent without
/// the same explicit opt-in that governs hosted embeddings.
pub fn validate_rerank_batch(
    allow_restricted_hosted_inputs: bool,
    candidates: &[RerankCandidate],
) -> RuntimeResult<()> {
    if !allow_restricted_hosted_inputs
        && candidates
            .iter()
            .any(|candidate| candidate.sensitivity == ContextSensitivity::Restricted)
    {
        return Err(RuntimeError::InvalidInput(
            "context rerank cannot receive restricted evidence without explicit opt-in"
                .to_string(),
        ));
    }
    Ok(())
}
