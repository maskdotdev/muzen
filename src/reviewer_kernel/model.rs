use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::model_anthropic::{
    anthropic_default_base_url, AnthropicMessagesClient,
};
use crate::reviewer_kernel::policy::ReviewerPolicy;
use crate::reviewer_kernel::review_contract::{
    ModelApiProtocol, ModelProfileRefV1, ProviderKind, TokenUsage,
};
use crate::reviewer_kernel::system::{redact_known_secrets, resolve_credential_ref};
use crate::reviewer_kernel::tool_engine::ToolRegistry;

#[async_trait]
pub trait ConcurrentModelClient: Send + Sync {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
        cancel: CancellationToken,
    ) -> RuntimeResult<ModelTurn>;

    fn estimate_cost(&self, _usage: &TokenUsage) -> Option<ModelCostEstimate> {
        None
    }
}

#[async_trait]
pub trait ConcurrentModelRouter: Send + Sync {
    async fn client_for(
        &self,
        scope: &SessionScope,
    ) -> RuntimeResult<Arc<dyn ConcurrentModelClient>>;
}

pub struct StaticModelRouter {
    client: Arc<dyn ConcurrentModelClient>,
}

impl StaticModelRouter {
    pub fn new(client: Arc<dyn ConcurrentModelClient>) -> Self {
        Self { client }
    }
}

impl std::fmt::Debug for StaticModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticModelRouter").finish_non_exhaustive()
    }
}

#[async_trait]
impl ConcurrentModelRouter for StaticModelRouter {
    async fn client_for(
        &self,
        _scope: &SessionScope,
    ) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
        Ok(Arc::clone(&self.client))
    }
}

pub struct ProfileModelRouter {
    clients: HashMap<String, Arc<dyn ConcurrentModelClient>>,
    default_profile_id: String,
}

pub trait CredentialResolver: Send + Sync {
    fn resolve_credential(&self, credential_ref: &str) -> RuntimeResult<String>;
}

#[derive(Debug, Clone, Default)]
pub struct EnvCredentialResolver;

impl CredentialResolver for EnvCredentialResolver {
    fn resolve_credential(&self, credential_ref: &str) -> RuntimeResult<String> {
        resolve_credential_ref(credential_ref)
            .map_err(|_| RuntimeError::InvalidInput("model credential is unavailable".to_string()))
    }
}

impl ProfileModelRouter {
    pub(crate) fn from_profiles(
        profiles: &[ModelProfileRefV1],
        default_profile_id: String,
        default_base_url: String,
        limiter: Arc<ModelLimiter>,
        tool_registry: Arc<ToolRegistry>,
        reviewer_policy: Arc<ReviewerPolicy>,
        credential_resolver: Arc<dyn CredentialResolver>,
    ) -> RuntimeResult<Self> {
        if profiles.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "at least one model profile is required".to_string(),
            ));
        }
        let mut clients = HashMap::new();
        for profile in profiles {
            let client = match profile.provider_kind {
                ProviderKind::OpenaiCompatible => openai_client_from_profile(
                    profile.clone(),
                    profile_base_url(profile, &default_base_url),
                    Arc::clone(&limiter),
                    Arc::clone(&tool_registry),
                    Arc::clone(&reviewer_policy),
                    Arc::clone(&credential_resolver),
                )?,
                ProviderKind::Anthropic => anthropic_client_from_profile(
                    profile.clone(),
                    // Anthropic profiles never fall back to the run-level
                    // OpenAI-compatible endpoint.
                    profile_base_url(profile, &anthropic_default_base_url()),
                    Arc::clone(&limiter),
                    Arc::clone(&tool_registry),
                    Arc::clone(&reviewer_policy),
                    Arc::clone(&credential_resolver),
                )?,
            };
            clients.insert(profile.id.clone(), client);
        }
        if !clients.contains_key(&default_profile_id) {
            return Err(RuntimeError::InvalidInput(format!(
                "missing default model profile {default_profile_id}"
            )));
        }
        Ok(Self {
            clients,
            default_profile_id,
        })
    }
}

impl std::fmt::Debug for ProfileModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileModelRouter")
            .field("clients", &self.clients.len())
            .field("default_profile_id", &self.default_profile_id)
            .finish()
    }
}

#[async_trait]
impl ConcurrentModelRouter for ProfileModelRouter {
    async fn client_for(
        &self,
        scope: &SessionScope,
    ) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
        if let Some(profile_id) = scope.model_profile_id.as_ref() {
            return self.clients.get(profile_id).cloned().ok_or_else(|| {
                RuntimeError::InvalidInput(format!("missing model profile {profile_id}"))
            });
        }
        self.clients
            .get(&self.default_profile_id)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "missing model profile {}",
                    self.default_profile_id
                ))
            })
    }
}

#[derive(Debug)]
pub struct ModelLimiter {
    global: Arc<Semaphore>,
    per_provider: DashMap<String, Arc<Semaphore>>,
    per_profile: DashMap<String, Arc<Semaphore>>,
    per_key: DashMap<String, Arc<Semaphore>>,
    per_session: DashMap<String, Arc<Semaphore>>,
    max_per_provider: usize,
    max_per_profile: usize,
    max_per_key: usize,
    max_per_session: usize,
}

#[derive(Debug)]
pub struct ModelPermit {
    _global: OwnedSemaphorePermit,
    _buckets: Vec<OwnedSemaphorePermit>,
}

impl ModelLimiter {
    #[cfg(test)]
    pub fn new(global_concurrency: usize) -> Self {
        Self::new_with_per_key(global_concurrency, global_concurrency)
    }

    pub fn new_with_per_key(global_concurrency: usize, max_per_key: usize) -> Self {
        Self::new_with_buckets(
            global_concurrency,
            global_concurrency,
            global_concurrency,
            max_per_key,
            1,
        )
    }

    pub fn new_with_buckets(
        global_concurrency: usize,
        max_per_provider: usize,
        max_per_profile: usize,
        max_per_key: usize,
        max_per_session: usize,
    ) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_concurrency.max(1))),
            per_provider: DashMap::new(),
            per_profile: DashMap::new(),
            per_key: DashMap::new(),
            per_session: DashMap::new(),
            max_per_provider: max_per_provider.max(1),
            max_per_profile: max_per_profile.max(1),
            max_per_key: max_per_key.max(1),
            max_per_session: max_per_session.max(1),
        }
    }

    #[cfg(test)]
    pub async fn acquire_for_key(&self, key: &str) -> RuntimeResult<ModelPermit> {
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        let key_limiter = self
            .per_key
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.max_per_key)))
            .clone();
        let key = key_limiter
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        Ok(ModelPermit {
            _global: global,
            _buckets: vec![key],
        })
    }

    pub async fn acquire_for_model(
        &self,
        provider: &str,
        profile_id: &str,
        credential_ref: &str,
        session_id: &SessionId,
    ) -> RuntimeResult<ModelPermit> {
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        let buckets = vec![
            self.acquire_bucket(&self.per_provider, provider, self.max_per_provider)
                .await?,
            self.acquire_bucket(&self.per_profile, profile_id, self.max_per_profile)
                .await?,
            self.acquire_bucket(&self.per_key, credential_ref, self.max_per_key)
                .await?,
            self.acquire_bucket(&self.per_session, &session_id.0, self.max_per_session)
                .await?,
        ];
        Ok(ModelPermit {
            _global: global,
            _buckets: buckets,
        })
    }

    async fn acquire_bucket(
        &self,
        buckets: &DashMap<String, Arc<Semaphore>>,
        key: &str,
        max_per_bucket: usize,
    ) -> RuntimeResult<OwnedSemaphorePermit> {
        buckets
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(max_per_bucket)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)
    }
}

/// Each profile may route to its own endpoint; profiles without one share the
/// run-level default.
fn profile_base_url(profile: &ModelProfileRefV1, default_base_url: &str) -> String {
    profile
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| default_base_url.to_string())
}

fn openai_client_from_profile(
    profile: ModelProfileRefV1,
    base_url: String,
    limiter: Arc<ModelLimiter>,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    credential_resolver: Arc<dyn CredentialResolver>,
) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
    match profile.api_protocol {
        ModelApiProtocol::Responses => Ok(Arc::new(OpenAiResponsesClient::from_profile(
            profile,
            base_url,
            limiter,
            tool_registry,
            reviewer_policy,
            credential_resolver,
        )?)),
        ModelApiProtocol::Messages => Err(RuntimeError::InvalidInput(format!(
            "model profile {} is openai_compatible but uses the messages protocol; use provider anthropic",
            profile.id
        ))),
    }
}

fn anthropic_client_from_profile(
    profile: ModelProfileRefV1,
    base_url: String,
    limiter: Arc<ModelLimiter>,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    credential_resolver: Arc<dyn CredentialResolver>,
) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
    if profile.api_protocol != ModelApiProtocol::Messages {
        return Err(RuntimeError::InvalidInput(format!(
            "model profile {} is anthropic and must use the messages protocol",
            profile.id
        )));
    }
    Ok(Arc::new(AnthropicMessagesClient::from_profile(
        profile,
        base_url,
        limiter,
        tool_registry,
        reviewer_policy,
        credential_resolver,
    )?))
}

#[derive(Debug)]
pub struct OpenAiResponsesClient {
    http: reqwest::Client,
    profile: ModelProfileRefV1,
    api_key: String,
    base_url: String,
    limiter: Arc<ModelLimiter>,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
}

impl OpenAiResponsesClient {
    pub(crate) fn from_profile(
        profile: ModelProfileRefV1,
        base_url: String,
        limiter: Arc<ModelLimiter>,
        tool_registry: Arc<ToolRegistry>,
        reviewer_policy: Arc<ReviewerPolicy>,
        credential_resolver: Arc<dyn CredentialResolver>,
    ) -> RuntimeResult<Self> {
        let api_key = credential_resolver.resolve_credential(&profile.credential_ref)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|_| RuntimeError::Invariant("failed to build async HTTP client"))?;
        Ok(Self {
            http,
            profile,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            limiter,
            tool_registry,
            reviewer_policy,
        })
    }
}

#[async_trait]
impl ConcurrentModelClient for OpenAiResponsesClient {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        _turn_id: TurnId,
        cancel: CancellationToken,
    ) -> RuntimeResult<ModelTurn> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let _permit = self
            .limiter
            .acquire_for_model(
                "openai_compatible",
                &self.profile.id,
                &self.profile.credential_ref,
                &scope.id,
            )
            .await?;
        let body = responses_request_body(
            &self.profile,
            &self.reviewer_policy,
            &self.tool_registry,
            scope,
            transcript,
        )?;
        let response = self
            .http
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| RuntimeError::Provider {
                status: None,
                retryable: true,
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(provider_http_error(status, response).await);
        }
        let decoded: ResponsesResponse =
            response.json().await.map_err(|_| RuntimeError::Provider {
                status: None,
                retryable: false,
            })?;
        parse_responses_response(decoded, &self.tool_registry)
    }
}

pub(crate) async fn provider_http_error(
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> RuntimeError {
    let message = response
        .text()
        .await
        .map(provider_error_message)
        .unwrap_or_else(|_| "provider error body unavailable".to_string());
    let retryable = (status.as_u16() == 429 && !is_non_retryable_provider_quota_error(&message))
        || status.is_server_error();
    RuntimeError::ProviderMessage {
        status: Some(status.as_u16()),
        retryable,
        message,
    }
}

pub(crate) fn provider_error_message(body: String) -> String {
    let redacted = redact_known_secrets(body.trim(), &[]);
    truncate_chars(&redacted, 1_000)
}

fn is_non_retryable_provider_quota_error(message: &str) -> bool {
    message.contains("\"code\":\"insufficient_quota\"")
        || message.contains("\"code\": \"insufficient_quota\"")
        || message.contains("insufficient_quota")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn responses_request_body(
    profile: &ModelProfileRefV1,
    reviewer_policy: &ReviewerPolicy,
    tool_registry: &ToolRegistry,
    scope: &SessionScope,
    transcript: &[ConversationItem],
) -> RuntimeResult<Value> {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for item in transcript {
        match item {
            ConversationItem::System { content } => instructions.push(content.as_str()),
            ConversationItem::User { content } => {
                input.push(response_message("user", content));
            }
            ConversationItem::AssistantText { content } => {
                input.push(response_message("assistant", content));
            }
            ConversationItem::AssistantToolCalls { calls } => {
                for call in calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.call_id.0,
                        "name": model_alias_for_tool(tool_registry, &call.name)?.as_str(),
                        "arguments": call.raw_arguments,
                    }));
                }
            }
            ConversationItem::ToolResult {
                call_id, content, ..
            } => {
                let compact = reviewer_policy.compact_tool_result(content, &scope.capabilities);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id.0,
                    "output": serde_json::to_string(&compact)
                        .map_err(|_| RuntimeError::Invariant("tool result serialization failed"))?,
                }));
            }
        }
    }
    if input.is_empty() {
        input.push(response_message("user", "Begin the task."));
    }

    let mut body = json!({
        "model": profile.model.as_str(),
        "input": input,
        "tools": openai_response_tools(
            reviewer_policy,
            tool_registry,
            transcript,
            &scope.capabilities,
        )?,
        "parallel_tool_calls": false,
        "tool_choice": "auto",
        "max_output_tokens": profile.max_output_tokens,
    });
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions.join("\n\n"));
    }
    if !(profile.model.starts_with("gpt-5") || profile.model.starts_with('o')) {
        body["temperature"] = json!(profile.temperature.unwrap_or(0.0));
    }
    if let Some(top_p) = profile.top_p {
        body["top_p"] = json!(top_p);
    }
    if let Some(response_format) = &scope.response_format {
        body["text"] = json!({
            "format": responses_json_schema_text_format(response_format),
        });
    }
    Ok(body)
}

fn responses_json_schema_text_format(response_format: &ModelResponseFormat) -> Value {
    json!({
        "type": "json_schema",
        "name": response_format.name.as_str(),
        "strict": response_format.strict,
        "schema": response_format.schema.clone(),
    })
}

fn response_message(role: &str, content: &str) -> Value {
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{
            "type": content_type,
            "text": content,
        }],
    })
}

fn openai_response_tools(
    reviewer_policy: &ReviewerPolicy,
    tool_registry: &ToolRegistry,
    transcript: &[ConversationItem],
    capabilities: &CapabilitySet,
) -> RuntimeResult<Vec<Value>> {
    let tools =
        reviewer_policy.tool_schemas_for_transcript(tool_registry, transcript, capabilities);
    tools
        .into_iter()
        .map(response_tool_from_chat_tool)
        .collect::<RuntimeResult<Vec<_>>>()
}

fn response_tool_from_chat_tool(tool: Value) -> RuntimeResult<Value> {
    let function = tool
        .get("function")
        .and_then(Value::as_object)
        .ok_or(RuntimeError::Invariant("chat tool schema missing function"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or(RuntimeError::Invariant("chat tool schema missing name"))?;
    let description =
        function
            .get("description")
            .and_then(Value::as_str)
            .ok_or(RuntimeError::Invariant(
                "chat tool schema missing description",
            ))?;
    let parameters = function
        .get("parameters")
        .cloned()
        .ok_or(RuntimeError::Invariant(
            "chat tool schema missing parameters",
        ))?;
    Ok(json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters,
        "strict": true,
    }))
}

pub(crate) fn model_alias_for_tool(
    tool_registry: &ToolRegistry,
    tool_id: &ToolId,
) -> RuntimeResult<ToolId> {
    tool_registry
        .alias_table()?
        .alias_for(tool_id)
        .cloned()
        .ok_or(RuntimeError::Invariant("missing model alias for tool"))
}

fn parse_responses_response(
    response: ResponsesResponse,
    tool_registry: &ToolRegistry,
) -> RuntimeResult<ModelTurn> {
    let usage = response.usage.unwrap_or_default().into_token_usage();
    let mut seen = HashSet::new();
    let mut calls = Vec::new();
    let mut text = String::new();
    for (index, item) in response.output.into_iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str);
        match item_type {
            Some("function_call") => {
                let call_id =
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .ok_or(RuntimeError::Provider {
                            status: None,
                            retryable: false,
                        })?;
                if !seen.insert(call_id.to_string()) {
                    return Err(RuntimeError::Provider {
                        status: None,
                        retryable: false,
                    });
                }
                let alias = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(RuntimeError::Provider {
                        status: None,
                        retryable: false,
                    })
                    .and_then(ToolId::parse)?;
                let Some(name) = tool_registry.tool_id_for_model_alias(&alias) else {
                    return Err(RuntimeError::InvalidInput("unknown tool name".to_string()));
                };
                let raw_arguments = item.get("arguments").and_then(Value::as_str).ok_or(
                    RuntimeError::Provider {
                        status: None,
                        retryable: false,
                    },
                )?;
                calls.push(ModelToolCall {
                    call_id: ToolCallId(call_id.to_string()),
                    index,
                    name,
                    raw_arguments: raw_arguments.to_string(),
                });
            }
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("output_text" | "text")
                        ) {
                            if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                                text.push_str(part_text);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !calls.is_empty() {
        Ok(ModelTurn::ToolCalls { calls, usage })
    } else {
        Ok(ModelTurn::Text {
            content: response.output_text.unwrap_or(text),
            usage,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    output: Vec<Value>,
    output_text: Option<String>,
    usage: Option<ResponsesUsage>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ResponsesUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl ResponsesUsage {
    fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.unwrap_or(0),
            output_tokens: self.output_tokens.unwrap_or(0),
            total_tokens: self.total_tokens.unwrap_or(0),
            cached_input_tokens: 0,
        }
    }
}

#[cfg(test)]
mod tests;
