use std::collections::{BTreeMap, VecDeque};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue as AxumHeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use futures::{future::join_all, StreamExt, TryStreamExt};
use libsql::{Builder, TransactionBehavior};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use super::{
    LocalRuntime, LocalRuntimeConfig, ModelProvider, ModelProviderError, ModelRequest, ModelStop,
    ModelToolCall, ModelTurn,
};
use crate::agent_runtime::client::RuntimeTransport;
use crate::agent_runtime::{
    AgentEvent, AgentInput, AgentName, AgentStatus, CancelOptions, ContentBlock, CreateOptions,
    ErrorCode, EventOptions, ExecutionErrorCode, ExistingSessionRoot, HeaderSecret, HeaderValue,
    IdempotencyKey, MessageDelivery, MessagePage, MessageRole, ModelProfileId, Muzen,
    OutputContract, PutSecretInput, Run, RunLimits, RunRoot, RunSpec, SendCommand, SessionId,
    SessionSpec, SingleRunOptions, SpawnCommand, TerminalAgentStatus, TerminalRunStatus,
    ToolEffect, ToolGrant, ToolProvider, ToolProviderId, Usage,
};

struct FakeMcpState {
    requests: Mutex<Vec<(HeaderMap, Value)>>,
    call_sse: bool,
    call_error: bool,
    call_status: StatusCode,
}

struct FakeMcpServer {
    url: String,
    state: Arc<FakeMcpState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeMcpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeMcpServer {
    async fn start(call_sse: bool, call_error: bool, call_status: StatusCode) -> Self {
        let state = Arc::new(FakeMcpState {
            requests: Mutex::new(Vec::new()),
            call_sse,
            call_error,
            call_status,
        });
        let app = Router::new()
            .route("/mcp", post(fake_mcp_handler))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("MCP listener");
        let address = listener.local_addr().expect("MCP address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fake MCP server");
        });
        Self {
            url: format!("http://{address}/mcp"),
            state,
            task,
        }
    }
}

async fn fake_mcp_handler(
    State(state): State<Arc<FakeMcpState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.requests.lock().push((headers, body.clone()));
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    match body["method"].as_str() {
        Some("initialize") => {
            let mut response = Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "protocolVersion": "2025-03-26", "capabilities": {},
                    "serverInfo": { "name": "fake", "version": "1" } }
            }))
            .into_response();
            response.headers_mut().insert(
                "mcp-session-id",
                AxumHeaderValue::from_static("session-test"),
            );
            response
        }
        Some("notifications/initialized") => StatusCode::ACCEPTED.into_response(),
        Some("tools/list") => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "tools": [{
                "name": "issues_search", "description": "Search issues",
                "inputSchema": {
                    "type": "object", "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }
            }] }
        }))
        .into_response(),
        Some("tools/call") if !state.call_status.is_success() => {
            (state.call_status, "call failed").into_response()
        }
        Some("tools/call") => {
            let result = if state.call_error {
                json!({ "content": [{ "type": "text", "text": "remote tool failed" }], "isError": true })
            } else {
                json!({ "content": [{ "type": "text", "text": "found issue" }] })
            };
            let message = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if state.call_sse {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(format!("event: message\ndata: {message}\n\n")))
                    .expect("SSE response")
            } else {
                Json(message).into_response()
            }
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

enum Script {
    Turn {
        delay: Duration,
        turn: ModelTurn,
    },
    Error {
        delay: Duration,
        error: ModelProviderError,
    },
}

struct ScriptedProvider {
    scripts: Mutex<VecDeque<Script>>,
    named_scripts: Mutex<BTreeMap<String, VecDeque<Script>>>,
    requests: Mutex<Vec<ModelRequest>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl ScriptedProvider {
    fn new(scripts: impl IntoIterator<Item = Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            named_scripts: Mutex::new(BTreeMap::new()),
            requests: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        })
    }

    fn named(scripts: impl IntoIterator<Item = (String, Vec<Script>)>) -> Arc<Self> {
        let provider = Self::new([]);
        *provider.named_scripts.lock() = scripts
            .into_iter()
            .map(|(name, scripts)| (name, scripts.into()))
            .collect();
        provider
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::Acquire)
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().clone()
    }
}

struct ActiveCall<'a>(&'a ScriptedProvider);

impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelTurn, ModelProviderError> {
        let agent_name = request.agent.name.as_str().to_owned();
        self.requests.lock().push(request);
        let script = self
            .named_scripts
            .lock()
            .get_mut(&agent_name)
            .and_then(VecDeque::pop_front)
            .or_else(|| self.scripts.lock().pop_front())
            .expect("scripted provider turn");
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
        let _active = ActiveCall(self);
        match script {
            Script::Turn { delay, turn } => {
                tokio::time::sleep(delay).await;
                Ok(turn)
            }
            Script::Error { delay, error } => {
                tokio::time::sleep(delay).await;
                Err(error)
            }
        }
    }
}

struct GatedDelayedProvider {
    started: Semaphore,
    release: Semaphore,
}

impl GatedDelayedProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    async fn wait_started(&self, count: u32) {
        self.started
            .acquire_many(count)
            .await
            .expect("gated provider remains open")
            .forget();
    }

    fn release(&self, count: usize) {
        self.release.add_permits(count);
    }
}

#[async_trait]
impl ModelProvider for GatedDelayedProvider {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelTurn, ModelProviderError> {
        self.started.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("gated provider remains open")
            .forget();
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(ModelTurn {
            content: vec![ContentBlock::Text {
                text: "done".to_owned(),
            }],
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
                tool_calls: 0,
            },
            stop: ModelStop::EndTurn,
        })
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../fixtures/agent-interface-v1.json"))
        .expect("agent fixture")
}

fn session_spec() -> SessionSpec {
    let mut spec: SessionSpec =
        serde_json::from_value(fixture()["sessionSpec"].clone()).expect("session fixture");
    spec.agent.output = None;
    spec.agent
        .tools
        .retain(|grant| grant.provider.as_str() != "mcp");
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
        max_depth: 0,
        max_input_bytes: NonZeroU64::new(1024).expect("limit"),
        max_total_tokens: None,
        max_total_tool_calls: Some(0),
        deadline_ms: None,
    }
}

fn turn(text: &str, input_tokens: u64, output_tokens: u64) -> Script {
    Script::Turn {
        delay: Duration::ZERO,
        turn: ModelTurn {
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens,
                output_tokens,
                tool_calls: 0,
            },
            stop: ModelStop::EndTurn,
        },
    }
}

fn delayed_turn(delay: Duration, text: &str) -> Script {
    Script::Turn {
        delay,
        turn: ModelTurn {
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
                tool_calls: 0,
            },
            stop: ModelStop::EndTurn,
        },
    }
}

fn tool_turn(calls: Vec<ModelToolCall>) -> Script {
    Script::Turn {
        delay: Duration::ZERO,
        turn: ModelTurn {
            content: Vec::new(),
            tool_calls: calls,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                tool_calls: 0,
            },
            stop: ModelStop::ToolUse,
        },
    }
}

fn tool_call(id: &str, provider: &str, name: &str, arguments: Value) -> ModelToolCall {
    ModelToolCall {
        id: id.to_owned(),
        provider: ToolProviderId::new(provider).expect("provider id"),
        name: name.to_owned(),
        arguments,
    }
}

fn grant(spec: &mut SessionSpec, provider: &str, tool: &str, effect: Option<ToolEffect>) {
    spec.agent.tools.push(ToolGrant {
        provider: ToolProviderId::new(provider).expect("provider id"),
        tool: tool.to_owned(),
        effects: effect.into_iter().collect(),
        max_calls: None,
    });
}

fn require_summary(spec: &mut SessionSpec) {
    spec.agent.output = Some(OutputContract {
        schema: json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } },
            "required": ["summary"],
            "additionalProperties": false
        }),
        name: None,
    });
}

async fn memory_runtime(provider: Arc<ScriptedProvider>) -> Muzen {
    Muzen::local(LocalRuntimeConfig::memory(provider))
        .await
        .expect("local runtime")
}

async fn create_sessions(muzen: &Muzen, count: usize) -> Vec<SessionId> {
    let mut ids = Vec::new();
    for _ in 0..count {
        ids.push(
            muzen
                .create_session(session_spec(), CreateOptions::default())
                .await
                .expect("create session")
                .id()
                .clone(),
        );
    }
    ids
}

async fn start_roots(muzen: &Muzen, ids: &[SessionId], limits: RunLimits) -> Run {
    muzen
        .start_run(RunSpec {
            roots: ids
                .iter()
                .map(|session_id| {
                    RunRoot::Existing(ExistingSessionRoot {
                        session_id: session_id.clone(),
                        input: input(session_id.as_str()),
                    })
                })
                .collect(),
            limits,
            idempotency_key: None,
            metadata: Default::default(),
        })
        .await
        .expect("start run")
}

async fn all_events(run: &Run) -> Vec<AgentEvent> {
    run.events(EventOptions::default())
        .try_collect()
        .await
        .expect("run events")
}

async fn wait_for_event(run: &Run, event_type: &str) -> AgentEvent {
    let mut events = run.events(EventOptions::default());
    while let Some(event) = events.next().await {
        let event = event.expect("run event");
        if event.event_type == event_type {
            return event;
        }
    }
    panic!("event stream ended before {event_type}")
}

async fn mcp_session(
    muzen: &Muzen,
    url: String,
    secret: Option<crate::agent_runtime::SecretRef>,
) -> crate::agent_runtime::AgentSession {
    let mut spec = session_spec();
    spec.agent.tools.clear();
    spec.agent.tools.push(ToolGrant {
        provider: ToolProviderId::new("mcp").expect("provider"),
        tool: "issues_search".to_owned(),
        effects: vec![ToolEffect::NetworkRead],
        max_calls: NonZeroU32::new(1),
    });
    spec.tool_providers.clear();
    let mut headers = BTreeMap::from([(
        "X-Tenant".to_owned(),
        HeaderValue::Literal("tenant-a".to_owned()),
    )]);
    if let Some(secret) = secret {
        headers.insert(
            "Authorization".to_owned(),
            HeaderValue::Secret(HeaderSecret { secret }),
        );
    }
    spec.tool_providers.push(ToolProvider::McpHttp {
        id: ToolProviderId::new("mcp").expect("provider"),
        url,
        credential: None,
        headers,
    });
    muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("MCP session")
}

fn mcp_limits() -> RunLimits {
    let mut value = limits();
    value.max_total_tool_calls = Some(2);
    value
}

#[tokio::test]
async fn mcp_sse_tool_call_resolves_headers_session_and_result() {
    let fake = FakeMcpServer::start(true, false, StatusCode::OK).await;
    let provider = ScriptedProvider::new([
        tool_turn(vec![
            tool_call(
                "mcp-1",
                "mcp",
                "issues_search",
                json!({ "query": "runtime" }),
            ),
            tool_call(
                "mcp-2",
                "mcp",
                "issues_search",
                json!({ "query": "over limit" }),
            ),
        ]),
        turn("done", 2, 1),
    ]);
    let muzen = Muzen::local(LocalRuntimeConfig::memory(provider.clone()).with_loopback_http(true))
        .await
        .expect("runtime");
    let secret = muzen
        .put_secret(PutSecretInput {
            value: base64::engine::general_purpose::STANDARD.encode("Bearer test-token"),
            idempotency_key: None,
        })
        .await
        .expect("secret");
    let session = mcp_session(&muzen, fake.url.clone(), Some(secret)).await;
    let run = session
        .run(
            input("search"),
            SingleRunOptions {
                limits: mcp_limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let result = run.wait().await.expect("result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(result.usage.tool_calls, 1);

    let model_requests = provider.requests();
    assert_eq!(model_requests[0].mcp_tools[0].name, "issues_search");
    assert_eq!(
        model_requests[0].mcp_tools[0].input_schema["required"],
        json!(["query"])
    );
    let transcript = &model_requests[1].transcript;
    let tool_message = transcript
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("tool result");
    assert!(serde_json::to_string(&tool_message.content)
        .expect("tool message")
        .contains("found issue"));

    let requests = fake.state.requests.lock();
    assert_eq!(
        requests
            .iter()
            .filter(|(_, body)| body["method"] == "initialize")
            .count(),
        1
    );
    let (headers, call) = requests
        .iter()
        .find(|(_, body)| body["method"] == "tools/call")
        .expect("tools/call");
    assert_eq!(call["params"]["arguments"], json!({ "query": "runtime" }));
    assert_eq!(headers["authorization"], "Bearer test-token");
    assert_eq!(headers["x-tenant"], "tenant-a");
    assert_eq!(headers["mcp-session-id"], "session-test");
    assert_eq!(
        requests
            .iter()
            .filter(|(_, body)| body["method"] == "tools/call")
            .count(),
        1,
        "grant maxCalls must reject the second call before dispatch"
    );
}

#[tokio::test]
async fn mcp_is_error_and_http_failure_continue_with_tool_errors() {
    for (is_error, status, retryable) in [
        (true, StatusCode::OK, false),
        (false, StatusCode::INTERNAL_SERVER_ERROR, true),
    ] {
        let fake = FakeMcpServer::start(false, is_error, status).await;
        let provider = ScriptedProvider::new([
            tool_turn(vec![tool_call(
                "mcp-err",
                "mcp",
                "issues_search",
                json!({}),
            )]),
            turn("continued", 1, 1),
        ]);
        let muzen =
            Muzen::local(LocalRuntimeConfig::memory(provider.clone()).with_loopback_http(true))
                .await
                .expect("runtime");
        let session = mcp_session(&muzen, fake.url.clone(), None).await;
        let run = session
            .run(
                input("search"),
                SingleRunOptions {
                    limits: mcp_limits(),
                    idempotency_key: None,
                    metadata: Default::default(),
                },
            )
            .await
            .expect("run");
        let result = run.wait().await.expect("result");
        assert_eq!(result.status, TerminalRunStatus::Completed);
        assert_eq!(result.usage.tool_calls, 1);
        let events = all_events(&run).await;
        let failed = events
            .into_iter()
            .find(|event| event.event_type == "tool.failed")
            .expect("tool.failed");
        assert_eq!(failed.payload["error"]["code"], "tool_error");
        assert_eq!(failed.payload["error"]["retryable"], retryable);
    }
}

#[tokio::test]
async fn mcp_missing_secret_and_loopback_policy_fail_without_stopping_agent() {
    for missing_secret in [true, false] {
        let fake = FakeMcpServer::start(false, false, StatusCode::OK).await;
        let provider = ScriptedProvider::new([
            tool_turn(vec![tool_call(
                "mcp-policy",
                "mcp",
                "issues_search",
                json!({}),
            )]),
            turn("continued", 1, 1),
        ]);
        let muzen = Muzen::local(
            LocalRuntimeConfig::memory(provider.clone()).with_loopback_http(missing_secret),
        )
        .await
        .expect("runtime");
        let unavailable =
            crate::agent_runtime::SecretRef::new("secret_missing").expect("secret ref");
        let session = mcp_session(
            &muzen,
            fake.url.clone(),
            missing_secret.then_some(unavailable),
        )
        .await;
        let run = session
            .run(
                input("search"),
                SingleRunOptions {
                    limits: mcp_limits(),
                    idempotency_key: None,
                    metadata: Default::default(),
                },
            )
            .await
            .expect("run");
        assert_eq!(
            run.wait().await.expect("result").status,
            TerminalRunStatus::Completed
        );
        let events = all_events(&run).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "trace")
                .count(),
            1
        );
        let failed = events
            .into_iter()
            .find(|event| event.event_type == "tool.failed")
            .expect("tool.failed");
        assert_eq!(
            failed.payload["error"]["code"],
            if missing_secret {
                "secretUnavailable"
            } else {
                "tool_error"
            }
        );
        assert_eq!(failed.payload["error"]["retryable"], false);
    }
}

#[tokio::test]
async fn mcp_server_down_traces_discovery_once_and_call_is_retryable() {
    let fake = FakeMcpServer::start(false, false, StatusCode::OK).await;
    let url = fake.url.clone();
    drop(fake);
    let provider = ScriptedProvider::new([
        tool_turn(vec![tool_call(
            "mcp-down",
            "mcp",
            "issues_search",
            json!({}),
        )]),
        turn("continued", 1, 1),
    ]);
    let muzen = Muzen::local(LocalRuntimeConfig::memory(provider.clone()).with_loopback_http(true))
        .await
        .expect("runtime");
    let session = mcp_session(&muzen, url, None).await;
    let run = session
        .run(
            input("search"),
            SingleRunOptions {
                limits: mcp_limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    assert_eq!(
        run.wait().await.expect("result").status,
        TerminalRunStatus::Completed
    );
    let events = all_events(&run).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "trace")
            .count(),
        1
    );
    let failed = events
        .iter()
        .find(|event| event.event_type == "tool.failed")
        .expect("tool.failed");
    assert_eq!(failed.payload["error"]["code"], "tool_error");
    assert_eq!(failed.payload["error"]["retryable"], true);
}

#[tokio::test]
async fn single_root_completes_through_public_api() {
    let provider = ScriptedProvider::new([turn("done", 3, 2)]);
    let muzen = memory_runtime(provider).await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("hello"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let result = run.wait().await.expect("durable result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(result.usage.input_tokens, 3);
    assert_eq!(result.usage.output_tokens, 2);
    assert_eq!(run.result().await.expect("result read"), Some(result));
    let events = all_events(&run).await;
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "run.queued",
            "agent.created",
            "run.started",
            "agent.started",
            "model.started",
            "message.accepted",
            "model.completed",
            "agent.completed",
            "run.completed",
        ]
    );
    assert!(events
        .iter()
        .enumerate()
        .all(|(index, event)| event.sequence == index as u64 + 1));
    let messages = session
        .messages(MessagePage::default())
        .await
        .expect("messages");
    assert_eq!(messages.items.len(), 2);
    assert_eq!(messages.items[0].role, MessageRole::User);
    assert_eq!(messages.items[1].role, MessageRole::Assistant);
}

#[tokio::test]
async fn scripted_provider_output_contract_parses_and_rejects_final_turns() {
    let provider = ScriptedProvider::new([turn("{\"summary\":\"done\"}", 3, 2), turn("{}", 3, 1)]);
    let muzen = memory_runtime(provider).await;
    let mut spec = session_spec();
    require_summary(&mut spec);

    let valid = muzen
        .create_session(spec.clone(), CreateOptions::default())
        .await
        .expect("valid session")
        .run(
            input("valid"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("valid run")
        .wait()
        .await
        .expect("valid result");
    assert_eq!(valid.outputs[0].output, Some(json!({ "summary": "done" })));

    let invalid_run = muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("invalid-output session")
        .run(
            input("invalid"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("invalid-output run");
    let invalid = invalid_run.wait().await.expect("invalid result");
    assert_eq!(invalid.outputs[0].status, TerminalAgentStatus::Failed);
    let error = invalid.outputs[0].error.as_ref().expect("schema error");
    assert_eq!(error.code, ExecutionErrorCode::ModelError);
    assert!(!error.retryable);
    assert!(error.message.contains("$.summary"));
    let event_types = all_events(&invalid_run)
        .await
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(event_types.iter().any(|event| event == "model.completed"));
    assert!(event_types.iter().any(|event| event == "agent.failed"));
    assert!(!event_types.iter().any(|event| event == "model.failed"));
}

#[tokio::test]
async fn session_creation_rejects_unsupported_output_schema_keywords() {
    let provider = ScriptedProvider::new([]);
    let muzen = memory_runtime(provider).await;
    let mut spec = session_spec();
    spec.agent.output = Some(OutputContract {
        schema: json!({ "type": "object", "patternProperties": {} }),
        name: None,
    });
    let error = match muzen.create_session(spec, CreateOptions::default()).await {
        Ok(_) => panic!("unsupported schema must fail"),
        Err(error) => error,
    };
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert_eq!(
        error.details().expect("validation details")["path"],
        "agent.output.schema.patternProperties"
    );
}

#[tokio::test]
async fn max_active_agents_one_serializes_multiple_roots() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(20), "one"),
        delayed_turn(Duration::from_millis(20), "two"),
    ]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let result = start_roots(&muzen, &ids, run_limits)
        .await
        .wait()
        .await
        .expect("result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert!(result
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::Completed));
    assert_eq!(provider.max_active(), 1);
}

#[tokio::test]
async fn one_provider_error_makes_multi_root_run_partial() {
    let provider = ScriptedProvider::new([
        Script::Error {
            delay: Duration::ZERO,
            error: ModelProviderError::new("provider unavailable").with_retryable(true),
        },
        turn("recovered", 2, 1),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    let result = run.wait().await.expect("result");
    assert_eq!(result.status, TerminalRunStatus::Partial);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Failed);
    assert_eq!(result.outputs[1].status, TerminalAgentStatus::Completed);
    assert!(all_events(&run)
        .await
        .iter()
        .any(|event| event.event_type == "model.failed"));
}

#[tokio::test]
async fn cancellation_interrupts_model_and_is_durable_and_idempotent() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_secs(5), "late")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    let first = run
        .cancel(CancelOptions {
            reason: Some("operator".to_owned()),
            idempotency_key: None,
        })
        .await
        .expect("cancel");
    assert_eq!(
        run.cancel(CancelOptions::default())
            .await
            .expect("cancel replay"),
        first
    );
    let result = run.wait().await.expect("cancelled result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Cancelled);
    assert!(all_events(&run)
        .await
        .iter()
        .any(|event| event.event_type == "run.cancel_requested"));
}

#[tokio::test]
async fn close_cancels_and_joins_in_flight_runs() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_secs(5), "late")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    muzen.close().await.expect("close runtime");
    let result = run.result().await.expect("result read").expect("result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Cancelled);
}

#[tokio::test]
async fn total_token_limit_marks_agent_budget_exhausted() {
    let provider = ScriptedProvider::new([turn("too expensive", 3, 2)]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_total_tokens = NonZeroU64::new(5);
    let result = start_roots(&muzen, &ids, run_limits)
        .await
        .wait()
        .await
        .expect("result");
    assert_eq!(result.status, TerminalRunStatus::Failed);
    assert_eq!(
        result.outputs[0].status,
        TerminalAgentStatus::BudgetExhausted
    );
    assert_eq!(result.usage.input_tokens + result.usage.output_tokens, 5);
    assert_eq!(
        result.outputs[0].error.as_ref().expect("error").message,
        "run token budget exhausted"
    );
}

#[tokio::test]
async fn deadline_records_cancellation_intent_and_cancelled_result() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_secs(5), "late")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.deadline_ms = NonZeroU64::new(20);
    let run = start_roots(&muzen, &ids, run_limits).await;
    let result = run.wait().await.expect("deadline result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    let events = all_events(&run).await;
    let cancel = events
        .iter()
        .find(|event| event.event_type == "run.cancel_requested")
        .expect("deadline cancellation event");
    assert_eq!(cancel.payload.get("reason"), Some(&json!("deadline")));
}

#[tokio::test]
async fn event_tail_after_mid_run_sequence_has_no_gap_or_duplicate() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(50), "done")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    let started = wait_for_event(&run, "model.started").await;
    assert_eq!(started.sequence, 5);
    let tail = run
        .events(EventOptions { after: Some(3) })
        .try_collect::<Vec<_>>()
        .await
        .expect("event tail");
    assert_eq!(
        tail.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        (4..=9).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_events_subscribed_mid_run_reach_terminal_event() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("mid-run-events.db");
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(300), "done")]);
    let runtime = LocalRuntime::connect(LocalRuntimeConfig::sqlite(provider, &path))
        .await
        .expect("SQLite local runtime");
    let session_id = runtime
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run_id = runtime
        .start_run(RunSpec {
            roots: vec![RunRoot::Existing(ExistingSessionRoot {
                session_id,
                input: input("live SQLite events"),
            })],
            limits: limits(),
            idempotency_key: None,
            metadata: Default::default(),
        })
        .await
        .expect("run");

    let events = tokio::time::timeout(
        Duration::from_secs(2),
        runtime
            .events(&run_id, EventOptions::default())
            .try_collect::<Vec<_>>(),
    )
    .await
    .expect("mid-run SQLite event stream reached terminal state")
    .expect("events");
    assert_eq!(events.first().expect("first event").sequence, 1);
    assert_eq!(
        events.last().expect("terminal event").event_type,
        "run.completed"
    );
    runtime.close().await.expect("close runtime");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_slow_event_read_does_not_block_concurrent_result_pollers() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("slow-events-concurrent-runs.db");
    let provider = GatedDelayedProvider::new();
    let runtime = Arc::new(
        LocalRuntime::connect(LocalRuntimeConfig::sqlite(provider.clone(), &path))
            .await
            .expect("SQLite local runtime"),
    );
    let external_database = Builder::new_local(&path)
        .build()
        .await
        .expect("external database");
    let external_connection = external_database.connect().expect("external connection");

    let sessions = join_all((0..5).map(|_| {
        let runtime = Arc::clone(&runtime);
        async move {
            runtime
                .create_session(session_spec(), CreateOptions::default())
                .await
                .expect("session")
        }
    }))
    .await;
    let run_ids = join_all(sessions.into_iter().enumerate().map(|(index, session_id)| {
        let runtime = Arc::clone(&runtime);
        async move {
            runtime
                .start_run(RunSpec {
                    roots: vec![RunRoot::Existing(ExistingSessionRoot {
                        session_id,
                        input: input(&format!("concurrent run {index}")),
                    })],
                    limits: limits(),
                    idempotency_key: None,
                    metadata: Default::default(),
                })
                .await
                .expect("run")
        }
    }))
    .await;

    tokio::time::timeout(Duration::from_secs(10), provider.wait_started(5))
        .await
        .expect("all five provider calls became active");
    let writer = external_connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .expect("external writer transaction");
    let mut events = runtime.events(&run_ids[0], EventOptions::default());
    while events
        .next()
        .await
        .expect("model.started event")
        .expect("event")
        .event_type
        != "model.started"
    {}
    assert!(runtime.inner.store.arm_append_activity_probe());
    provider.release(5);
    tokio::time::timeout(
        Duration::from_secs(10),
        runtime.inner.store.wait_append_activity_started(),
    )
    .await
    .expect("provider completion reached the contended SQLite operation");

    let next_event = events.next();
    futures::pin_mut!(next_event);
    assert!(
        futures::poll!(&mut next_event).is_pending(),
        "event read waits behind the in-flight SQLite writer"
    );
    let (queued_tx, mut queued_rx) = tokio::sync::mpsc::unbounded_channel();
    let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
    let pollers = run_ids
        .iter()
        .cloned()
        .map(|run_id| {
            let runtime = Arc::clone(&runtime);
            let queued_tx = queued_tx.clone();
            let completed_tx = completed_tx.clone();
            tokio::spawn(async move {
                let first_result = runtime.run_result(&run_id);
                futures::pin_mut!(first_result);
                assert!(
                    futures::poll!(&mut first_result).is_pending(),
                    "result request waits behind the contended SQLite operation"
                );
                queued_tx.send(()).expect("queued result request");
                if first_result.await.expect("poll run result").is_some() {
                    completed_tx.send(()).expect("poll completion");
                    return;
                }
                loop {
                    if runtime
                        .run_result(&run_id)
                        .await
                        .expect("poll run result")
                        .is_some()
                    {
                        completed_tx.send(()).expect("poll completion");
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        })
        .collect::<Vec<_>>();
    drop(queued_tx);
    drop(completed_tx);
    tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..5 {
            queued_rx.recv().await.expect("queued result request");
        }
    })
    .await
    .expect("all result requests were polled and queued");
    writer.commit().await.expect("release external writer");

    let completed_while_subscriber_paused = tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..5 {
            completed_rx.recv().await.expect("result poller completion");
        }
    })
    .await
    .is_ok();
    tokio::time::timeout(Duration::from_secs(10), next_event)
        .await
        .expect("slow event read resumed")
        .expect("slow event after pause")
        .expect("event");
    let pollers = tokio::time::timeout(Duration::from_secs(10), join_all(pollers))
        .await
        .expect("result pollers stopped");
    for poller in pollers {
        poller.expect("result poller task");
    }
    runtime.close().await.expect("close runtime");
    assert!(
        completed_while_subscriber_paused,
        "an unpolled event read must not hold the SQLite queue ahead of result requests"
    );
}

#[tokio::test]
async fn finished_run_replay_after_middle_delivers_terminal_event() {
    let provider = ScriptedProvider::new([turn("done", 2, 1)]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    run.wait().await.expect("result");
    let tail = run
        .events(EventOptions { after: Some(5) })
        .try_collect::<Vec<_>>()
        .await
        .expect("finished event tail");
    assert_eq!(
        tail.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        (6..=9).collect::<Vec<_>>()
    );
    assert_eq!(
        tail.last().expect("terminal event").event_type,
        "run.completed"
    );
}

#[tokio::test]
async fn parked_subscriber_drains_tail_after_terminal_state_cleanup() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(50), "done")]);
    let runtime = LocalRuntime::connect(LocalRuntimeConfig::memory(provider))
        .await
        .expect("local runtime");
    let session_id = runtime
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run_id = runtime
        .start_run(RunSpec {
            roots: vec![RunRoot::Existing(ExistingSessionRoot {
                session_id,
                input: input("park"),
            })],
            limits: limits(),
            idempotency_key: None,
            metadata: Default::default(),
        })
        .await
        .expect("run");
    let mut prefix = runtime.events(&run_id, EventOptions::default());
    while let Some(event) = prefix.next().await {
        if event.expect("prefix event").event_type == "model.started" {
            break;
        }
    }
    drop(prefix);
    let tail = runtime
        .events(&run_id, EventOptions { after: Some(5) })
        .try_collect::<Vec<_>>()
        .await
        .expect("parked event tail");
    assert_eq!(
        tail.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        (6..=9).collect::<Vec<_>>()
    );
    assert_eq!(
        tail.last().expect("terminal event").event_type,
        "run.completed"
    );
    assert!(runtime.inner.notifications.lock().is_empty());
    assert!(runtime.inner.scheduled.lock().is_empty());
    assert!(runtime.inner.tasks.lock().is_empty());
}

#[tokio::test]
async fn follow_up_extends_run_and_accumulates_usage() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "first"),
        turn("second", 4, 2),
    ]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    run.send(SendCommand {
        session_id: ids[0].clone(),
        input: input("continue"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: None,
    })
    .await
    .expect("accept follow-up");
    let result = run.wait().await.expect("extended result");
    assert_eq!(result.usage.input_tokens, 6);
    assert_eq!(result.usage.output_tokens, 3);
    assert_eq!(provider.requests().len(), 2);
    let second = &provider.requests()[1].transcript;
    assert_eq!(second.len(), 3);
    assert_eq!(second[0].role, MessageRole::User);
    assert_eq!(second[1].role, MessageRole::Assistant);
    assert_eq!(second[2].role, MessageRole::User);
    assert_eq!(second[2].content, input("continue").content);
    let events = all_events(&run).await;
    assert!(events
        .iter()
        .any(|event| event.event_type == "agent.waiting"));
    assert!(events.iter().any(|event| event.event_type == "run.waiting"));
    assert!(events
        .iter()
        .enumerate()
        .all(|(index, event)| event.sequence == index as u64 + 1));
}

#[tokio::test]
async fn steer_mid_model_call_is_delivered_after_assistant_before_next_request() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "first"),
        turn("steered", 1, 1),
    ]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    run.send(SendCommand {
        session_id: ids[0].clone(),
        input: input("steer now"),
        delivery: MessageDelivery::Steer,
        idempotency_key: None,
    })
    .await
    .expect("accept steer");
    run.wait().await.expect("steered result");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].transcript.len(), 3);
    assert_eq!(requests[1].transcript[1].role, MessageRole::Assistant);
    assert_eq!(
        requests[1].transcript[2].content,
        input("steer now").content
    );
}

#[tokio::test]
async fn send_errors_and_idempotency_follow_command_contract() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "first"),
        turn("second", 1, 1),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let run = start_roots(&muzen, &ids[..1], limits()).await;
    wait_for_event(&run, "model.started").await;
    let key = IdempotencyKey::new("send-key").expect("key");
    let command = SendCommand {
        session_id: ids[0].clone(),
        input: input("again"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: Some(key.clone()),
    };
    let receipt = run.send(command.clone()).await.expect("first send");
    assert_eq!(run.send(command).await.expect("replay send"), receipt);
    let mut changed = SendCommand {
        session_id: ids[0].clone(),
        input: input("different"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: Some(key),
    };
    assert_eq!(
        run.send(changed.clone())
            .await
            .expect_err("changed replay")
            .code(),
        ErrorCode::Conflict
    );
    changed.idempotency_key = None;
    changed.session_id = ids[1].clone();
    assert_eq!(
        run.send(changed)
            .await
            .expect_err("untracked target")
            .code(),
        ErrorCode::NotFound
    );
    run.wait().await.expect("result");
    assert_eq!(
        run.send(SendCommand {
            session_id: ids[0].clone(),
            input: input("late"),
            delivery: MessageDelivery::FollowUp,
            idempotency_key: None
        })
        .await
        .expect_err("terminal send")
        .code(),
        ErrorCode::Conflict
    );
}

#[tokio::test]
async fn spawn_creates_ordered_child_and_run_waits_for_it() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "parent"),
        turn("child", 2, 1),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    run_limits.max_depth = 1;
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    let child = run
        .spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("child input"),
            idempotency_key: Some(IdempotencyKey::new("spawn-key").expect("key")),
        })
        .await
        .expect("spawn child");
    let replay = run
        .spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("child input"),
            idempotency_key: Some(IdempotencyKey::new("spawn-key").expect("key")),
        })
        .await
        .expect("spawn replay");
    assert_eq!(child.id(), replay.id());
    assert_eq!(
        run.spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("different child input"),
            idempotency_key: Some(IdempotencyKey::new("spawn-key").expect("key")),
        })
        .await
        .err()
        .expect("changed spawn replay")
        .code(),
        ErrorCode::Conflict
    );
    let snapshot = run.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.agents.len(), 2);
    assert_eq!(snapshot.agents[1].parent_session_id.as_ref(), Some(&ids[0]));
    assert_eq!(snapshot.agents[1].path, vec![0, 0]);
    let result = run.wait().await.expect("spawned result");
    assert_eq!(result.outputs.len(), 2);
    assert_eq!(result.outputs[1].session_id, *child.id());
    assert_eq!(
        child
            .messages(MessagePage::default())
            .await
            .expect("child messages")
            .items[0]
            .content,
        input("child input").content
    );
    assert_eq!(
        child
            .snapshot()
            .await
            .expect("child snapshot")
            .active_run_id,
        None
    );
}

#[tokio::test]
async fn spawned_child_enforces_its_output_contract() {
    let root_spec = session_spec();
    let root_name = root_spec.agent.name.as_str().to_owned();
    let provider = ScriptedProvider::named([
        (
            root_name,
            vec![delayed_turn(Duration::from_millis(40), "parent done")],
        ),
        ("structured-child".to_owned(), vec![turn("{}", 2, 1)]),
    ]);
    let muzen = memory_runtime(provider).await;
    let root = muzen
        .create_session(root_spec, CreateOptions::default())
        .await
        .expect("root session");
    let mut run_limits = limits();
    run_limits.max_depth = 1;
    let run = root
        .run(
            input("root input"),
            SingleRunOptions {
                limits: run_limits,
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("root run");
    wait_for_event(&run, "model.started").await;

    let mut child_agent = session_spec().agent;
    child_agent.name = AgentName::new("structured-child").expect("child name");
    child_agent.output = Some(OutputContract {
        schema: json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } },
            "required": ["summary"]
        }),
        name: Some("child_output".to_owned()),
    });
    let child = run
        .spawn(SpawnCommand {
            parent_session_id: root.id().clone(),
            agent: child_agent,
            input: input("child input"),
            idempotency_key: None,
        })
        .await
        .expect("spawn child");

    let result = run.wait().await.expect("partial result");
    assert_eq!(result.status, TerminalRunStatus::Partial);
    let child_output = result
        .outputs
        .iter()
        .find(|output| output.session_id == *child.id())
        .expect("child output");
    assert_eq!(child_output.status, TerminalAgentStatus::Failed);
    let error = child_output.error.as_ref().expect("child schema error");
    assert_eq!(error.code, ExecutionErrorCode::ModelError);
    assert!(error.message.contains("$.summary"));
}

#[tokio::test]
async fn spawn_limits_and_authority_fail_without_creating_child() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(80), "parent")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_agents = NonZeroU32::new(1).expect("limit");
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    let command = SpawnCommand {
        parent_session_id: ids[0].clone(),
        agent: session_spec().agent,
        input: input("child"),
        idempotency_key: None,
    };
    assert_eq!(
        run.spawn(command).await.err().expect("max agents").code(),
        ErrorCode::ResourceExhausted
    );
    assert_eq!(run.snapshot().await.expect("snapshot").agents.len(), 1);
    run.cancel(CancelOptions::default()).await.expect("cancel");
    run.wait().await.expect("cancelled");

    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(80), "parent")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    assert_eq!(
        run.spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("child"),
            idempotency_key: None
        })
        .await
        .err()
        .expect("max depth")
        .code(),
        ErrorCode::ResourceExhausted
    );
    assert_eq!(run.snapshot().await.expect("snapshot").agents.len(), 1);
    run.cancel(CancelOptions::default()).await.expect("cancel");
    run.wait().await.expect("cancelled");

    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(80), "parent")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut authority_limits = limits();
    authority_limits.max_depth = 1;
    let run = start_roots(&muzen, &ids, authority_limits).await;
    wait_for_event(&run, "model.started").await;
    let mut agent = session_spec().agent;
    agent.model = ModelProfileId::new("outside").expect("model id");
    assert_eq!(
        run.spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent,
            input: input("child"),
            idempotency_key: None
        })
        .await
        .err()
        .expect("model authority")
        .code(),
        ErrorCode::PermissionDenied
    );
    assert_eq!(run.snapshot().await.expect("snapshot").agents.len(), 1);
    run.cancel(CancelOptions::default()).await.expect("cancel");
    run.wait().await.expect("cancelled");
}

#[tokio::test]
async fn failed_child_makes_completed_root_run_partial() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "parent"),
        Script::Error {
            delay: Duration::ZERO,
            error: ModelProviderError::new("child failed"),
        },
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    run_limits.max_depth = 1;
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    run.spawn(SpawnCommand {
        parent_session_id: ids[0].clone(),
        agent: session_spec().agent,
        input: input("child"),
        idempotency_key: None,
    })
    .await
    .expect("spawn child");
    let result = run.wait().await.expect("partial result");
    assert_eq!(result.status, TerminalRunStatus::Partial);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Completed);
    assert_eq!(result.outputs[1].status, TerminalAgentStatus::Failed);
}

#[tokio::test]
async fn cancellation_interrupts_agent_waiting_on_follow_up() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(30), "first"),
        delayed_turn(Duration::from_secs(5), "second root"),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    run.send(SendCommand {
        session_id: ids[0].clone(),
        input: input("follow"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: None,
    })
    .await
    .expect("follow-up");
    wait_for_event(&run, "agent.waiting").await;
    assert_eq!(
        run.snapshot().await.expect("waiting snapshot").agents[0].status,
        AgentStatus::Waiting
    );
    run.cancel(CancelOptions::default()).await.expect("cancel");
    let result = run.wait().await.expect("cancelled result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    assert!(result
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::Cancelled));
}

#[tokio::test]
async fn send_to_terminal_agent_in_live_run_conflicts() {
    let provider = ScriptedProvider::new([
        turn("first done", 1, 1),
        delayed_turn(Duration::from_secs(5), "second late"),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "agent.completed").await;
    assert_eq!(
        run.send(SendCommand {
            session_id: ids[0].clone(),
            input: input("too late"),
            delivery: MessageDelivery::FollowUp,
            idempotency_key: None,
        })
        .await
        .expect_err("terminal agent send")
        .code(),
        ErrorCode::Conflict
    );
    run.cancel(CancelOptions::default()).await.expect("cancel");
    let result = run.wait().await.expect("partial result");
    assert_eq!(result.status, TerminalRunStatus::Partial);
}

#[tokio::test]
async fn model_tool_spawn_creates_and_runs_path_ordered_child() {
    let mut root_spec = session_spec();
    grant(
        &mut root_spec,
        "builtin",
        "agent.spawn",
        Some(ToolEffect::AgentSpawn),
    );
    let mut child_agent = root_spec.agent.clone();
    child_agent.name = AgentName::new("child").expect("agent name");
    let provider = ScriptedProvider::named([
        (
            "builder".to_owned(),
            vec![
                tool_turn(vec![tool_call(
                    "spawn-1",
                    "builtin",
                    "agent.spawn",
                    json!({ "agent": child_agent, "input": input("child input") }),
                )]),
                turn("parent done", 1, 1),
            ],
        ),
        ("child".to_owned(), vec![turn("child done", 1, 1)]),
    ]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let session = muzen
        .create_session(root_spec, CreateOptions::default())
        .await
        .expect("root session");
    let mut run_limits = limits();
    run_limits.max_depth = 1;
    run_limits.max_total_tool_calls = None;
    let run = session
        .run(
            input("root input"),
            SingleRunOptions {
                limits: run_limits,
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let result = run.wait().await.expect("result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(result.outputs.len(), 2);
    assert_eq!(result.outputs[0].path, vec![0]);
    assert_eq!(result.outputs[1].path, vec![0, 0]);
    assert_eq!(
        result.outputs[1].session_id,
        run.snapshot().await.expect("snapshot").agents[1].session_id
    );
    let events = all_events(&run).await;
    for event_type in [
        "tool.started",
        "tool.completed",
        "agent.created",
        "agent.started",
        "agent.completed",
    ] {
        assert!(events.iter().any(|event| event.event_type == event_type));
    }
    let requests = provider.requests();
    let parent_second = requests
        .iter()
        .find(|request| {
            request.agent.name.as_str() == "builder"
                && request
                    .transcript
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
        })
        .expect("parent second request");
    let tool_message = parent_second
        .transcript
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("tool result");
    let ContentBlock::Text { text } = &tool_message.content[0] else {
        panic!("tool result text")
    };
    let envelope: Value = serde_json::from_str(text).expect("tool envelope");
    assert_eq!(envelope["callId"], "spawn-1");
    assert!(envelope["result"].is_string());
}

#[tokio::test]
async fn model_tool_message_follow_up_extends_target() {
    let provider = ScriptedProvider::new([]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let mut sender_spec = session_spec();
    sender_spec.agent.name = AgentName::new("sender").expect("name");
    grant(
        &mut sender_spec,
        "builtin",
        "agent.message",
        Some(ToolEffect::AgentMessage),
    );
    let mut target_spec = session_spec();
    target_spec.agent.name = AgentName::new("target").expect("name");
    let sender = muzen
        .create_session(sender_spec, CreateOptions::default())
        .await
        .expect("sender");
    let target = muzen
        .create_session(target_spec, CreateOptions::default())
        .await
        .expect("target");
    *provider.named_scripts.lock() = BTreeMap::from([
        (
            "sender".to_owned(),
            vec![
                tool_turn(vec![tool_call(
                    "message-1",
                    "builtin",
                    "agent.message",
                    json!({
                        "sessionId": target.id(),
                        "input": input("from sender"),
                        "delivery": "follow_up"
                    }),
                )]),
                turn("sender done", 1, 1),
            ]
            .into(),
        ),
        (
            "target".to_owned(),
            vec![
                delayed_turn(Duration::from_millis(30), "target first"),
                turn("target second", 1, 1),
            ]
            .into(),
        ),
    ]);
    let mut run_limits = limits();
    run_limits.max_total_tool_calls = None;
    let run = start_roots(
        &muzen,
        &[sender.id().clone(), target.id().clone()],
        run_limits,
    )
    .await;
    assert_eq!(
        run.wait().await.expect("result").status,
        TerminalRunStatus::Completed
    );
    let messages = target
        .messages(MessagePage::default())
        .await
        .expect("target messages");
    assert!(messages
        .items
        .iter()
        .any(|message| message.content == input("from sender").content));
    assert_eq!(
        provider
            .requests()
            .iter()
            .filter(|request| request.agent.name.as_str() == "target")
            .count(),
        2
    );
}

#[tokio::test]
async fn tool_authority_and_unsupported_failures_return_results_and_continue() {
    for case in ["missing_grant", "missing_effect", "mcp"] {
        let provider = ScriptedProvider::new([]);
        let muzen = memory_runtime(Arc::clone(&provider)).await;
        let mut spec = session_spec();
        let (provider_id, tool_name, arguments) = match case {
            "missing_grant" => ("builtin", "agent.absent", json!({})),
            "missing_effect" => {
                grant(&mut spec, "builtin", "agent.message", None);
                ("builtin", "agent.message", Value::Null)
            }
            "mcp" => {
                grant(
                    &mut spec,
                    "mcp",
                    "issues.search",
                    Some(ToolEffect::NetworkRead),
                );
                ("mcp", "issues.search", json!({ "query": "x" }))
            }
            _ => unreachable!(),
        };
        let session = muzen
            .create_session(spec, CreateOptions::default())
            .await
            .expect("session");
        *provider.scripts.lock() = vec![
            tool_turn(vec![tool_call(
                "authority-1",
                provider_id,
                tool_name,
                arguments,
            )]),
            turn("continued", 1, 1),
        ]
        .into();
        let mut run_limits = limits();
        run_limits.max_total_tool_calls = None;
        let run = session
            .run(
                input("go"),
                SingleRunOptions {
                    limits: run_limits,
                    idempotency_key: None,
                    metadata: Default::default(),
                },
            )
            .await
            .expect("run");
        let result = run.wait().await.expect("result");
        assert_eq!(result.status, TerminalRunStatus::Completed, "{case}");
        assert_eq!(result.usage.tool_calls, u64::from(case == "mcp"), "{case}");
        let events = all_events(&run).await;
        assert!(events.iter().any(|event| event.event_type == "tool.failed"));
        let requests = provider.requests();
        assert!(requests[1]
            .transcript
            .iter()
            .any(|message| message.role == MessageRole::Tool));
    }
}

#[tokio::test]
async fn tool_grant_rejection_continues_but_agent_budget_terminates() {
    for grant_limit in [true, false] {
        let provider = ScriptedProvider::new([]);
        let muzen = memory_runtime(Arc::clone(&provider)).await;
        let mut spec = session_spec();
        grant(
            &mut spec,
            "builtin",
            "agent.message",
            Some(ToolEffect::AgentMessage),
        );
        let tool_grant = spec
            .agent
            .tools
            .iter_mut()
            .find(|grant| grant.tool == "agent.message")
            .expect("grant");
        if grant_limit {
            tool_grant.max_calls = NonZeroU32::new(1);
        } else {
            spec.agent.budget.as_mut().expect("budget").max_tool_calls = 1;
        }
        let session = muzen
            .create_session(spec, CreateOptions::default())
            .await
            .expect("session");
        let call = |id: &str| {
            tool_call(
                id,
                "builtin",
                "agent.message",
                json!({
                    "sessionId": session.id(),
                    "input": { "content": "again" },
                    "delivery": "steer"
                }),
            )
        };
        let mut scripts = vec![tool_turn(vec![call("one"), call("two")])];
        if grant_limit {
            scripts.push(turn("continued", 1, 1));
        }
        *provider.scripts.lock() = scripts.into();
        let mut run_limits = limits();
        run_limits.max_total_tool_calls = None;
        let run = session
            .run(
                input("go"),
                SingleRunOptions {
                    limits: run_limits,
                    idempotency_key: None,
                    metadata: Default::default(),
                },
            )
            .await
            .expect("run");
        let result = run.wait().await.expect("result");
        let output = &result.outputs[0];
        if grant_limit {
            assert_eq!(output.status, TerminalAgentStatus::Completed);
            assert_eq!(output.output, Some(json!("continued")));
            let events = all_events(&run).await;
            let failed = events
                .iter()
                .find(|event| event.event_type == "tool.failed")
                .expect("grant failure event");
            assert_eq!(
                failed.payload["error"]["message"],
                "tool grant maxCalls exhausted"
            );
            assert!(provider.requests()[1].transcript.iter().any(|message| {
                message.role == MessageRole::Tool
                    && serde_json::to_string(&message.content)
                        .expect("tool result JSON")
                        .contains("tool grant maxCalls exhausted")
            }));
        } else {
            assert_eq!(output.status, TerminalAgentStatus::BudgetExhausted);
            assert_eq!(
                output.error.as_ref().expect("error").message,
                "agent maxToolCalls exhausted"
            );
        }
        assert_eq!(result.usage.tool_calls, 1);
    }
}

#[tokio::test]
async fn run_tool_budget_exhaustion_marks_every_live_agent() {
    let provider = ScriptedProvider::new([]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let mut first_spec = session_spec();
    first_spec.agent.name = AgentName::new("first").expect("name");
    grant(
        &mut first_spec,
        "builtin",
        "agent.message",
        Some(ToolEffect::AgentMessage),
    );
    let mut second_spec = session_spec();
    second_spec.agent.name = AgentName::new("second").expect("name");
    let first = muzen
        .create_session(first_spec, CreateOptions::default())
        .await
        .expect("first");
    let second = muzen
        .create_session(second_spec, CreateOptions::default())
        .await
        .expect("second");
    let call = |id: &str| {
        tool_call(
            id,
            "builtin",
            "agent.message",
            json!({
                "sessionId": second.id(),
                "input": input("hold"),
                "delivery": "follow_up"
            }),
        )
    };
    *provider.named_scripts.lock() = BTreeMap::from([
        (
            "first".to_owned(),
            vec![tool_turn(vec![call("one"), call("two")])].into(),
        ),
        (
            "second".to_owned(),
            vec![delayed_turn(Duration::from_secs(5), "late")].into(),
        ),
    ]);
    let mut run_limits = limits();
    run_limits.max_total_tool_calls = Some(1);
    let result = start_roots(
        &muzen,
        &[first.id().clone(), second.id().clone()],
        run_limits,
    )
    .await
    .wait()
    .await
    .expect("result");
    assert_eq!(result.status, TerminalRunStatus::Failed);
    assert!(result
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::BudgetExhausted));
    assert!(result.outputs.iter().all(|output| {
        output
            .error
            .as_ref()
            .is_some_and(|error| error.message == "run maxTotalToolCalls exhausted")
    }));
    assert_eq!(result.usage.tool_calls, 1);
}

#[tokio::test]
async fn self_steer_from_tool_batch_precedes_next_model_request() {
    let provider = ScriptedProvider::new([]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let mut spec = session_spec();
    grant(
        &mut spec,
        "builtin",
        "agent.message",
        Some(ToolEffect::AgentMessage),
    );
    let session = muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("session");
    *provider.scripts.lock() = vec![
        tool_turn(vec![tool_call(
            "steer-1",
            "builtin",
            "agent.message",
            json!({
                "sessionId": session.id(),
                "input": input("steered in batch"),
                "delivery": "steer"
            }),
        )]),
        turn("done", 1, 1),
    ]
    .into();
    let mut run_limits = limits();
    run_limits.max_total_tool_calls = None;
    session
        .run(
            input("go"),
            SingleRunOptions {
                limits: run_limits,
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run")
        .wait()
        .await
        .expect("result");
    let requests = provider.requests();
    let transcript = &requests[1].transcript;
    let tool_index = transcript
        .iter()
        .position(|message| message.role == MessageRole::Tool)
        .expect("tool result");
    let steer_index = transcript
        .iter()
        .position(|message| message.content == input("steered in batch").content)
        .expect("steer");
    assert!(tool_index < steer_index);
}

#[tokio::test]
async fn cancel_during_tool_batch_stops_durable_tool_events_at_cancel_boundary() {
    let provider = ScriptedProvider::new([]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let mut spec = session_spec();
    grant(
        &mut spec,
        "builtin",
        "agent.message",
        Some(ToolEffect::AgentMessage),
    );
    let session = muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("session");
    let call = |id: &str| {
        tool_call(
            id,
            "builtin",
            "agent.message",
            json!({
                "sessionId": session.id(),
                "input": input("later"),
                "delivery": "follow_up"
            }),
        )
    };
    *provider.scripts.lock() = vec![tool_turn(vec![call("first"), call("second")])].into();
    let mut run_limits = limits();
    run_limits.max_total_tool_calls = None;
    let run = session
        .run(
            input("go"),
            SingleRunOptions {
                limits: run_limits,
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    wait_for_event(&run, "tool.completed").await;
    run.cancel(CancelOptions::default()).await.expect("cancel");
    let result = run.wait().await.expect("result");
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Cancelled);
    let events = all_events(&run).await;
    let cancel_sequence = events
        .iter()
        .find(|event| event.event_type == "run.cancel_requested")
        .expect("cancel event")
        .sequence;
    assert!(!events.iter().any(|event| {
        event.sequence > cancel_sequence && event.event_type.starts_with("tool.")
    }));
}

#[tokio::test]
async fn sqlite_reopen_preserves_tool_result_transcript() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("tool-runtime.db");
    let provider = ScriptedProvider::new([
        tool_turn(vec![tool_call(
            "mcp-1",
            "mcp",
            "issues.search",
            json!({ "query": "durable" }),
        )]),
        turn("done", 1, 1),
    ]);
    let muzen = sqlite_runtime(&path, provider).await;
    let mut spec = session_spec();
    grant(
        &mut spec,
        "mcp",
        "issues.search",
        Some(ToolEffect::NetworkRead),
    );
    let session = muzen
        .create_session(spec, CreateOptions::default())
        .await
        .expect("session");
    let session_id = session.id().clone();
    let mut run_limits = limits();
    run_limits.max_total_tool_calls = None;
    session
        .run(
            input("persist tools"),
            SingleRunOptions {
                limits: run_limits,
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run")
        .wait()
        .await
        .expect("result");
    muzen.close().await.expect("close");
    let reopened = sqlite_runtime(&path, ScriptedProvider::new([])).await;
    let messages = reopened
        .get_session(&session_id)
        .await
        .expect("session")
        .messages(MessagePage::default())
        .await
        .expect("messages");
    assert!(messages
        .items
        .iter()
        .any(|message| message.role == MessageRole::Tool));
}

#[tokio::test]
async fn sqlite_reopen_preserves_result_events_and_messages() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("local-runtime.db");
    let provider = ScriptedProvider::new([turn("durable", 2, 1)]);
    let muzen = sqlite_runtime(&path, provider).await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let session_id = session.id().clone();
    let run = session
        .run(
            input("persist"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let run_id = run.id().clone();
    run.wait().await.expect("result");
    muzen.close().await.expect("close runtime");

    let reopened = sqlite_runtime(&path, ScriptedProvider::new([])).await;
    let run = reopened.get_run(&run_id).await.expect("reopened run");
    assert_eq!(
        run.result()
            .await
            .expect("persisted result")
            .expect("result")
            .status,
        TerminalRunStatus::Completed
    );
    assert_eq!(all_events(&run).await.len(), 9);
    let session = reopened
        .get_session(&session_id)
        .await
        .expect("reopened session");
    assert_eq!(
        session
            .messages(MessagePage::default())
            .await
            .expect("persisted messages")
            .items
            .len(),
        2
    );
}

async fn sqlite_runtime(path: &Path, provider: Arc<ScriptedProvider>) -> Muzen {
    Muzen::local(LocalRuntimeConfig::sqlite(provider, path))
        .await
        .expect("SQLite local runtime")
}
