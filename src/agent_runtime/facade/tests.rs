use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::*;
use crate::agent_runtime::{
    AgentOutput, ExecutionError, ExecutionErrorCode, MessageRole, ModelProvider,
    ModelProviderError, ModelRequest, ModelStop, ModelToolCall, ModelTurn, RunId, RunResult,
    TerminalAgentStatus, TerminalRunStatus, Usage,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Deserialize, PartialEq)]
struct SearchInput {
    query: String,
    limit: u32,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Answer {
    answer: String,
}

#[test]
fn tool_validation_and_duplicate_detection_match_python_messages() {
    let invalid_name = Tool::new("not.valid", "", json!({}), |_| async { Ok(Value::Null) })
        .err()
        .expect("invalid name");
    assert_eq!(invalid_name.code(), ErrorCode::InvalidInput);
    assert_eq!(
        invalid_name.message(),
        "tool.name must match [a-zA-Z0-9_-]{1,64}"
    );
    assert_eq!(
        invalid_name.details(),
        Some(&json!({ "path": "tool.name" }))
    );

    let tool = value_tool("lookup", "ok");
    let duplicate = LoopbackToolServer::new([tool.clone(), tool])
        .err()
        .expect("duplicate name");
    assert_eq!(duplicate.message(), "tools must have unique function names");

    let duplicate = Agent::new("use tools", "gpt-test")
        .api_key("test")
        .tools([value_tool("lookup", "one"), value_tool("lookup", "two")])
        .build()
        .err()
        .expect("duplicate name");
    assert_eq!(duplicate.message(), "tools must have unique function names");

    let hijacked = Agent::new("use tools", "gpt-test")
        .api_key("test")
        .can_spawn(true)
        .tool(value_tool("agent_spawn", "ok"))
        .build()
        .err()
        .expect("model-visible collision with builtin agent.spawn");
    assert_eq!(hijacked.message(), "tools must have unique function names");
    let allowed = Agent::new("use tools", "gpt-test")
        .api_key("test")
        .tool(value_tool("agent_spawn", "ok"))
        .build();
    assert!(
        allowed.is_ok(),
        "agent_spawn is only reserved while can_spawn grants the builtin"
    );
}

#[test]
fn facade_option_validation_matches_python_contract() {
    assert_invalid(
        Agent::builder().build(),
        "instructions is required",
        "instructions",
    );
    assert_invalid(
        Agent::builder().instructions("do it").build(),
        "model is required",
        "model",
    );
    assert_invalid(
        Agent::new("do it", "gpt-test")
            .api_key("test")
            .transport("grpc")
            .build(),
        "transport must be 'local' or 'http'",
        "transport",
    );
    assert_invalid(
        Agent::new("do it", "gpt-test")
            .api_key("test")
            .transport("http")
            .build(),
        "base_url is required for HTTP transport",
        "base_url",
    );
    let http_tools = Agent::new("do it", "gpt-test")
        .api_key("test")
        .transport("http")
        .base_url("https://muzen.example")
        .tool(
            Tool::new("lookup", "Look up a value.", search_schema(), |_| async {
                Ok(Value::String("ok".to_owned()))
            })
            .expect("tool"),
        )
        .build()
        .expect("HTTP facade tools are client-executed");
    assert!(http_tools.tool_server.is_none());
    assert_eq!(http_tools.client_tools.len(), 1);
    assert!(http_tools
        .spec_template
        .tool_providers
        .iter()
        .any(|provider| {
            matches!(
                provider,
                ToolProvider::Client { id, timeout_ms: None }
                    if id.as_str() == LOCAL_TOOLS_PROVIDER_ID
            )
        }));
    let grant = http_tools
        .spec_template
        .agent
        .tools
        .iter()
        .find(|grant| grant.tool == "lookup")
        .expect("client tool grant");
    assert_eq!(grant.description.as_deref(), Some("Look up a value."));
    assert_eq!(grant.input_schema.as_ref(), Some(&search_schema()));
    let authed = Agent::new("do it", "gpt-test")
        .api_key("test")
        .transport("http")
        .base_url("https://muzen.example")
        .bearer_token("service-token")
        .build()
        .expect("bearer token is a connection option");
    assert_eq!(authed.bearer_token.as_deref(), Some("service-token"));
    assert_invalid(
        Agent::new("do it", "gpt-test")
            .api_key("test")
            .max_output_tokens(0)
            .build(),
        "max_output_tokens must be positive",
        "max_output_tokens",
    );
    assert_invalid(
        Agent::new("   ", "gpt-test").api_key("test").build(),
        "instructions text blocks must not be empty",
        "instructions",
    );
    assert_invalid(
        Agent::builder()
            .instructions(Vec::<ContentBlock>::new())
            .model("gpt-test")
            .api_key("test")
            .build(),
        "instructions must contain at least one content block",
        "instructions",
    );

    let spec = sample_spec();
    assert_invalid(
        Agent::builder()
            .spec(spec)
            .tools(Vec::<Tool>::new())
            .build(),
        "spec cannot be combined with facade authoring options",
        "spec",
    );
}

#[test]
fn missing_provider_environment_key_is_invalid_input() {
    let _guard = ENV_LOCK.lock();
    let previous = std::env::var_os("ANTHROPIC_API_KEY");
    std::env::remove_var("ANTHROPIC_API_KEY");
    let result = Agent::new("do it", "claude-test").build();
    if let Some(previous) = previous {
        std::env::set_var("ANTHROPIC_API_KEY", previous);
    }
    assert_invalid(
        result,
        "api_key is required when ANTHROPIC_API_KEY is not set",
        "api_key",
    );
}

#[test]
fn model_string_mapping_and_defaults_match_python() {
    for (model, provider, protocol, name) in [
        (
            "claude-sonnet-5",
            ModelProviderKind::Anthropic,
            ModelProtocol::Messages,
            "claude-sonnet-5",
        ),
        (
            "gpt-4o-mini",
            ModelProviderKind::OpenaiCompatible,
            ModelProtocol::ChatCompletions,
            "gpt-4o-mini",
        ),
        (
            "anthropic:not-claude",
            ModelProviderKind::Anthropic,
            ModelProtocol::Messages,
            "not-claude",
        ),
        (
            "openai:claude-named",
            ModelProviderKind::OpenaiCompatible,
            ModelProtocol::ChatCompletions,
            "claude-named",
        ),
    ] {
        let agent = Agent::new("review carefully", model)
            .api_key("test")
            .can_spawn(true)
            .can_message(true)
            .max_total_tokens(500)
            .deadline_ms(1_000)
            .build()
            .expect("agent");
        let profile = &agent.spec_template.models[0];
        assert_eq!(profile.provider, provider);
        assert_eq!(profile.protocol, protocol);
        assert_eq!(profile.model, name);
        assert_eq!(profile.max_input_tokens.get(), 128_000);
        assert_eq!(profile.max_output_tokens.get(), 4_096);
        assert_eq!(
            agent.spec_template.agent.instructions,
            vec![ContentBlock::Text {
                text: "review carefully".to_owned()
            }]
        );
        assert_eq!(
            agent
                .spec_template
                .agent
                .tools
                .iter()
                .map(|grant| (grant.tool.as_str(), grant.effects.as_slice()))
                .collect::<Vec<_>>(),
            vec![
                ("agent.spawn", &[ToolEffect::AgentSpawn][..]),
                ("agent.message", &[ToolEffect::AgentMessage][..]),
            ]
        );
        assert_eq!(agent.default_limits.max_active_agents.get(), 4);
        assert_eq!(agent.default_limits.max_agents.get(), 16);
        assert_eq!(agent.default_limits.max_depth, 3);
        assert_eq!(agent.default_limits.max_input_bytes.get(), 1_048_576);
        assert_eq!(
            agent.default_limits.max_total_tokens.map(NonZeroU64::get),
            Some(500)
        );
        assert_eq!(
            agent.default_limits.deadline_ms.map(NonZeroU64::get),
            Some(1_000)
        );
    }
}

#[tokio::test]
async fn loopback_server_wire_conformance() {
    let details = Tool::typed::<SearchInput>(
        "details",
        "Search the product docs.",
        search_schema(),
        |input: SearchInput| async move {
            Ok(json!({
                "query": input.query,
                "count": input.limit
            }))
        },
    )
    .expect("tool");
    let fail = Tool::new(
        "fail",
        "Fail deliberately.",
        json!({ "type": "object" }),
        |_| async { Err(MuzenError::internal("failed: boom")) },
    )
    .expect("tool");
    let server = LoopbackToolServer::new([details, fail]).expect("server");
    let url = server.start().expect("start");
    assert_eq!(server.start().expect("idempotent start"), url);
    let client = reqwest::Client::new();

    let initialized = client
        .post(&url)
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .send()
        .await
        .expect("initialize");
    assert_eq!(initialized.status(), StatusCode::OK);
    assert_eq!(
        initialized
            .headers()
            .get("mcp-session-id")
            .expect("session header"),
        "muzen-python-tools"
    );
    let initialized: Value = initialized.json().await.expect("initialize JSON");
    assert_eq!(initialized["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(
        initialized["result"]["capabilities"],
        json!({ "tools": {} })
    );

    let notification = client
        .post(&url)
        .json(&json!({
            "jsonrpc": "2.0", "method": "notifications/initialized", "params": {}
        }))
        .send()
        .await
        .expect("notification");
    assert_eq!(notification.status(), StatusCode::ACCEPTED);
    assert!(notification.bytes().await.expect("empty body").is_empty());

    let listed = post_json(
        &client,
        &url,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(
        listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect::<Vec<_>>(),
        vec!["details", "fail"]
    );
    assert_eq!(listed["result"]["tools"][0]["inputSchema"], search_schema());

    let called = post_json(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "details", "arguments": { "query": "retry", "limit": 1 } }
        }),
    )
    .await;
    assert_eq!(called["result"]["isError"], false);
    assert_eq!(
        called["result"]["structuredContent"],
        json!({ "query": "retry", "count": 1 })
    );
    assert_eq!(
        called["result"]["content"][0]["text"],
        "{\"count\": 1, \"query\": \"retry\"}"
    );

    let failed = post_json(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "fail", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(failed["result"]["isError"], true);
    assert_eq!(failed["result"]["content"][0]["text"], "failed: boom");

    let invalid_arguments = post_json(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "details", "arguments": [] }
        }),
    )
    .await;
    assert_eq!(invalid_arguments["error"]["code"], -32602);
    assert_eq!(
        invalid_arguments["error"]["message"],
        "arguments must be an object"
    );

    let typed_error = post_json(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "details", "arguments": { "query": "retry" } }
        }),
    )
    .await;
    assert_eq!(typed_error["result"]["isError"], true);
    assert!(typed_error["result"]["content"][0]["text"]
        .as_str()
        .expect("error text")
        .contains("invalid tool arguments"));

    let unknown = post_json(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "missing", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(
        unknown["error"],
        json!({ "code": -32602, "message": "unknown tool" })
    );

    let method = post_json(
        &client,
        &url,
        json!({ "jsonrpc": "2.0", "id": 8, "method": "missing" }),
    )
    .await;
    assert_eq!(method["error"]["code"], -32601);

    let parse = client
        .post(&url)
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .expect("parse response")
        .json::<Value>()
        .await
        .expect("parse JSON");
    assert_eq!(
        parse["error"],
        json!({ "code": -32700, "message": "invalid JSON" })
    );

    server.close().await;
    assert!(client.post(&url).body("{}").send().await.is_err());
}

#[tokio::test]
async fn scripted_model_calls_rust_tool_and_structured_output_deserializes() {
    let provider = ScriptedProvider::new([
        ModelTurn {
            content: Vec::new(),
            tool_calls: vec![ModelToolCall {
                id: "search-1".to_owned(),
                provider: ToolProviderId::new(LOCAL_TOOLS_PROVIDER_ID).expect("provider"),
                name: "search".to_owned(),
                arguments: json!({ "query": "retry policy", "limit": 3 }),
            }],
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
                tool_calls: 0,
            },
            stop: ModelStop::ToolUse,
        },
        ModelTurn {
            content: vec![ContentBlock::Text {
                text: "{\"answer\":\"retry three times\"}".to_owned(),
            }],
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                tool_calls: 0,
            },
            stop: ModelStop::EndTurn,
        },
    ]);
    let client =
        Muzen::local(LocalRuntimeConfig::memory(provider.clone()).with_loopback_http(true))
            .await
            .expect("local runtime");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&calls);
    let search = Tool::typed::<SearchInput>(
        "search",
        "Search the product docs.",
        search_schema(),
        move |input: SearchInput| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().push((input.query, input.limit));
                Ok(Value::String("retry three times".to_owned()))
            }
        },
    )
    .expect("search tool");
    let mut agent = Agent::new("Answer using tools.", "gpt-test")
        .client(client)
        .api_key("test")
        .tool(search)
        .output_schema(json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false
        }))
        .build()
        .expect("agent");

    let result = agent.run("find the retry policy").await.expect("run");
    assert_eq!(result.status, TerminalAgentStatus::Completed);
    assert_eq!(result.text, "{\"answer\":\"retry three times\"}");
    assert_eq!(
        result.output::<Answer>().expect("typed output"),
        Answer {
            answer: "retry three times".to_owned()
        }
    );
    assert_eq!(calls.lock().as_slice(), &[("retry policy".to_owned(), 3)]);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].mcp_tools.len(), 1);
    assert_eq!(requests[0].mcp_tools[0].name, "search");
    assert_eq!(requests[0].mcp_tools[0].input_schema, search_schema());
    let tool_message = requests[1]
        .transcript
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("tool result in transcript");
    assert!(serde_json::to_string(&tool_message.content)
        .expect("serialize transcript")
        .contains("retry three times"));
    assert_eq!(result.raw.usage.tool_calls, 1);

    result.into_ok().expect("successful result");
    agent.close().await.expect("close");
    agent.close().await.expect("idempotent close");
}

#[tokio::test]
async fn conversation_reuses_session_and_one_shot_does_not() {
    let provider =
        ScriptedProvider::new([text_turn("first"), text_turn("second"), text_turn("fresh")]);
    let client = Muzen::local(LocalRuntimeConfig::memory(provider))
        .await
        .expect("local runtime");
    let mut agent = Agent::new("Answer.", "gpt-test")
        .client(client)
        .api_key("test")
        .build()
        .expect("agent");
    let first_session;
    {
        let conversation = agent.session().await.expect("conversation");
        let first = conversation.run("one").await.expect("first");
        let second = conversation.run("two").await.expect("second");
        assert_eq!(first.session_id, second.session_id);
        first_session = first.session_id;
        conversation.close().await;
    }
    let fresh = agent.run("fresh").await.expect("fresh");
    assert_ne!(fresh.session_id, first_session);
    agent.close().await.expect("close");
}

#[tokio::test]
async fn http_tool_pump_posts_errors_and_swallows_benign_answer_races() {
    let events = [
        pump_event(
            1,
            "tool.requested",
            json!({
                "callId": "failed-call",
                "provider": LOCAL_TOOLS_PROVIDER_ID,
                "tool": "fail",
                "arguments": { "value": 1 },
                "timeoutMs": 120_000
            }),
        ),
        pump_event(
            2,
            "tool.requested",
            json!({
                "callId": "unknown-call",
                "provider": LOCAL_TOOLS_PROVIDER_ID,
                "tool": "missing",
                "arguments": {},
                "timeoutMs": 120_000
            }),
        ),
        pump_event(3, "run.completed", json!({})),
    ]
    .concat();
    let server = FakePumpServer::start(events, [StatusCode::CONFLICT, StatusCode::NOT_FOUND]).await;
    let client =
        Muzen::http(server.base_url(), HttpTransportOptions::default()).expect("HTTP client");
    let run_id = RunId::new("pump-run").expect("run id");
    let run = client.get_run(&run_id).await.expect("fake run");
    let fail = Tool::new("fail", "", json!({ "type": "object" }), |_| async {
        Err(MuzenError::internal("tool exploded"))
    })
    .expect("failing tool");

    result::pump_run_tools(client, run, Arc::new(vec![fail]))
        .await
        .expect("conflict and not_found are benign");

    assert_eq!(
        server.answers(),
        vec![
            (
                "failed-call".to_owned(),
                json!({ "error": { "message": "tool exploded", "retryable": false } }),
            ),
            (
                "unknown-call".to_owned(),
                json!({
                    "error": {
                        "message": "unknown local tool: missing",
                        "retryable": false
                    }
                }),
            ),
        ]
    );
}

#[tokio::test]
async fn http_tool_pump_recovers_after_two_failures_at_the_same_cursor() {
    let events = [
        pump_event(
            1,
            "tool.requested",
            json!({
                "callId": "recovered-call",
                "provider": LOCAL_TOOLS_PROVIDER_ID,
                "tool": "recover",
                "arguments": { "value": 1 },
                "timeoutMs": 120_000
            }),
        ),
        pump_event(2, "run.completed", json!({})),
    ]
    .concat();
    let server = FakePumpServer::start_with_event_failures(events, [], 2).await;
    let client =
        Muzen::http(server.base_url(), HttpTransportOptions::default()).expect("HTTP client");
    let run_id = RunId::new("pump-run").expect("run id");
    let run = client.get_run(&run_id).await.expect("fake run");
    let tool = value_tool("recover", "recovered");

    result::pump_run_tools(client, run, Arc::new(vec![tool]))
        .await
        .expect("pump should recover after two connect failures");

    assert_eq!(server.event_requests(), 3);
    assert_eq!(
        server.answers(),
        vec![(
            "recovered-call".to_owned(),
            json!({ "result": "recovered" })
        )]
    );
}

#[tokio::test]
async fn http_tool_pump_bounds_permanent_retryable_failures() {
    let server = FakePumpServer::start_with_permanent_event_failure(String::new(), false).await;
    let client =
        Muzen::http(server.base_url(), HttpTransportOptions::default()).expect("HTTP client");
    let run_id = RunId::new("pump-run").expect("run id");
    let run = client.get_run(&run_id).await.expect("fake run");

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        result::pump_run_tools(client, run, Arc::new(vec![value_tool("unused", "unused")])),
    )
    .await
    .expect("pump retries should be bounded")
    .expect_err("permanent stream failure should surface");

    assert_eq!(server.event_requests(), 6);
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.message(), "fake event stream unavailable");
    assert!(error.retryable());
}

#[tokio::test]
async fn run_in_session_surfaces_pump_failure_and_cancels_run() {
    let server = FakePumpServer::start_with_permanent_event_failure(String::new(), true).await;
    let client =
        Muzen::http(server.base_url(), HttpTransportOptions::default()).expect("HTTP client");
    let session_id = crate::agent_runtime::SessionId::new("pump-session").expect("session id");
    let session = client.get_session(&session_id).await.expect("fake session");
    let input = AgentInput {
        content: vec![crate::agent_runtime::ContentBlock::Text {
            text: "run".to_owned(),
        }],
    };
    let limits = RunLimits {
        max_active_agents: std::num::NonZeroU32::new(1).expect("non-zero"),
        max_agents: std::num::NonZeroU32::new(1).expect("non-zero"),
        max_depth: 0,
        max_input_bytes: NonZeroU64::new(1024).expect("non-zero"),
        max_total_tokens: None,
        max_total_tool_calls: None,
        deadline_ms: None,
    };

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        result::run_in_session(
            &session,
            &client,
            Arc::new(vec![value_tool("unused", "unused")]),
            input,
            limits,
            false,
        ),
    )
    .await
    .expect("facade should fail promptly")
    .expect_err("pump transport failure should fail the facade run");

    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.message(), "fake event stream unavailable");
    assert_eq!(server.event_requests(), 6);
    assert_eq!(server.cancel_requests(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_facade_tool_round_trips_through_real_service() {
    let Some(binary) = discover_agent_service_binary() else {
        eprintln!(
            "skipping real-service facade e2e: muzen-agent-service is missing; set \
             MUZEN_AGENT_SERVICE_BIN or build target/release/muzen-agent-service"
        );
        return;
    };
    let model = GoldModelServer::start().await;
    let service_address = reserve_loopback_address();
    let _service = ServiceProcess::start(&binary, service_address).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&calls);
    let echo = Tool::new(
        "echo",
        "Echo the supplied value.",
        json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        move |arguments| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().push(arguments.clone());
                Ok(json!({ "echoed": arguments["value"] }))
            }
        },
    )
    .expect("echo tool");
    let mut agent = Agent::new("Use the echo tool once.", "gpt-test")
        .transport("http")
        .base_url(format!("http://{service_address}"))
        .model_base_url(model.base_url())
        .api_key("test")
        .tool(echo)
        .build()
        .expect("HTTP tool agent");

    let result = agent
        .run("echo the scripted value")
        .await
        .expect("agent run");
    assert_eq!(calls.lock().as_slice(), &[json!({ "value": "from-model" })]);
    assert!(
        result.text.contains(r#"{"echoed":"from-model"}"#),
        "final model answer did not contain the raw JSON tool result: {}",
        result.text
    );
    assert_eq!(model.request_count(), 2);
    agent.close().await.expect("close agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_facade_pumps_spawned_child_client_tool_through_real_service() {
    let Some(binary) = discover_agent_service_binary() else {
        eprintln!(
            "skipping real-service facade e2e: muzen-agent-service is missing; set \
             MUZEN_AGENT_SERVICE_BIN or build target/release/muzen-agent-service"
        );
        return;
    };
    let model = SwarmModelServer::start().await;
    let service_address = reserve_loopback_address();
    let _service = ServiceProcess::start(&binary, service_address).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&calls);
    let echo = Tool::new(
        "echo",
        "Echo the supplied value.",
        echo_schema(),
        move |arguments| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().push(arguments.clone());
                Ok(json!({ "echoed": arguments["value"] }))
            }
        },
    )
    .expect("echo tool");
    let mut agent = Agent::new(SWARM_ROOT_INSTRUCTIONS, "gpt-test")
        .transport("http")
        .base_url(format!("http://{service_address}"))
        .model_base_url(model.base_url())
        .api_key("test")
        .can_spawn(true)
        .tool(echo)
        .build()
        .expect("HTTP swarm tool agent");

    let result = agent
        .run("spawn the scripted child")
        .await
        .expect("swarm agent run");
    assert_eq!(
        calls.lock().as_slice(),
        &[json!({ "value": "from-swarm-child" })]
    );
    assert_eq!(result.status, TerminalAgentStatus::Completed);
    assert_eq!(result.raw.status, TerminalRunStatus::Completed);
    assert_eq!(result.text, "root finished after spawning child");
    assert_eq!(result.raw.outputs.len(), 2);
    assert!(result
        .raw
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::Completed));
    let child = result
        .raw
        .outputs
        .iter()
        .find(|output| output.session_id != result.session_id)
        .expect("spawned child output");
    assert_eq!(
        child.output.clone(),
        Some(json!(
            "child tool result: {\"echoed\":\"from-swarm-child\"}"
        ))
    );
    assert_eq!(model.request_count(), 4);
    agent.close().await.expect("close agent");
}

#[test]
fn failed_result_is_inspectable_and_status_error_is_opt_in() {
    let session_id = SessionId::new("session-1").expect("session");
    let output = AgentOutput {
        session_id: session_id.clone(),
        path: Vec::new(),
        status: TerminalAgentStatus::Failed,
        output: None,
        usage: Usage::default(),
        error: Some(ExecutionError {
            code: ExecutionErrorCode::ModelError,
            message: "provider failed".to_owned(),
            retryable: true,
            details: None,
        }),
    };
    let raw = RunResult {
        run_id: RunId::new("run-1").expect("run"),
        status: TerminalRunStatus::Failed,
        outputs: vec![output],
        usage: Usage::default(),
        artifacts: Vec::new(),
        metadata: BTreeMap::new(),
    };
    let result = result_from_run(raw, &session_id, false).expect("result");
    assert_eq!(result.text, "null");
    let error = result.into_ok().expect_err("failed result");
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.message(), "provider failed");
    assert!(error.retryable());
    assert_eq!(
        error.details(),
        Some(&json!({ "status": "failed", "executionCode": "model_error" }))
    );
}

fn assert_invalid(result: Result<Agent, MuzenError>, message: &str, path: &str) {
    let error = result.err().expect("invalid facade options");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert_eq!(error.message(), message);
    assert_eq!(error.details(), Some(&json!({ "path": path })));
}

fn sample_spec() -> SessionSpec {
    Agent::new("do it", "gpt-test")
        .api_key("test")
        .build()
        .expect("sample agent")
        .spec_template
        .clone()
}

fn value_tool(name: &str, value: &'static str) -> Tool {
    Tool::new(name, "", json!({ "type": "object" }), move |_| async move {
        Ok(Value::String(value.to_owned()))
    })
    .expect("tool")
}

fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" },
            "limit": { "type": "integer" }
        },
        "additionalProperties": false,
        "required": ["query", "limit"]
    })
}

fn echo_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "required": ["value"],
        "additionalProperties": false
    })
}

async fn post_json(client: &reqwest::Client, url: &str, body: Value) -> Value {
    client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("response JSON")
}

fn text_turn(text: &str) -> ModelTurn {
    ModelTurn {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        tool_calls: Vec::new(),
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            tool_calls: 0,
        },
        stop: ModelStop::EndTurn,
    }
}

struct FakePumpState {
    events: String,
    statuses: Mutex<VecDeque<StatusCode>>,
    answers: Mutex<Vec<(String, Value)>>,
    event_failures_remaining: AtomicUsize,
    fail_events_permanently: bool,
    event_requests: AtomicUsize,
    cancel_requests: AtomicUsize,
    cancelled: AtomicBool,
    block_result_until_cancel: bool,
    cancelled_notify: Notify,
}

struct FakePumpServer {
    address: std::net::SocketAddr,
    state: Arc<FakePumpState>,
    task: tokio::task::JoinHandle<()>,
}

impl FakePumpServer {
    async fn start(events: String, statuses: impl IntoIterator<Item = StatusCode>) -> Self {
        Self::start_configured(events, statuses, 0, false, false).await
    }

    async fn start_with_event_failures(
        events: String,
        statuses: impl IntoIterator<Item = StatusCode>,
        failures: usize,
    ) -> Self {
        Self::start_configured(events, statuses, failures, false, false).await
    }

    async fn start_with_permanent_event_failure(
        events: String,
        block_result_until_cancel: bool,
    ) -> Self {
        Self::start_configured(
            events,
            std::iter::empty(),
            0,
            true,
            block_result_until_cancel,
        )
        .await
    }

    async fn start_configured(
        events: String,
        statuses: impl IntoIterator<Item = StatusCode>,
        event_failures: usize,
        fail_events_permanently: bool,
        block_result_until_cancel: bool,
    ) -> Self {
        let state = Arc::new(FakePumpState {
            events,
            statuses: Mutex::new(statuses.into_iter().collect()),
            answers: Mutex::new(Vec::new()),
            event_failures_remaining: AtomicUsize::new(event_failures),
            fail_events_permanently,
            event_requests: AtomicUsize::new(0),
            cancel_requests: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            block_result_until_cancel,
            cancelled_notify: Notify::new(),
        });
        let app = Router::new()
            .route("/v1/sessions/{session}", get(fake_pump_session))
            .route("/v1/runs", post(fake_pump_start_run))
            .route("/v1/runs/{run}", get(fake_pump_snapshot))
            .route("/v1/runs/{run}/result", get(fake_pump_result))
            .route("/v1/runs/{run}/events", get(fake_pump_events))
            .route("/v1/runs/{run}/cancel", post(fake_pump_cancel))
            .route("/v1/runs/{run}/tools/{call}/result", post(fake_pump_answer))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake pump listener");
        let address = listener.local_addr().expect("fake pump address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fake pump server");
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn answers(&self) -> Vec<(String, Value)> {
        self.state.answers.lock().clone()
    }

    fn event_requests(&self) -> usize {
        self.state.event_requests.load(Ordering::SeqCst)
    }

    fn cancel_requests(&self) -> usize {
        self.state.cancel_requests.load(Ordering::SeqCst)
    }
}

impl Drop for FakePumpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake_pump_snapshot(AxumPath(run): AxumPath<String>) -> Json<Value> {
    Json(json!({
        "id": run,
        "status": "running",
        "roots": [],
        "agents": [],
        "lastSequence": 3,
        "createdAt": "now",
        "updatedAt": "now"
    }))
}

async fn fake_pump_session(AxumPath(session): AxumPath<String>) -> Json<Value> {
    Json(json!({
        "id": session,
        "status": "open",
        "createdAt": "now",
        "updatedAt": "now",
        "metadata": {}
    }))
}

async fn fake_pump_start_run() -> Json<Value> {
    Json(json!("pump-run"))
}

async fn fake_pump_result(State(state): State<Arc<FakePumpState>>) -> Json<Value> {
    if state.block_result_until_cancel && !state.cancelled.load(Ordering::SeqCst) {
        state.cancelled_notify.notified().await;
    }
    if state.cancelled.load(Ordering::SeqCst) {
        Json(json!({
            "runId": "pump-run",
            "status": "cancelled",
            "outputs": [],
            "usage": { "inputTokens": 0, "outputTokens": 0, "toolCalls": 0 },
            "artifacts": [],
            "metadata": {}
        }))
    } else {
        Json(Value::Null)
    }
}

async fn fake_pump_events(State(state): State<Arc<FakePumpState>>) -> axum::response::Response {
    state.event_requests.fetch_add(1, Ordering::SeqCst);
    let temporary_failure = state
        .event_failures_remaining
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
            (remaining > 0).then(|| remaining - 1)
        })
        .is_ok();
    if state.fail_events_permanently || temporary_failure {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "code": "unavailable",
                "message": "fake event stream unavailable",
                "retryable": true
            })),
        )
            .into_response();
    }
    ([(CONTENT_TYPE, "text/event-stream")], state.events.clone()).into_response()
}

async fn fake_pump_cancel(State(state): State<Arc<FakePumpState>>) -> Json<Value> {
    state.cancel_requests.fetch_add(1, Ordering::SeqCst);
    state.cancelled.store(true, Ordering::SeqCst);
    state.cancelled_notify.notify_waiters();
    Json(json!({ "sequence": 1 }))
}

async fn fake_pump_answer(
    State(state): State<Arc<FakePumpState>>,
    AxumPath((_run, call)): AxumPath<(String, String)>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    state.answers.lock().push((call, body));
    let status = state.statuses.lock().pop_front().unwrap_or(StatusCode::OK);
    if status.is_success() {
        return (status, Json(Value::Null)).into_response();
    }
    let code = if status == StatusCode::CONFLICT {
        "conflict"
    } else {
        "not_found"
    };
    (
        status,
        Json(json!({
            "code": code,
            "message": "benign answer race",
            "retryable": false
        })),
    )
        .into_response()
}

fn pump_event(sequence: u64, event_type: &str, payload: Value) -> String {
    let event = json!({
        "runId": "pump-run",
        "sequence": sequence,
        "type": event_type,
        "timestamp": "now",
        "payload": payload
    });
    format!("id: {sequence}\nevent: run.event\ndata: {event}\n\n")
}

struct GoldModelState {
    requests: Mutex<Vec<Value>>,
}

struct GoldModelServer {
    address: std::net::SocketAddr,
    state: Arc<GoldModelState>,
    task: tokio::task::JoinHandle<()>,
}

impl GoldModelServer {
    async fn start() -> Self {
        let state = Arc::new(GoldModelState {
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/chat/completions", post(gold_model_response))
            .route("/v1/chat/completions", post(gold_model_response))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("gold model listener");
        let address = listener.local_addr().expect("gold model address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("gold model server");
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn request_count(&self) -> usize {
        self.state.requests.lock().len()
    }
}

impl Drop for GoldModelServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn gold_model_response(
    State(state): State<Arc<GoldModelState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.requests.lock().push(body.clone());
    let tool_result = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str());
    let message = match tool_result {
        Some(result) => json!({
            "role": "assistant",
            "content": format!("tool result: {result}")
        }),
        None => json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "echo-call",
                "type": "function",
                "function": {
                    "name": "echo",
                    "arguments": "{\"value\":\"from-model\"}"
                }
            }]
        }),
    };
    Json(json!({
        "choices": [{
            "message": message,
            "finish_reason": if tool_result.is_some() { "stop" } else { "tool_calls" }
        }],
        "usage": { "prompt_tokens": 2, "completion_tokens": 1 }
    }))
}

const SWARM_ROOT_INSTRUCTIONS: &str = "Spawn one child that uses echo, then finish.";
const SWARM_CHILD_INSTRUCTIONS: &str = "Use echo once, then report its result.";

struct SwarmModelServer {
    address: std::net::SocketAddr,
    state: Arc<GoldModelState>,
    task: tokio::task::JoinHandle<()>,
}

impl SwarmModelServer {
    async fn start() -> Self {
        let state = Arc::new(GoldModelState {
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/chat/completions", post(swarm_model_response))
            .route("/v1/chat/completions", post(swarm_model_response))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("swarm model listener");
        let address = listener.local_addr().expect("swarm model address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("swarm model server");
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn request_count(&self) -> usize {
        self.state.requests.lock().len()
    }
}

impl Drop for SwarmModelServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn swarm_model_response(
    State(state): State<Arc<GoldModelState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.requests.lock().push(body.clone());
    let messages = body["messages"].as_array().expect("model messages");
    let system = messages
        .first()
        .and_then(|message| message["content"].as_str())
        .expect("system instructions");
    let tool_result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str());

    let (message, finish_reason) = match (system, tool_result) {
        (SWARM_ROOT_INSTRUCTIONS, None) => {
            let child = json!({
                "name": "echo-child",
                "instructions": [SWARM_CHILD_INSTRUCTIONS],
                "model": "default",
                "tools": [{
                    "provider": LOCAL_TOOLS_PROVIDER_ID,
                    "tool": "echo",
                    "description": "Echo the supplied value.",
                    "inputSchema": echo_schema(),
                    "effects": []
                }]
            });
            let arguments = json!({
                "agent": child,
                "input": "echo the scripted child value"
            });
            (
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "spawn-child-call",
                        "type": "function",
                        "function": {
                            "name": "agent_spawn",
                            "arguments": arguments.to_string()
                        }
                    }]
                }),
                "tool_calls",
            )
        }
        (SWARM_ROOT_INSTRUCTIONS, Some(_)) => (
            json!({
                "role": "assistant",
                "content": "root finished after spawning child"
            }),
            "stop",
        ),
        (SWARM_CHILD_INSTRUCTIONS, None) => (
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "child-echo-call",
                    "type": "function",
                    "function": {
                        "name": "echo",
                        "arguments": "{\"value\":\"from-swarm-child\"}"
                    }
                }]
            }),
            "tool_calls",
        ),
        (SWARM_CHILD_INSTRUCTIONS, Some(result)) => (
            json!({
                "role": "assistant",
                "content": format!("child tool result: {result}")
            }),
            "stop",
        ),
        (instructions, _) => panic!("unexpected model instructions: {instructions}"),
    };

    Json(json!({
        "choices": [{
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": { "prompt_tokens": 2, "completion_tokens": 1 }
    }))
}

struct ServiceProcess {
    child: Child,
}

impl ServiceProcess {
    async fn start(binary: &Path, address: std::net::SocketAddr) -> Self {
        let mut child = Command::new(binary)
            .args([
                "--listen",
                &address.to_string(),
                "--store",
                "memory",
                "--allow-loopback-http",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start muzen-agent-service");
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return Self { child };
            }
            if let Some(status) = child.try_wait().expect("poll service process") {
                panic!("muzen-agent-service exited before listening: {status}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("muzen-agent-service did not listen on {address}");
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn discover_agent_service_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MUZEN_AGENT_SERVICE_BIN") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/release/muzen-agent-service"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/muzen-agent-service"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn reserve_loopback_address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve service port");
    listener.local_addr().expect("reserved service address")
}

struct ScriptedProvider {
    turns: Mutex<VecDeque<ModelTurn>>,
    requests: Mutex<Vec<ModelRequest>>,
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new(turns: impl IntoIterator<Item = ModelTurn>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().clone()
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelTurn, ModelProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.requests.lock().push(request);
        self.turns
            .lock()
            .pop_front()
            .ok_or_else(|| ModelProviderError::new("script exhausted"))
    }
}
