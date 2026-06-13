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

use crate::contracts::{ModelProfileRefV1, TokenUsage, ToolCallingMode};
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
        body["tool_choice"] = match profile.tool_calling_mode {
            ToolCallingMode::Required => {
                json!({ "type": "any", "disable_parallel_tool_use": true })
            }
            ToolCallingMode::Auto => {
                json!({ "type": "auto", "disable_parallel_tool_use": true })
            }
        };
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
mod tests {
    use std::io::Write;
    use std::net::TcpListener;

    use super::*;
    use crate::contracts::{AgentBudget, ModelApiProtocol, ProviderKind, Role, ToolName};
    use crate::runtime::tools::{CustomToolHandler, CustomToolOptions, CustomToolOutput};
    use crate::tests::support::{read_http_request, split_http_body};

    struct StaticCredentialResolver;

    impl CredentialResolver for StaticCredentialResolver {
        fn resolve_credential(&self, _credential_ref: &str) -> RuntimeResult<String> {
            Ok("test-anthropic-key".to_string())
        }
    }

    struct NoopCustomTool;

    #[async_trait::async_trait]
    impl CustomToolHandler for NoopCustomTool {
        async fn execute(
            &self,
            _context: crate::runtime::tools::CustomToolContext,
            _args: Value,
            _cancel: CancellationToken,
        ) -> RuntimeResult<CustomToolOutput> {
            Ok(CustomToolOutput::default())
        }
    }

    fn aliased_registry() -> (ToolRegistry, ToolId, ToolId) {
        let mut registry = ToolRegistry::review_defaults().expect("registry");
        let internal_tool = ToolId::parse("internal_swarm_tool").unwrap();
        let model_alias = ToolId::parse("provider_swarm_tool").unwrap();
        registry
            .register_custom_with_alias_and_effects(
                internal_tool.clone(),
                model_alias.clone(),
                "provider-visible alias test tool",
                json!({
                    "type": "object",
                    "properties": {
                        "value": { "type": "string" }
                    },
                    "required": ["value"],
                    "additionalProperties": false
                }),
                CustomToolOptions::default(),
                Arc::new(NoopCustomTool),
            )
            .unwrap();
        (registry, internal_tool, model_alias)
    }

    fn anthropic_profile(tool_calling_mode: ToolCallingMode) -> ModelProfileRefV1 {
        ModelProfileRefV1 {
            id: "claude".to_string(),
            provider_kind: ProviderKind::Anthropic,
            api_protocol: ModelApiProtocol::Messages,
            provider_profile_id: "anthropic".to_string(),
            credential_ref: "env:TEST_ANTHROPIC_KEY".to_string(),
            model: "claude-opus-4-8".to_string(),
            base_url: None,
            max_input_tokens: 32_000,
            max_output_tokens: 1_024,
            tool_calling_mode,
            temperature: None,
            top_p: None,
        }
    }

    fn test_scope() -> SessionScope {
        SessionScope {
            id: SessionId("anthropic-test".to_string()),
            role: Role::Generalist,
            objective: "anthropic client test".to_string(),
            instructions: Vec::new(),
            snapshot_id: None,
            model_profile_id: Some("claude".to_string()),
            capabilities: CapabilitySet::review_read_only(),
            budget: AgentBudget {
                max_turns: 2,
                max_tool_calls: 2,
                max_prompt_tokens: 32_000,
                max_output_tokens: 1_024,
                budget_source: crate::contracts::BudgetSource::PlannedDefault,
            },
        }
    }

    fn tool_result_envelope(ok: bool, call_id: &str) -> Box<ToolResultEnvelope> {
        Box::new(ToolResultEnvelope {
            ok,
            tool_call_id: ToolCallId(call_id.to_string()),
            tool_name: ToolId::from(ToolName::SearchText),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: SnapshotId("snapshot".to_string()),
            artifact_id: None,
            cache: CacheInfo {
                status: CacheStatus::NotCacheable,
                key_hash: None,
            },
            limits: LimitInfo::default(),
            data: Some(json!({ "matches": [] })),
            error: None,
        })
    }

    #[test]
    fn request_body_maps_transcript_to_messages_api_shape() {
        let (registry, internal_tool, model_alias) = aliased_registry();
        let policy = ReviewerPolicy::new();
        let scope = test_scope();
        let transcript = vec![
            ConversationItem::System {
                content: "Be terse.".to_string(),
            },
            ConversationItem::System {
                content: "Cite evidence.".to_string(),
            },
            ConversationItem::User {
                content: "Hello".to_string(),
            },
            ConversationItem::AssistantToolCalls {
                calls: vec![ModelToolCall {
                    call_id: ToolCallId("call-1".to_string()),
                    index: 0,
                    name: internal_tool.clone(),
                    raw_arguments: r#"{"value":"ok"}"#.to_string(),
                }],
            },
            ConversationItem::ToolResult {
                call_id: ToolCallId("call-1".to_string()),
                name: internal_tool,
                content: tool_result_envelope(false, "call-1"),
            },
            ConversationItem::AssistantText {
                content: "Done.".to_string(),
            },
        ];

        let body = anthropic_request_body(
            &anthropic_profile(ToolCallingMode::Auto),
            &policy,
            &registry,
            &scope,
            &transcript,
            &MessageAssemblyCache::new(),
        )
        .expect("request body");

        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(body["max_tokens"], 1_024);
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "Be terse.\n\nCite evidence.");
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello");
        assert_eq!(messages[1]["role"], "assistant");
        let tool_use = &messages[1]["content"][0];
        assert_eq!(tool_use["type"], "tool_use");
        assert_eq!(tool_use["id"], "call-1");
        assert_eq!(tool_use["name"], model_alias.as_str());
        assert_eq!(tool_use["input"], json!({ "value": "ok" }));
        assert_eq!(messages[2]["role"], "user");
        let tool_result = &messages[2]["content"][0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["tool_use_id"], "call-1");
        assert_eq!(tool_result["is_error"], true);
        assert!(tool_result["content"].is_string());
        assert_eq!(
            tool_result["cache_control"],
            json!({ "type": "ephemeral" }),
            "newest tool result must carry the moving cache breakpoint"
        );
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["content"], "Done.");

        let tools = body["tools"].as_array().expect("tools");
        assert!(!tools.is_empty());
        for tool in tools {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert!(tool["input_schema"].is_object());
            assert!(tool.get("function").is_none());
        }
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "auto", "disable_parallel_tool_use": true })
        );
    }

    #[test]
    fn cache_breakpoint_moves_to_newest_tool_result_only() {
        let (registry, internal_tool, _) = aliased_registry();
        let policy = ReviewerPolicy::new();
        let scope = test_scope();
        let transcript = vec![
            ConversationItem::User {
                content: "Start".to_string(),
            },
            ConversationItem::AssistantToolCalls {
                calls: vec![ModelToolCall {
                    call_id: ToolCallId("call-1".to_string()),
                    index: 0,
                    name: internal_tool.clone(),
                    raw_arguments: r#"{"value":"a"}"#.to_string(),
                }],
            },
            ConversationItem::ToolResult {
                call_id: ToolCallId("call-1".to_string()),
                name: internal_tool.clone(),
                content: tool_result_envelope(true, "call-1"),
            },
            ConversationItem::AssistantToolCalls {
                calls: vec![ModelToolCall {
                    call_id: ToolCallId("call-2".to_string()),
                    index: 0,
                    name: internal_tool.clone(),
                    raw_arguments: r#"{"value":"b"}"#.to_string(),
                }],
            },
            ConversationItem::ToolResult {
                call_id: ToolCallId("call-2".to_string()),
                name: internal_tool,
                content: tool_result_envelope(true, "call-2"),
            },
            ConversationItem::User {
                content: "Continue.".to_string(),
            },
        ];

        let body = anthropic_request_body(
            &anthropic_profile(ToolCallingMode::Auto),
            &policy,
            &registry,
            &scope,
            &transcript,
            &MessageAssemblyCache::new(),
        )
        .expect("request body");

        let messages = body["messages"].as_array().expect("messages");
        let older = &messages[2]["content"][0];
        assert_eq!(older["type"], "tool_result");
        assert!(
            older.get("cache_control").is_none(),
            "older tool results must keep byte-stable serialization"
        );
        let newest = &messages[4]["content"][0];
        assert_eq!(newest["type"], "tool_result");
        assert_eq!(newest["cache_control"], json!({ "type": "ephemeral" }));
        assert!(
            messages[5].get("content").expect("content").is_string(),
            "trailing user instruction stays in string form"
        );
        let tool_use = &messages[3]["content"][0];
        assert!(
            tool_use.get("cache_control").is_none(),
            "tool_use blocks never carry the breakpoint"
        );
    }

    #[test]
    fn request_body_omits_tools_for_text_only_scope_and_maps_required_mode() {
        let (registry, _, _) = aliased_registry();
        let policy = ReviewerPolicy::new();
        let transcript = vec![ConversationItem::User {
            content: "Answer in text.".to_string(),
        }];

        let mut text_only = test_scope();
        text_only.capabilities.tool_grants.clear();
        let body = anthropic_request_body(
            &anthropic_profile(ToolCallingMode::Required),
            &policy,
            &registry,
            &text_only,
            &transcript,
            &MessageAssemblyCache::new(),
        )
        .expect("request body");
        assert!(body.get("tools").is_none(), "no tools without grants");
        assert!(
            body.get("tool_choice").is_none(),
            "tool_choice requires tools"
        );

        let body = anthropic_request_body(
            &anthropic_profile(ToolCallingMode::Required),
            &policy,
            &registry,
            &test_scope(),
            &transcript,
            &MessageAssemblyCache::new(),
        )
        .expect("request body");
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "any", "disable_parallel_tool_use": true })
        );
    }

    #[test]
    fn response_parses_text_turn_with_usage_total() {
        let (registry, _, _) = aliased_registry();
        let turn = parse_anthropic_response(
            serde_json::from_value(json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "", "signature": "sig" },
                    { "type": "text", "text": "Hello " },
                    { "type": "text", "text": "world" }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 7, "output_tokens": 5 }
            }))
            .expect("response"),
            &registry,
        )
        .expect("turn");
        let ModelTurn::Text { content, usage } = turn else {
            panic!("expected text turn");
        };
        assert_eq!(content, "Hello world");
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.total_tokens, 12);
    }

    #[test]
    fn response_parses_tool_use_through_alias_table_and_rejects_duplicates() {
        let (registry, internal_tool, model_alias) = aliased_registry();
        let tool_use = json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": model_alias.as_str(),
            "input": { "value": "ok" }
        });
        let turn = parse_anthropic_response(
            serde_json::from_value(json!({
                "content": [tool_use],
                "usage": { "input_tokens": 3, "output_tokens": 2 }
            }))
            .expect("response"),
            &registry,
        )
        .expect("turn");
        let ModelTurn::ToolCalls { calls, .. } = turn else {
            panic!("expected tool call turn");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, internal_tool);
        assert_eq!(calls[0].call_id.0, "toolu_1");
        assert_eq!(calls[0].raw_arguments, r#"{"value":"ok"}"#);

        let duplicate = parse_anthropic_response(
            serde_json::from_value(json!({
                "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": model_alias.as_str(), "input": {} },
                    { "type": "tool_use", "id": "toolu_1", "name": model_alias.as_str(), "input": {} }
                ]
            }))
            .expect("response"),
            &registry,
        );
        assert!(duplicate.is_err(), "duplicate call ids are rejected");
    }

    #[test]
    fn live_loopback_streams_sse_events_into_a_tool_call_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("address");
        let (_registry, _, model_alias) = aliased_registry();
        let events = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_stream","name":model_alias.as_str(),"input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"value\""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":":\"ok\"}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        let body = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_http_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("response");
            request
        });

        let (registry, internal_tool, _) = aliased_registry();
        let client = AnthropicMessagesClient::from_profile(
            anthropic_profile(ToolCallingMode::Auto),
            format!("http://{address}/v1"),
            Arc::new(ModelLimiter::new(1)),
            Arc::new(registry),
            Arc::new(ReviewerPolicy::new()),
            Arc::new(StaticCredentialResolver),
        )
        .expect("client");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let turn = runtime
            .block_on(client.complete(
                &test_scope(),
                &[ConversationItem::User {
                    content: "Hello".to_string(),
                }],
                TurnId(0),
                CancellationToken::new(),
            ))
            .expect("completion");

        let request = server.join().expect("server thread");
        let (_, request_body) = split_http_body(&request);
        let request_json: Value = serde_json::from_slice(request_body).expect("request json");
        assert_eq!(request_json["stream"], true);

        let ModelTurn::ToolCalls { calls, usage } = turn else {
            panic!("expected tool call turn");
        };
        assert_eq!(calls[0].name, internal_tool);
        assert_eq!(calls[0].call_id.0, "toolu_stream");
        assert_eq!(calls[0].raw_arguments, r#"{"value":"ok"}"#);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.total_tokens, 17);
    }

    #[test]
    fn live_loopback_cancels_mid_stream_without_waiting_for_the_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = read_http_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                )
                .expect("first event");
            stream.flush().expect("flush");
            // Hold the connection open well past the cancellation point.
            std::thread::sleep(std::time::Duration::from_secs(5));
        });

        let (registry, _, _) = aliased_registry();
        let client = AnthropicMessagesClient::from_profile(
            anthropic_profile(ToolCallingMode::Auto),
            format!("http://{address}/v1"),
            Arc::new(ModelLimiter::new(1)),
            Arc::new(registry),
            Arc::new(ReviewerPolicy::new()),
            Arc::new(StaticCredentialResolver),
        )
        .expect("client");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let started = std::time::Instant::now();
        let result = runtime.block_on(async {
            let cancel = CancellationToken::new();
            let canceller = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                canceller.cancel();
            });
            client
                .complete(
                    &test_scope(),
                    &[ConversationItem::User {
                        content: "Hello".to_string(),
                    }],
                    TurnId(0),
                    cancel,
                )
                .await
        });
        assert!(
            matches!(result, Err(RuntimeError::Cancelled)),
            "cancellation interrupts the stream"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cancel returns promptly instead of waiting out the server"
        );
        drop(server);
    }

    #[test]
    fn live_loopback_round_trip_sends_required_headers_and_parses_tool_use() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("address");
        let (_registry, _, model_alias) = aliased_registry();
        let canned_response = json!({
            "id": "msg_loopback",
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_loopback",
                "name": model_alias.as_str(),
                "input": { "value": "from-server" }
            }],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 11, "output_tokens": 6 }
        })
        .to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_http_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                canned_response.len(),
                canned_response
            );
            stream.write_all(response.as_bytes()).expect("response");
            request
        });

        let (registry, internal_tool, _) = aliased_registry();
        let client = AnthropicMessagesClient::from_profile(
            anthropic_profile(ToolCallingMode::Auto),
            format!("http://{address}/v1"),
            Arc::new(ModelLimiter::new(1)),
            Arc::new(registry),
            Arc::new(ReviewerPolicy::new()),
            Arc::new(StaticCredentialResolver),
        )
        .expect("client");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let turn = runtime
            .block_on(client.complete(
                &test_scope(),
                &[ConversationItem::User {
                    content: "Hello".to_string(),
                }],
                TurnId(0),
                CancellationToken::new(),
            ))
            .expect("completion");

        let request = server.join().expect("server thread");
        let (headers, request_body) = split_http_body(&request);
        assert!(headers.starts_with("POST /v1/messages HTTP/1.1"));
        let headers_lower = headers.to_ascii_lowercase();
        assert!(headers_lower.contains("x-api-key: test-anthropic-key"));
        assert!(headers_lower.contains("anthropic-version: 2023-06-01"));
        let request_json: Value = serde_json::from_slice(request_body).expect("request json");
        assert_eq!(request_json["model"], "claude-opus-4-8");
        assert_eq!(request_json["messages"][0]["content"], "Hello");

        let ModelTurn::ToolCalls { calls, usage } = turn else {
            panic!("expected tool call turn");
        };
        assert_eq!(calls[0].name, internal_tool);
        assert_eq!(calls[0].call_id.0, "toolu_loopback");
        assert_eq!(usage.total_tokens, 17);
    }
}
