//! Anthropic Messages API client (`POST {base}/messages`). Raw HTTP, same
//! shape as the OpenAI-compatible clients in `model.rs`: one completion per
//! call, limiter permit held for the request, retry classification via the
//! shared provider error mapping (429 and 5xx — including 529 overloaded —
//! are retryable).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::contracts::{ModelProfileRefV1, TokenUsage};
use crate::runtime::assembly::MessageAssemblyCache;
use crate::runtime::contracts::*;
use crate::runtime::model::{
    model_alias_for_tool, provider_error_message, provider_http_error, ConcurrentModelClient,
    CredentialResolver, ModelLimiter,
};
use crate::runtime::model_sse::{next_streaming_data, SseStream, STREAM_REQUEST_TIMEOUT};
use crate::runtime::policy::ReviewerPolicy;
use crate::runtime::tools::ToolRegistry;

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub(crate) fn anthropic_default_base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string())
}

#[derive(Debug)]
pub(crate) struct AnthropicMessagesClient {
    http: reqwest::Client,
    profile: ModelProfileRefV1,
    api_key: String,
    base_url: String,
    limiter: Arc<ModelLimiter>,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    assembly: MessageAssemblyCache,
}

impl AnthropicMessagesClient {
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
            assembly: MessageAssemblyCache::new(),
        })
    }
}

#[async_trait]
impl ConcurrentModelClient for AnthropicMessagesClient {
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
                "anthropic",
                &self.profile.id,
                &self.profile.credential_ref,
                &scope.id,
            )
            .await?;
        let mut body = anthropic_request_body(
            &self.profile,
            &self.reviewer_policy,
            &self.tool_registry,
            scope,
            transcript,
            &self.assembly,
        )?;
        body["stream"] = json!(true);
        let response = self
            .http
            .post(format!("{}/messages", self.base_url))
            .timeout(STREAM_REQUEST_TIMEOUT)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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
        let decoded = anthropic_message_from_stream(response, &cancel).await?;
        parse_anthropic_response(decoded, &self.tool_registry)
    }
}

struct StreamingContentBlock {
    start: Value,
    text: String,
    input_json: String,
}

impl StreamingContentBlock {
    fn into_value(self) -> Value {
        let mut block = self.start;
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let opening = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                block["text"] = json!(format!("{opening}{}", self.text));
            }
            Some("tool_use") => {
                if !self.input_json.is_empty() {
                    block["input"] =
                        serde_json::from_str(&self.input_json).unwrap_or_else(|_| json!({}));
                }
            }
            _ => {}
        }
        block
    }
}

/// Accumulates Messages API stream events into the non-streaming response
/// shape. Providers that ignored `stream: true` and answered with one plain
/// JSON body are detected (no SSE events seen) and parsed directly.
async fn anthropic_message_from_stream(
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> RuntimeResult<AnthropicMessageResponse> {
    let mut sse = SseStream::new(response);
    let mut blocks: std::collections::BTreeMap<u64, StreamingContentBlock> =
        std::collections::BTreeMap::new();
    let mut input_tokens: Option<u64> = None;
    let mut output_tokens: Option<u64> = None;
    let mut cache_read_input_tokens: Option<u64> = None;
    loop {
        let Some(data) = next_streaming_data(&mut sse, cancel).await? else {
            break;
        };
        let event: Value = serde_json::from_str(&data).map_err(|_| RuntimeError::Provider {
            status: None,
            retryable: false,
        })?;
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let usage = &event["message"]["usage"];
                input_tokens = usage["input_tokens"].as_u64().or(input_tokens);
                output_tokens = usage["output_tokens"].as_u64().or(output_tokens);
                cache_read_input_tokens = usage["cache_read_input_tokens"]
                    .as_u64()
                    .or(cache_read_input_tokens);
            }
            Some("content_block_start") => {
                blocks.insert(
                    index,
                    StreamingContentBlock {
                        start: event["content_block"].clone(),
                        text: String::new(),
                        input_json: String::new(),
                    },
                );
            }
            Some("content_block_delta") => {
                let Some(block) = blocks.get_mut(&index) else {
                    continue;
                };
                let delta = &event["delta"];
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            block.text.push_str(text);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(json_text) = delta.get("partial_json").and_then(Value::as_str) {
                            block.input_json.push_str(json_text);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                output_tokens = event["usage"]["output_tokens"].as_u64().or(output_tokens);
            }
            Some("error") => {
                let error = &event["error"];
                let error_type = error.get("type").and_then(Value::as_str).unwrap_or("");
                let retryable = matches!(
                    error_type,
                    "overloaded_error" | "rate_limit_error" | "api_error"
                );
                return Err(RuntimeError::ProviderMessage {
                    status: None,
                    retryable,
                    message: provider_error_message(error.to_string()),
                });
            }
            // ping, content_block_stop, message_stop carry no turn payload.
            _ => {}
        }
    }
    if !sse.yielded_events() {
        return serde_json::from_slice(&sse.into_raw_body()).map_err(|_| RuntimeError::Provider {
            status: None,
            retryable: false,
        });
    }
    Ok(AnthropicMessageResponse {
        content: blocks
            .into_values()
            .map(StreamingContentBlock::into_value)
            .collect(),
        usage: Some(AnthropicUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
        }),
    })
}

/// System turns become the top-level `system` string; assistant tool calls
/// become `tool_use` content blocks and tool results become `tool_result`
/// blocks in user turns, which is the Messages API's transcript shape.
/// Marks the newest tool_result block with a cache breakpoint. Tool results
/// are appended once and serialized identically on every later turn, so the
/// prefix up to this block stays byte-stable and the provider extends the
/// cache across turns instead of recomputing the whole transcript. String
/// messages are left alone: converting only the latest one to block form
/// would change its serialization on the next turn and break prefix matching.
fn mark_latest_tool_result_cache_breakpoint(messages: &mut [Value]) {
    for message in messages.iter_mut().rev() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if let Some(block) = blocks
            .iter_mut()
            .rev()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            block["cache_control"] = json!({ "type": "ephemeral" });
            return;
        }
    }
}

fn anthropic_message_for_item(
    reviewer_policy: &ReviewerPolicy,
    tool_registry: &ToolRegistry,
    scope: &SessionScope,
    item: &ConversationItem,
) -> RuntimeResult<Option<Value>> {
    Ok(match item {
        // System items render into the top-level `system` field, not the
        // message list; the caller collects them separately.
        ConversationItem::System { .. } => None,
        ConversationItem::User { content } => Some(json!({ "role": "user", "content": content })),
        ConversationItem::AssistantText { content } => {
            Some(json!({ "role": "assistant", "content": content }))
        }
        ConversationItem::AssistantToolCalls { calls } => {
            let blocks = calls
                .iter()
                .map(|call| {
                    let name = model_alias_for_tool(tool_registry, &call.name)?;
                    // tool_use input must be a JSON object; raw_arguments
                    // originated from our own parse so this round-trips.
                    let input = serde_json::from_str::<Value>(&call.raw_arguments)
                        .unwrap_or_else(|_| json!({}));
                    Ok(json!({
                        "type": "tool_use",
                        "id": call.call_id.0,
                        "name": name.as_str(),
                        "input": input,
                    }))
                })
                .collect::<RuntimeResult<Vec<_>>>()?;
            Some(json!({ "role": "assistant", "content": blocks }))
        }
        ConversationItem::ToolResult {
            call_id, content, ..
        } => {
            let compact = reviewer_policy.compact_tool_result(content, &scope.capabilities);
            Some(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call_id.0,
                    "content": serde_json::to_string(&compact).map_err(|_| {
                        RuntimeError::Invariant("tool result serialization failed")
                    })?,
                    "is_error": !content.ok,
                }],
            }))
        }
    })
}

fn anthropic_request_body(
    profile: &ModelProfileRefV1,
    reviewer_policy: &ReviewerPolicy,
    tool_registry: &ToolRegistry,
    scope: &SessionScope,
    transcript: &[ConversationItem],
    assembly: &MessageAssemblyCache,
) -> RuntimeResult<Value> {
    let system: Vec<&str> = transcript
        .iter()
        .filter_map(|item| match item {
            ConversationItem::System { content } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    let mut messages = assembly.assemble(&scope.id.0, &scope.capabilities, transcript, |item| {
        anthropic_message_for_item(reviewer_policy, tool_registry, scope, item)
    })?;

    // The breakpoint is applied to this call's clone, never to the cached
    // rendering, so the cached prefix stays byte-stable across turns.
    mark_latest_tool_result_cache_breakpoint(&mut messages);
    let tools = anthropic_tools(
        reviewer_policy,
        tool_registry,
        transcript,
        &scope.capabilities,
    )?;
    let mut body = json!({
        "model": profile.model.as_str(),
        "max_tokens": profile.max_output_tokens,
        "messages": messages,
    });
    if !system.is_empty() {
        // Block form so the stable system prompt can carry a cache breakpoint.
        body["system"] = json!([{
            "type": "text",
            "text": system.join("\n\n"),
            "cache_control": { "type": "ephemeral" },
        }]);
    }
    // The Messages API rejects tool_choice without tools, so the text-only
    // final turn (capabilities cleared) omits both.
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!({ "type": "auto", "disable_parallel_tool_use": true });
    }
    if let Some(temperature) = profile.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = profile.top_p {
        body["top_p"] = json!(top_p);
    }
    Ok(body)
}

fn anthropic_tools(
    reviewer_policy: &ReviewerPolicy,
    tool_registry: &ToolRegistry,
    transcript: &[ConversationItem],
    capabilities: &CapabilitySet,
) -> RuntimeResult<Vec<Value>> {
    reviewer_policy
        .tool_schemas_for_transcript(tool_registry, transcript, capabilities)
        .into_iter()
        .map(anthropic_tool_from_chat_tool)
        .collect()
}

fn anthropic_tool_from_chat_tool(tool: Value) -> RuntimeResult<Value> {
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
        "name": name,
        "description": description,
        "input_schema": parameters,
    }))
}

fn parse_anthropic_response(
    response: AnthropicMessageResponse,
    tool_registry: &ToolRegistry,
) -> RuntimeResult<ModelTurn> {
    let usage = response.usage.unwrap_or_default().into_token_usage();
    let mut seen = HashSet::new();
    let mut calls = Vec::new();
    let mut text = String::new();
    for (index, block) in response.content.into_iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let call_id =
                    block
                        .get("id")
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
                let alias = block
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
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                calls.push(ModelToolCall {
                    call_id: ToolCallId(call_id.to_string()),
                    index,
                    name,
                    raw_arguments: input.to_string(),
                });
            }
            Some("text") => {
                if let Some(block_text) = block.get("text").and_then(Value::as_str) {
                    text.push_str(block_text);
                }
            }
            // thinking / redacted_thinking and future block types are not
            // part of the turn contract.
            _ => {}
        }
    }
    if !calls.is_empty() {
        Ok(ModelTurn::ToolCalls { calls, usage })
    } else {
        Ok(ModelTurn::Text {
            content: text,
            usage,
        })
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageResponse {
    #[serde(default)]
    content: Vec<Value>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

impl AnthropicUsage {
    fn into_token_usage(self) -> TokenUsage {
        // Anthropic reports cache reads separately from input_tokens; fold
        // them into input/total so cross-provider accounting stays uniform,
        // and surface the cached share for cost visibility.
        let cached_input_tokens = self.cache_read_input_tokens.unwrap_or(0);
        let input_tokens = self.input_tokens.unwrap_or(0) + cached_input_tokens;
        let output_tokens = self.output_tokens.unwrap_or(0);
        TokenUsage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            cached_input_tokens,
        }
    }
}

#[cfg(test)]
mod tests;
