use std::io::Write;
use std::net::TcpListener;

use super::*;
use crate::reviewer_kernel::review_contract::{
    AgentBudget, ModelApiProtocol, ProviderKind, Role, ToolName,
};
use crate::reviewer_kernel::tool_engine::{CustomToolHandler, CustomToolOptions, CustomToolOutput};
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
        _context: crate::reviewer_kernel::tool_engine::CustomToolContext,
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

fn anthropic_profile() -> ModelProfileRefV1 {
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
        response_format: None,
        capabilities: CapabilitySet::review_read_only(),
        budget: AgentBudget {
            max_turns: 2,
            max_tool_calls: 2,
            max_prompt_tokens: 32_000,
            max_output_tokens: 1_024,
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::PlannedDefault,
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
        &anthropic_profile(),
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
        &anthropic_profile(),
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
fn request_body_omits_tool_choice_without_tools_and_uses_auto_when_tools_available() {
    let (registry, _, _) = aliased_registry();
    let policy = ReviewerPolicy::new();
    let transcript = vec![ConversationItem::User {
        content: "Answer in text.".to_string(),
    }];

    let mut text_only = test_scope();
    text_only.capabilities.tool_grants.clear();
    let body = anthropic_request_body(
        &anthropic_profile(),
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
        &anthropic_profile(),
        &policy,
        &registry,
        &test_scope(),
        &transcript,
        &MessageAssemblyCache::new(),
    )
    .expect("request body");
    assert_eq!(
        body["tool_choice"],
        json!({ "type": "auto", "disable_parallel_tool_use": true })
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
fn response_parses_tool_use_through_model_alias_lookup_and_rejects_duplicates() {
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
        anthropic_profile(),
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
        anthropic_profile(),
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
        anthropic_profile(),
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
