use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

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
    assert_invalid(
        Agent::new("do it", "gpt-test")
            .api_key("test")
            .transport("http")
            .base_url("https://muzen.example")
            .tool(value_tool("lookup", "ok"))
            .build(),
        "tools require transport='local'; a remote service cannot reach the client's loopback server",
        "tools",
    );
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
