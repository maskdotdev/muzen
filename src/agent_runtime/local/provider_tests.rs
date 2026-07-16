use std::collections::VecDeque;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use futures::TryStreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};

use super::provider::{anthropic_request, chat_request, responses_request, ModelRequest};
use super::LocalRuntimeConfig;
use crate::agent_runtime::{
    AgentInput, AgentMessage, ContentBlock, CreateOptions, EventOptions, ExecutionErrorCode,
    IdempotencyKey, MessagePage, MessageRole, ModelProtocol, ModelProviderKind, Muzen,
    OutputContract, PutSecretInput, RunLimits, SessionId, SessionSpec, SingleRunOptions,
    TerminalAgentStatus, TerminalRunStatus, ToolEffect, ToolGrant, ToolProviderId,
};

#[derive(Clone)]
struct RecordedRequest {
    path: String,
    headers: HeaderMap,
    body: Value,
}

type Responder = Arc<dyn Fn(&str, &Value) -> (StatusCode, Value) + Send + Sync>;

struct FakeState {
    requests: Mutex<Vec<RecordedRequest>>,
    queued: Mutex<VecDeque<(StatusCode, Value)>>,
    responder: Option<Responder>,
}

struct FakeServer {
    address: SocketAddr,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeServer {
    async fn queued(responses: impl IntoIterator<Item = (StatusCode, Value)>) -> Self {
        Self::start(Arc::new(FakeState {
            requests: Mutex::new(Vec::new()),
            queued: Mutex::new(responses.into_iter().collect()),
            responder: None,
        }))
        .await
    }

    async fn dynamic(responder: Responder) -> Self {
        Self::start(Arc::new(FakeState {
            requests: Mutex::new(Vec::new()),
            queued: Mutex::new(VecDeque::new()),
            responder: Some(responder),
        }))
        .await
    }

    async fn queued_ipv6(responses: impl IntoIterator<Item = (StatusCode, Value)>) -> Self {
        Self::start_on(
            Arc::new(FakeState {
                requests: Mutex::new(Vec::new()),
                queued: Mutex::new(responses.into_iter().collect()),
                responder: None,
            }),
            "[::1]:0",
        )
        .await
    }

    async fn start(state: Arc<FakeState>) -> Self {
        Self::start_on(state, "127.0.0.1:0").await
    }

    async fn start_on(state: Arc<FakeState>, address: &str) -> Self {
        let app = Router::new()
            .route("/v1/messages", post(fake_handler))
            .route("/v1/chat/completions", post(fake_handler))
            .route("/v1/responses", post(fake_handler))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .expect("fake listener");
        let address = listener.local_addr().expect("fake address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fake model server");
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

    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().clone()
    }
}

async fn fake_handler(
    State(state): State<Arc<FakeState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let path = uri.path().to_owned();
    state.requests.lock().push(RecordedRequest {
        path: path.clone(),
        headers,
        body: body.clone(),
    });
    let response = state
        .responder
        .as_ref()
        .map(|responder| responder(&path, &body))
        .or_else(|| state.queued.lock().pop_front())
        .expect("fake response");
    (response.0, Json(response.1))
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../fixtures/agent-interface-v1.json"))
        .expect("agent fixture")
}

fn session_spec() -> SessionSpec {
    let mut spec: SessionSpec =
        serde_json::from_value(fixture()["sessionSpec"].clone()).expect("session fixture");
    spec.agent.output = None;
    spec
}

fn input(text: &str) -> AgentInput {
    AgentInput {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    }
}

fn limits() -> RunLimits {
    RunLimits {
        max_active_agents: NonZeroU32::new(4).expect("limit"),
        max_agents: NonZeroU32::new(4).expect("limit"),
        max_depth: 1,
        max_input_bytes: NonZeroU64::new(4096).expect("limit"),
        max_total_tokens: None,
        max_total_tool_calls: Some(4),
        deadline_ms: None,
    }
}

async fn router_runtime(loopback: bool) -> Muzen {
    Muzen::local(LocalRuntimeConfig::memory_with_model_router().with_loopback_http(loopback))
        .await
        .expect("router runtime")
}

async fn put_key(muzen: &Muzen, key: &str) -> crate::agent_runtime::SecretRef {
    muzen
        .put_secret(PutSecretInput {
            value: base64::engine::general_purpose::STANDARD.encode(key),
            idempotency_key: None,
        })
        .await
        .expect("put secret")
}

fn configure_model(
    spec: &mut SessionSpec,
    provider: ModelProviderKind,
    protocol: ModelProtocol,
    base_url: String,
    secret: crate::agent_runtime::SecretRef,
) {
    let model = &mut spec.models[0];
    model.provider = provider;
    model.protocol = protocol;
    model.base_url = Some(base_url);
    model.credential = secret;
    model.max_output_tokens = NonZeroU64::new(37).expect("max output tokens");
}

fn grant_agent_builtins(spec: &mut SessionSpec) {
    spec.agent
        .tools
        .retain(|grant| grant.tool == "issues.search");
    spec.agent.tools.extend([
        ToolGrant {
            provider: ToolProviderId::new("builtin").expect("provider"),
            tool: "agent.spawn".to_owned(),
            effects: vec![ToolEffect::AgentSpawn],
            max_calls: None,
        },
        ToolGrant {
            provider: ToolProviderId::new("builtin").expect("provider"),
            tool: "agent.message".to_owned(),
            effects: vec![ToolEffect::AgentMessage],
            max_calls: None,
        },
    ]);
}

fn require_summary(spec: &mut SessionSpec) -> Value {
    let schema = json!({
        "type": "object",
        "properties": { "summary": { "type": "string" } },
        "required": ["summary"],
        "additionalProperties": false
    });
    spec.agent.output = Some(OutputContract {
        schema: schema.clone(),
        name: Some("implementation_result".to_owned()),
    });
    schema
}

fn assert_output_failure(result: &crate::agent_runtime::RunResult, path: &str) {
    assert_eq!(result.status, TerminalRunStatus::Failed);
    let output = &result.outputs[0];
    assert_eq!(output.status, TerminalAgentStatus::Failed);
    let error = output.error.as_ref().expect("output error");
    assert_eq!(error.code, ExecutionErrorCode::ModelError);
    assert!(!error.retryable);
    assert!(error.message.contains(path), "{}", error.message);
}

async fn run_once(muzen: &Muzen, spec: SessionSpec) -> crate::agent_runtime::RunResult {
    let session = muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("session");
    session
        .run(
            input("hello provider"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run")
        .wait()
        .await
        .expect("run result")
}

async fn output_contract_tool_round_trip(protocol: ModelProtocol) {
    let mut spec = session_spec();
    spec.agent.instructions = vec![ContentBlock::Text {
        text: "structured parent".to_owned(),
    }];
    grant_agent_builtins(&mut spec);
    require_summary(&mut spec);
    let mut child = spec.agent.clone();
    child.name = crate::agent_runtime::AgentName::new("plain-child").expect("child name");
    child.instructions = vec![ContentBlock::Text {
        text: "plain child".to_owned(),
    }];
    child.output = None;
    let arguments = json!({ "agent": child, "input": input("child work") }).to_string();
    let responder: Responder = Arc::new(move |path, body| {
        let (system, has_tool_result) = if path.ends_with("chat/completions") {
            (
                body["messages"][0]["content"].as_str().unwrap_or_default(),
                body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|message| message["role"] == "tool"),
            )
        } else {
            (
                body["input"][0]["content"].as_str().unwrap_or_default(),
                body["input"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|item| item["type"] == "function_call_output"),
            )
        };
        if path.ends_with("chat/completions") {
            let message = if system == "structured parent" && !has_tool_result {
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "spawn-1",
                        "type": "function",
                        "function": { "name": "agent_spawn", "arguments": arguments }
                    }]
                })
            } else if system == "structured parent" {
                json!({ "role": "assistant", "content": "{\"summary\":\"parent done\"}" })
            } else {
                json!({ "role": "assistant", "content": "child done" })
            };
            (
                StatusCode::OK,
                json!({
                    "choices": [{ "message": message, "finish_reason": if has_tool_result { "stop" } else { "tool_calls" } }],
                    "usage": { "prompt_tokens": 2, "completion_tokens": 1 }
                }),
            )
        } else {
            let output = if system == "structured parent" && !has_tool_result {
                json!([{
                    "type": "function_call", "call_id": "spawn-1", "name": "agent_spawn",
                    "arguments": arguments
                }])
            } else if system == "structured parent" {
                json!([{
                    "type": "message", "role": "assistant",
                    "content": [{ "type": "output_text", "text": "{\"summary\":\"parent done\"}" }]
                }])
            } else {
                json!([{
                    "type": "message", "role": "assistant",
                    "content": [{ "type": "output_text", "text": "child done" }]
                }])
            };
            (
                StatusCode::OK,
                json!({
                    "output": output,
                    "usage": { "input_tokens": 2, "output_tokens": 1 },
                    "status": "completed"
                }),
            )
        }
    });
    let fake = FakeServer::dynamic(responder).await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "tool-output-key").await;
    configure_model(
        &mut spec,
        ModelProviderKind::OpenaiCompatible,
        protocol,
        format!("{}/v1", fake.base_url()),
        secret,
    );

    let result = run_once(&muzen, spec).await;
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(result.outputs.len(), 2);
    assert!(result
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::Completed));
    assert!(result
        .outputs
        .iter()
        .any(|output| output.output == Some(json!({ "summary": "parent done" }))));

    let parent_requests = fake
        .requests()
        .into_iter()
        .filter(|request| {
            request.body["messages"][0]["content"] == "structured parent"
                || request.body["input"][0]["content"] == "structured parent"
        })
        .collect::<Vec<_>>();
    assert_eq!(parent_requests.len(), 2);
    for request in parent_requests {
        assert!(request.body.get("tools").is_some());
        match protocol {
            ModelProtocol::ChatCompletions => {
                assert_eq!(request.body["response_format"]["type"], "json_schema")
            }
            ModelProtocol::Responses => {
                assert_eq!(request.body["text"]["format"]["type"], "json_schema")
            }
            ModelProtocol::Messages => unreachable!(),
        }
    }
}

#[tokio::test]
async fn anthropic_router_runs_end_to_end() {
    let fake = FakeServer::queued([(
        StatusCode::OK,
        json!({
            "content": [{"type": "text", "text": "anthropic done"}],
            "usage": {"input_tokens": 8, "output_tokens": 3},
            "stop_reason": "end_turn"
        }),
    )])
    .await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "anthropic-test-key").await;
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::Anthropic,
        ModelProtocol::Messages,
        fake.base_url(),
        secret,
    );
    let session = muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("session");
    let result = session
        .run(
            input("hello"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run")
        .wait()
        .await
        .expect("result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(
        (result.usage.input_tokens, result.usage.output_tokens),
        (8, 3)
    );
    let messages = session
        .messages(MessagePage::default())
        .await
        .expect("messages");
    assert_eq!(
        messages.items.last().expect("assistant").role,
        MessageRole::Assistant
    );
    let requests = fake.requests();
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].headers["x-api-key"], "anthropic-test-key");
    assert_eq!(requests[0].headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        requests[0].body["system"],
        "Implement the requested change."
    );
    assert_eq!(requests[0].body["max_tokens"], 37);
}

#[tokio::test]
async fn anthropic_prompts_and_enforces_output_contract() {
    let response = |text: &str| {
        json!({
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 8, "output_tokens": 3},
            "stop_reason": "end_turn"
        })
    };
    let fake = FakeServer::queued([
        (StatusCode::OK, response("{\"summary\":\"done\"}")),
        (StatusCode::OK, response("{}")),
        (StatusCode::OK, response("not json")),
    ])
    .await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "anthropic-output-key").await;
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::Anthropic,
        ModelProtocol::Messages,
        fake.base_url(),
        secret,
    );
    let schema = require_summary(&mut spec);

    let valid = run_once(&muzen, spec.clone()).await;
    assert_eq!(valid.outputs[0].output, Some(json!({ "summary": "done" })));
    assert_output_failure(&run_once(&muzen, spec.clone()).await, "$.summary");
    assert_output_failure(&run_once(&muzen, spec).await, "at $");

    for request in fake.requests() {
        let system = request.body["system"].as_str().expect("system prompt");
        assert!(system.contains("final non-tool-use turn"));
        assert!(system.contains("implementation_result"));
        assert!(system.contains(&schema.to_string()));
        assert!(request.body.get("response_format").is_none());
        assert!(request.body.get("tool_choice").is_none());
    }
}

#[tokio::test]
async fn chat_completions_router_runs_end_to_end() {
    let fake = FakeServer::queued([(StatusCode::OK, json!({
        "choices": [{"message": {"role": "assistant", "content": "chat done"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 6, "completion_tokens": 2}
    }))])
    .await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "openai-chat-key").await;
    let mut spec = session_spec();
    grant_agent_builtins(&mut spec);
    configure_model(
        &mut spec,
        ModelProviderKind::OpenaiCompatible,
        ModelProtocol::ChatCompletions,
        format!("{}/v1", fake.base_url()),
        secret,
    );
    let result = run_once(&muzen, spec).await;
    assert_eq!(result.status, TerminalRunStatus::Completed);
    let request = &fake.requests()[0];
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.headers["authorization"], "Bearer openai-chat-key");
    assert_eq!(request.body["max_tokens"], 37);
    let names = request.body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().expect("name"))
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["agent_spawn", "agent_message"]);
    assert!(!request.body.to_string().contains("agent.spawn"));
    let spawn = &request.body["tools"][0]["function"]["parameters"];
    let instructions = &spawn["properties"]["agent"]["properties"]["instructions"];
    assert_eq!(instructions["oneOf"][0]["type"], "string");
    assert_eq!(
        instructions["oneOf"][1]["items"]["oneOf"][1]["properties"]["type"]["const"],
        "text"
    );
    assert_eq!(spawn["properties"]["input"]["oneOf"][0]["type"], "string");
}

#[tokio::test]
async fn chat_completions_maps_and_enforces_output_contract() {
    let fake = FakeServer::queued([
        (StatusCode::OK, json!({
            "choices": [{"message": {"role": "assistant", "content": "{\"summary\":\"done\"}"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 6, "completion_tokens": 2}
        })),
        (StatusCode::OK, json!({
            "choices": [{"message": {"role": "assistant", "content": "{}"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 6, "completion_tokens": 1}
        })),
        (StatusCode::OK, json!({
            "choices": [{"message": {"role": "assistant", "content": "not json"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 6, "completion_tokens": 1}
        })),
    ])
    .await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "chat-output-key").await;
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::OpenaiCompatible,
        ModelProtocol::ChatCompletions,
        format!("{}/v1", fake.base_url()),
        secret,
    );
    let schema = require_summary(&mut spec);

    let valid = run_once(&muzen, spec.clone()).await;
    assert_eq!(valid.outputs[0].output, Some(json!({ "summary": "done" })));
    assert_output_failure(&run_once(&muzen, spec.clone()).await, "$.summary");
    assert_output_failure(&run_once(&muzen, spec).await, "at $");

    for request in fake.requests() {
        assert_eq!(
            request.body["response_format"],
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "implementation_result",
                    "schema": schema,
                    "strict": true
                }
            })
        );
    }
}

#[tokio::test]
async fn chat_completions_keeps_output_format_during_tool_turns() {
    output_contract_tool_round_trip(ModelProtocol::ChatCompletions).await;
}

#[tokio::test]
async fn ipv6_loopback_http_is_allowed_with_opt_in() {
    let fake = FakeServer::queued_ipv6([(StatusCode::OK, json!({
        "choices": [{"message": {"role": "assistant", "content": "ipv6 done"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 2, "completion_tokens": 1}
    }))])
    .await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "ipv6-key").await;
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::OpenaiCompatible,
        ModelProtocol::ChatCompletions,
        format!("{}/v1", fake.base_url()),
        secret,
    );
    let result = run_once(&muzen, spec).await;
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(fake.requests()[0].path, "/v1/chat/completions");
}

#[tokio::test]
async fn responses_router_runs_end_to_end() {
    let fake = FakeServer::queued([(StatusCode::OK, json!({
        "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "responses done"}]}],
        "usage": {"input_tokens": 7, "output_tokens": 4},
        "status": "completed"
    }))])
    .await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "openai-responses-key").await;
    let mut spec = session_spec();
    grant_agent_builtins(&mut spec);
    configure_model(
        &mut spec,
        ModelProviderKind::OpenaiCompatible,
        ModelProtocol::Responses,
        format!("{}/v1", fake.base_url()),
        secret,
    );
    let result = run_once(&muzen, spec).await;
    assert_eq!(result.status, TerminalRunStatus::Completed);
    let request = &fake.requests()[0];
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(
        request.headers["authorization"],
        "Bearer openai-responses-key"
    );
    assert_eq!(request.body["max_output_tokens"], 37);
    let names = request.body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["agent_spawn", "agent_message"]);
    assert!(!request.body.to_string().contains("agent.spawn"));
}

#[tokio::test]
async fn responses_maps_and_enforces_output_contract() {
    let response = |text: &str| {
        json!({
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}],
            "usage": {"input_tokens": 7, "output_tokens": 2},
            "status": "completed"
        })
    };
    let fake = FakeServer::queued([
        (StatusCode::OK, response("{\"summary\":\"done\"}")),
        (StatusCode::OK, response("{}")),
        (StatusCode::OK, response("not json")),
    ])
    .await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "responses-output-key").await;
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::OpenaiCompatible,
        ModelProtocol::Responses,
        format!("{}/v1", fake.base_url()),
        secret,
    );
    let schema = require_summary(&mut spec);

    let valid = run_once(&muzen, spec.clone()).await;
    assert_eq!(valid.outputs[0].output, Some(json!({ "summary": "done" })));
    assert_output_failure(&run_once(&muzen, spec.clone()).await, "$.summary");
    assert_output_failure(&run_once(&muzen, spec).await, "at $");

    for request in fake.requests() {
        assert_eq!(
            request.body["text"],
            json!({
                "format": {
                    "type": "json_schema",
                    "name": "implementation_result",
                    "schema": schema,
                    "strict": true
                }
            })
        );
    }
}

#[tokio::test]
async fn responses_keeps_output_format_during_tool_turns() {
    output_contract_tool_round_trip(ModelProtocol::Responses).await;
}

#[tokio::test]
async fn anthropic_tool_round_trip_reconstructs_calls_and_results() {
    let responder: Responder = Arc::new(|_, body| {
        let system = body["system"].as_str().unwrap_or_default();
        let has_result = body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .any(|block| block["type"] == "tool_result");
        if system == "parent instructions" && !has_result {
            let child = json!({
                "name": "child",
                "instructions": ["child instructions"],
                "model": "primary",
                "tools": [],
                "budget": {
                    "maxTurns": 20,
                    "maxToolCalls": 60,
                    "maxPromptTokens": 120000,
                    "maxOutputTokens": 20000
                }
            });
            (
                StatusCode::OK,
                json!({
                    "content": [
                        {
                            "type": "tool_use", "id": "unknown-1", "name": "imaginary_tool",
                            "input": {"value": true}
                        },
                        {
                            "type": "tool_use", "id": "spawn-1", "name": "agent_spawn",
                            "input": {"agent": child, "input": "work"}
                        }
                    ],
                    "usage": {"input_tokens": 4, "output_tokens": 2}, "stop_reason": "tool_use"
                }),
            )
        } else {
            (
                StatusCode::OK,
                json!({
                    "content": [{"type": "text", "text": "done"}],
                    "usage": {"input_tokens": 3, "output_tokens": 1}, "stop_reason": "end_turn"
                }),
            )
        }
    });
    let fake = FakeServer::dynamic(responder).await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "tool-key").await;
    let mut spec = session_spec();
    spec.agent.instructions = vec![ContentBlock::Text {
        text: "parent instructions".to_owned(),
    }];
    grant_agent_builtins(&mut spec);
    configure_model(
        &mut spec,
        ModelProviderKind::Anthropic,
        ModelProtocol::Messages,
        fake.base_url(),
        secret,
    );
    let session = muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("hello provider"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let result = run.wait().await.expect("run result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(result.outputs.len(), 2);
    assert!(result
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::Completed));
    let requests = fake.requests();
    let first_parent = requests
        .iter()
        .find(|request| {
            request.body["system"] == "parent instructions"
                && request.body["messages"]
                    .as_array()
                    .is_some_and(|messages| messages.len() == 1)
        })
        .expect("first parent request");
    let tool_names = first_parent.body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["agent_spawn", "agent_message"]);
    assert!(!first_parent.body.to_string().contains("agent.spawn"));
    let second_parent = requests
        .iter()
        .find(|request| {
            request.body["system"] == "parent instructions"
                && request.body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|message| message["content"].as_array().into_iter().flatten())
                    .any(|block| block["type"] == "tool_result")
        })
        .expect("second parent request");
    let blocks = second_parent.body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    assert!(blocks.iter().any(|block| {
        block["type"] == "tool_use" && block["id"] == "spawn-1" && block["name"] == "agent_spawn"
    }));
    assert!(blocks
        .iter()
        .any(|block| block["type"] == "tool_result" && block["tool_use_id"] == "spawn-1"));
    assert!(blocks.iter().any(|block| {
        block["type"] == "tool_result"
            && block["tool_use_id"] == "unknown-1"
            && block["is_error"] == true
    }));
    let public_messages = session
        .messages(MessagePage::default())
        .await
        .expect("public messages")
        .items;
    assert!(public_messages
        .iter()
        .filter(|message| { message.role == MessageRole::Assistant })
        .all(|message| !serde_json::to_string(&message.content)
            .expect("assistant content JSON")
            .contains("assistant_tool_calls")));
    assert!(public_messages
        .iter()
        .any(|message| { message.role == MessageRole::Assistant && message.content.is_empty() }));
    let public_wire = serde_json::to_string(&public_messages).expect("public message JSON");
    assert!(public_wire.contains("agent.spawn"));
    assert!(!public_wire.contains("agent_spawn"));
    let events = run
        .events(EventOptions::default())
        .try_collect::<Vec<_>>()
        .await
        .expect("events");
    assert!(events.iter().any(|event| event.event_type == "tool.failed"));
}

#[test]
fn all_protocol_replay_paths_use_wire_safe_builtin_names() {
    let mut spec = session_spec();
    grant_agent_builtins(&mut spec);
    let session_id = SessionId::new("wire-test").expect("session id");
    let assistant = AgentMessage {
        id: "assistant".to_owned(),
        session_id: session_id.clone(),
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: json!({
                "_muzen": "assistant_tool_calls",
                "calls": [{
                    "id": "call-1", "provider": "builtin", "name": "agent.spawn",
                    "arguments": {}
                }]
            })
            .to_string(),
        }],
        created_at: "2026-01-01T00:00:00Z".to_owned(),
    };
    let legacy_tool = AgentMessage {
        id: "legacy-tool".to_owned(),
        session_id,
        role: MessageRole::Tool,
        content: vec![ContentBlock::Text {
            text: json!({
                "callId": "legacy-1", "provider": "builtin", "tool": "agent.message",
                "result": true
            })
            .to_string(),
        }],
        created_at: "2026-01-01T00:00:01Z".to_owned(),
    };
    let request = ModelRequest {
        agent: spec.agent,
        model: spec.models.remove(0),
        transcript: vec![assistant, legacy_tool],
        tool_providers: spec.tool_providers,
    };
    for body in [
        anthropic_request(&request),
        chat_request(&request),
        responses_request(&request),
    ] {
        let wire = body.to_string();
        assert!(wire.contains("agent_spawn"));
        assert!(wire.contains("agent_message"));
        assert!(!wire.contains("agent.spawn"));
        assert!(!wire.contains("agent.message"));
    }
}

#[tokio::test]
async fn deleted_secret_surfaces_secret_unavailable() {
    let fake = FakeServer::queued([]).await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "deleted-key").await;
    muzen.delete_secret(&secret).await.expect("delete");
    muzen.delete_secret(&secret).await.expect("delete replay");
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::Anthropic,
        ModelProtocol::Messages,
        fake.base_url(),
        secret,
    );
    let result = run_once(&muzen, spec).await;
    assert_eq!(result.status, TerminalRunStatus::Failed);
    assert_eq!(
        result.outputs[0].error.as_ref().expect("error").code,
        ExecutionErrorCode::SecretUnavailable
    );
    assert_eq!(
        serde_json::to_value(result.outputs[0].error.as_ref().expect("error")).expect("wire error")
            ["code"],
        "secretUnavailable"
    );
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn provider_429_is_retryable_and_safe() {
    let fake =
        FakeServer::queued([(StatusCode::TOO_MANY_REQUESTS, json!({"error": "slow down"}))]).await;
    let muzen = router_runtime(true).await;
    let secret = put_key(&muzen, "never-echo-this-key").await;
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::Anthropic,
        ModelProtocol::Messages,
        fake.base_url(),
        secret,
    );
    let result = run_once(&muzen, spec).await;
    let error = result.outputs[0].error.as_ref().expect("error");
    assert!(error.retryable);
    assert_eq!(error.details.as_ref().expect("details")["status"], 429);
    assert!(!format!("{error:?}").contains("never-echo-this-key"));
}

#[tokio::test]
async fn loopback_http_is_disabled_by_default() {
    let fake = FakeServer::queued([]).await;
    let muzen = router_runtime(false).await;
    let secret = put_key(&muzen, "loopback-key").await;
    let mut spec = session_spec();
    configure_model(
        &mut spec,
        ModelProviderKind::Anthropic,
        ModelProtocol::Messages,
        fake.base_url(),
        secret,
    );
    let result = run_once(&muzen, spec).await;
    let error = result.outputs[0].error.as_ref().expect("error");
    assert!(!error.retryable);
    assert!(error
        .message
        .contains("loopback HTTP requires explicit local opt-in"));
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn secret_put_replay_and_delete_are_idempotent() {
    let muzen = router_runtime(false).await;
    let input = PutSecretInput {
        value: base64::engine::general_purpose::STANDARD.encode("same-value"),
        idempotency_key: Some(IdempotencyKey::new("secret-replay").expect("key")),
    };
    let first = muzen.put_secret(input.clone()).await.expect("first put");
    assert_eq!(muzen.put_secret(input).await.expect("replay put"), first);
    let conflict = muzen
        .put_secret(PutSecretInput {
            value: base64::engine::general_purpose::STANDARD.encode("different-value"),
            idempotency_key: Some(IdempotencyKey::new("secret-replay").expect("key")),
        })
        .await
        .expect_err("digest conflict");
    assert_eq!(conflict.code(), crate::agent_runtime::ErrorCode::Conflict);
    muzen.delete_secret(&first).await.expect("delete");
    muzen.delete_secret(&first).await.expect("delete replay");
}
