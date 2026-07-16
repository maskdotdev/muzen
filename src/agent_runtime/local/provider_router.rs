use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};
use serde_json::{json, Value};

use super::credentials::CredentialResolver;
use super::provider::{
    anthropic_request, chat_request, parse_anthropic, parse_chat, parse_responses,
    responses_request, ModelProvider, ModelProviderError, ModelRequest, ModelTurn,
};
use crate::agent_runtime::{ExecutionErrorCode, ModelProtocol, ModelProviderKind, MuzenError};

const ANTHROPIC_DEFAULT: &str = "https://api.anthropic.com";
const OPENAI_DEFAULT: &str = "https://api.openai.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const SAFE_BODY_LIMIT: usize = 512;

pub(crate) struct ProviderRouter {
    credentials: Arc<dyn CredentialResolver>,
    client: Client,
    allow_loopback_http: bool,
}

impl ProviderRouter {
    pub(crate) fn new(
        credentials: Arc<dyn CredentialResolver>,
        allow_loopback_http: bool,
    ) -> Result<Self, MuzenError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                MuzenError::internal(format!("failed to build model HTTP client: {error}"))
            })?;
        Ok(Self {
            credentials,
            client,
            allow_loopback_http,
        })
    }

    async fn credential(&self, request: &ModelRequest) -> Result<String, ModelProviderError> {
        let secret = self
            .credentials
            .resolve(&request.model.credential)
            .await
            .ok_or_else(|| {
                ModelProviderError::new("model credential is unavailable")
                    .with_code(ExecutionErrorCode::SecretUnavailable)
            })?;
        std::str::from_utf8(secret.as_bytes())
            .map(str::to_owned)
            .map_err(|_| {
                ModelProviderError::new("model credential is not valid UTF-8")
                    .with_code(ExecutionErrorCode::SecretUnavailable)
            })
    }

    fn endpoint(&self, request: &ModelRequest) -> Result<Url, ModelProviderError> {
        let (base, suffix) = match (request.model.provider, request.model.protocol) {
            (ModelProviderKind::Anthropic, ModelProtocol::Messages) => (
                request
                    .model
                    .base_url
                    .as_deref()
                    .unwrap_or(ANTHROPIC_DEFAULT),
                "v1/messages",
            ),
            (ModelProviderKind::OpenaiCompatible, ModelProtocol::ChatCompletions) => (
                request.model.base_url.as_deref().unwrap_or(OPENAI_DEFAULT),
                "chat/completions",
            ),
            (ModelProviderKind::OpenaiCompatible, ModelProtocol::Responses) => (
                request.model.base_url.as_deref().unwrap_or(OPENAI_DEFAULT),
                "responses",
            ),
            _ => {
                return Err(ModelProviderError::new(
                    "unsupported model provider protocol",
                ))
            }
        };
        let base =
            Url::parse(base).map_err(|_| ModelProviderError::new("model base URL is invalid"))?;
        if request.model.base_url.is_some() {
            let loopback = base.host().is_some_and(|host| {
                let serialized = host.to_string();
                serialized == "localhost"
                    || serialized
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<IpAddr>()
                        .is_ok_and(|address| {
                            address == IpAddr::V4(Ipv4Addr::LOCALHOST)
                                || address == IpAddr::V6(Ipv6Addr::LOCALHOST)
                        })
            });
            let allowed = base.scheme() == "https"
                || (base.scheme() == "http" && loopback && self.allow_loopback_http);
            if !allowed {
                return Err(ModelProviderError::new(
                    "model base URL must use HTTPS; loopback HTTP requires explicit local opt-in",
                ));
            }
        }
        let joined = format!("{}/{}", base.as_str().trim_end_matches('/'), suffix);
        Url::parse(&joined).map_err(|_| ModelProviderError::new("model endpoint URL is invalid"))
    }

    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
        credential: &str,
    ) -> Result<Value, ModelProviderError> {
        let response = builder.send().await.map_err(|error| {
            ModelProviderError::new(format!("model provider transport error: {error}"))
                .with_retryable(true)
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            ModelProviderError::new(format!("failed to read model provider response: {error}"))
                .with_retryable(true)
        })?;
        if !status.is_success() {
            return Err(http_error(status, &body, credential));
        }
        serde_json::from_str(&body).map_err(|error| {
            ModelProviderError::new(format!("invalid model provider JSON response: {error}"))
                .with_details(json!({ "status": status.as_u16() }))
        })
    }
}

#[async_trait]
impl ModelProvider for ProviderRouter {
    async fn complete(&self, request: ModelRequest) -> Result<ModelTurn, ModelProviderError> {
        let credential = self.credential(&request).await?;
        let endpoint = self.endpoint(&request)?;
        match (request.model.provider, request.model.protocol) {
            (ModelProviderKind::Anthropic, ModelProtocol::Messages) => {
                let response = self
                    .send(
                        self.client
                            .post(endpoint)
                            .header("x-api-key", &credential)
                            .header("anthropic-version", ANTHROPIC_VERSION)
                            .json(&anthropic_request(&request)),
                        &credential,
                    )
                    .await?;
                parse_anthropic(&request, response)
            }
            (ModelProviderKind::OpenaiCompatible, ModelProtocol::ChatCompletions) => {
                let response = self
                    .send(
                        self.client
                            .post(endpoint)
                            .bearer_auth(&credential)
                            .json(&chat_request(&request)),
                        &credential,
                    )
                    .await?;
                parse_chat(&request, response)
            }
            (ModelProviderKind::OpenaiCompatible, ModelProtocol::Responses) => {
                let response = self
                    .send(
                        self.client
                            .post(endpoint)
                            .bearer_auth(&credential)
                            .json(&responses_request(&request)),
                        &credential,
                    )
                    .await?;
                parse_responses(&request, response)
            }
            _ => Err(ModelProviderError::new(
                "unsupported model provider protocol",
            )),
        }
    }
}

fn http_error(status: StatusCode, body: &str, credential: &str) -> ModelProviderError {
    let retryable = status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error();
    ModelProviderError::new(format!("model provider returned HTTP {status}"))
        .with_retryable(retryable)
        .with_details(json!({
            "status": status.as_u16(),
            "body": safe_excerpt(body, credential),
        }))
}

fn safe_excerpt(body: &str, credential: &str) -> String {
    let redacted = if credential.is_empty() {
        body.to_owned()
    } else {
        body.replace(credential, "[REDACTED]")
    };
    redacted
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .take(SAFE_BODY_LIMIT)
        .collect()
}
