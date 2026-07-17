use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use futures::{future::join_all, stream, StreamExt, TryStreamExt};
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::net::TcpListener;

use super::{router, HttpServiceConfig, HttpTransportOptions};
use crate::agent_runtime::{
    AgentInput, AgentMessage, AnswerToolCallInput, AnswerToolCallOutcome, ArtifactChunk,
    ArtifactId, CancelOptions, Capabilities, CommandOptions, CommandReceipt, ContentBlock,
    CreateOptions, ErrorCode, EventOptions, EventStream, ExistingSessionRoot, IdempotencyKey,
    LocalRuntime, LocalRuntimeConfig, LocalStoreConfig, MessageDelivery, MessagePage,
    ModelProvider, ModelProviderError, ModelRequest, ModelStop, ModelToolCall, ModelTurn, Muzen,
    MuzenError, Page, PutSecretInput, Run, RunId, RunLimits, RunResult, RunRoot, RunSnapshot,
    RunSpec, RunStatus, RuntimeTransport, SecretRef, SendCommand, SessionId, SessionSnapshot,
    SessionSpec, SingleRunOptions, SpawnCommand, TerminalRunStatus, ToolEffect, ToolGrant,
    ToolProvider, ToolProviderId, Usage,
};

struct ScriptedProvider {
    turns: Mutex<VecDeque<(Duration, ModelTurn)>>,
}

impl ScriptedProvider {
    fn new(turns: impl IntoIterator<Item = (Duration, &'static str)>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(
                turns
                    .into_iter()
                    .map(|(delay, text)| {
                        (
                            delay,
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
                            },
                        )
                    })
                    .collect(),
            ),
        })
    }

    fn from_turns(turns: impl IntoIterator<Item = ModelTurn>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(
                turns
                    .into_iter()
                    .map(|turn| (Duration::ZERO, turn))
                    .collect(),
            ),
        })
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelTurn, ModelProviderError> {
        let (delay, turn) = self.turns.lock().pop_front().expect("scripted turn");
        tokio::time::sleep(delay).await;
        Ok(turn)
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
        max_active_agents: NonZeroU32::new(4).unwrap(),
        max_agents: NonZeroU32::new(4).unwrap(),
        max_depth: 1,
        max_input_bytes: NonZeroU64::new(1024).unwrap(),
        max_total_tokens: None,
        max_total_tool_calls: Some(0),
        deadline_ms: None,
    }
}

fn client_session_spec(timeout_ms: u64) -> SessionSpec {
    let mut spec = session_spec();
    spec.agent.tools = vec![ToolGrant {
        provider: ToolProviderId::new("client").expect("provider"),
        tool: "lookup_issue".to_owned(),
        description: None,
        input_schema: None,
        effects: vec![ToolEffect::NetworkRead],
        max_calls: None,
    }];
    spec.tool_providers = vec![ToolProvider::Client {
        id: ToolProviderId::new("client").expect("provider"),
        timeout_ms: NonZeroU64::new(timeout_ms),
    }];
    spec
}

fn client_tool_turn(call_id: impl Into<String>) -> ModelTurn {
    ModelTurn {
        content: Vec::new(),
        tool_calls: vec![ModelToolCall {
            id: call_id.into(),
            provider: ToolProviderId::new("client").expect("provider"),
            name: "lookup_issue".to_owned(),
            arguments: serde_json::json!({ "query": "runtime" }),
        }],
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            tool_calls: 0,
        },
        stop: ModelStop::ToolUse,
    }
}

fn completed_turn() -> ModelTurn {
    ModelTurn {
        content: vec![ContentBlock::Text {
            text: "done".to_owned(),
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

fn client_limits() -> RunLimits {
    let mut value = limits();
    value.max_total_tool_calls = Some(1);
    value
}

async fn start_server(
    provider: Arc<dyn ModelProvider>,
    config: HttpServiceConfig,
) -> (String, Arc<LocalRuntime>, tokio::task::JoinHandle<()>) {
    start_server_with_store(provider, config, LocalStoreConfig::Memory).await
}

async fn start_server_with_store(
    provider: Arc<dyn ModelProvider>,
    config: HttpServiceConfig,
    store: LocalStoreConfig,
) -> (String, Arc<LocalRuntime>, tokio::task::JoinHandle<()>) {
    let runtime = Arc::new(
        LocalRuntime::connect(LocalRuntimeConfig {
            provider: Some(provider),
            store,
            close_timeout: Duration::from_secs(5),
            allow_loopback_http: false,
        })
        .await
        .expect("runtime"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let app = router(runtime.clone(), config);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{address}"), runtime, task)
}

async fn exercise_concurrent_run_stress(base: &str) {
    let muzen = Muzen::http(base, HttpTransportOptions::default()).expect("HTTP client");
    let sessions =
        join_all((0..5).map(|_| muzen.create_session(session_spec(), CreateOptions::default())))
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("sessions");
    let starts = sessions.into_iter().enumerate().map(|(index, session)| {
        tokio::spawn(async move {
            session
                .run(
                    input(&format!("concurrent run {index}")),
                    SingleRunOptions {
                        limits: limits(),
                        idempotency_key: None,
                        metadata: BTreeMap::new(),
                    },
                )
                .await
                .expect("start concurrent run")
        })
    });
    let runs = join_all(starts)
        .await
        .into_iter()
        .map(|result| result.expect("start task"))
        .collect::<Vec<_>>();

    let slow_run = runs[0].clone();
    let slow_subscriber = tokio::spawn(async move {
        let mut events = slow_run.events(EventOptions::default());
        let mut terminal = false;
        while let Some(event) = events.next().await {
            let event = event.expect("slow event");
            terminal |= event.event_type == "run.completed";
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(terminal, "slow subscriber reached the terminal event");
    });
    let pollers = runs.into_iter().map(|run: Run| {
        tokio::spawn(async move {
            loop {
                if let Some(result) = run.result().await.expect("poll run result") {
                    assert_eq!(result.status, TerminalRunStatus::Completed);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    });
    for poller in join_all(pollers).await {
        poller.expect("result poller");
    }
    slow_subscriber.await.expect("slow subscriber");
}

async fn concurrent_run_stress(store: LocalStoreConfig) {
    let provider =
        ScriptedProvider::new((0..5).map(|_| (Duration::from_millis(300), "concurrent done")));
    let (base, runtime, server) =
        start_server_with_store(provider, HttpServiceConfig::default(), store).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        exercise_concurrent_run_stress(&base),
    )
    .await;
    server.abort();
    let _ = tokio::time::timeout(Duration::from_secs(1), runtime.close()).await;
    outcome.expect("concurrent runs, result polling, and slow SSE must not wedge");
}

#[derive(Default)]
struct IdleReconnectServerState {
    requests: Mutex<Vec<(Option<String>, Option<String>)>>,
    attempts: AtomicUsize,
}

fn fake_sse_event(sequence: u64, event_type: &str) -> String {
    format!(
        "id: {sequence}\nevent: run.event\ndata: {{\"runId\":\"run-idle\",\"sequence\":{sequence},\"type\":\"{event_type}\",\"timestamp\":\"now\",\"payload\":{{}}}}\n\n"
    )
}

async fn idle_reconnect_events(
    State(state): State<Arc<IdleReconnectServerState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    state.requests.lock().push((
        uri.query().map(str::to_owned),
        headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    ));
    let attempt = state.attempts.fetch_add(1, Ordering::SeqCst);
    let body = if attempt == 0 {
        let first = stream::once(async {
            Ok::<Bytes, Infallible>(Bytes::from(fake_sse_event(1, "run.started")))
        });
        Body::from_stream(first.chain(stream::pending::<Result<Bytes, Infallible>>()))
    } else {
        Body::from(format!(
            "{}{}",
            fake_sse_event(2, "agent.completed"),
            fake_sse_event(3, "run.completed")
        ))
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(body)
        .expect("fake SSE response")
}

#[tokio::test]
async fn http_events_reconnect_after_idle_with_cursor_and_no_duplicates() {
    let state = Arc::new(IdleReconnectServerState::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let app = Router::new()
        .route("/v1/runs/{run_id}/events", get(idle_reconnect_events))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    let transport = super::HttpTransport::new(
        format!("http://{address}"),
        HttpTransportOptions {
            bearer_token: None,
            sse_idle_timeout: Some(Duration::from_millis(100)),
        },
    )
    .expect("HTTP transport");
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        transport
            .events(
                &RunId::new("run-idle").expect("run id"),
                EventOptions::default(),
            )
            .try_collect::<Vec<_>>(),
    )
    .await
    .expect("idle reconnect must be bounded")
    .expect("idle reconnect stays transparent");

    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let requests = state.requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], (None, None));
    assert_eq!(
        requests[1],
        (Some("after=1".to_owned()), Some("1".to_owned()))
    );
    drop(requests);
    server.abort();
}

#[tokio::test]
async fn http_events_connect_timeout_surfaces_unavailable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.expect("accept");
        futures::future::pending::<()>().await;
    });
    let transport = super::HttpTransport::new(
        format!("http://{address}"),
        HttpTransportOptions {
            bearer_token: None,
            sse_idle_timeout: Some(Duration::from_millis(100)),
        },
    )
    .expect("HTTP transport");
    let mut events = transport.events(
        &RunId::new("run-connect-timeout").expect("run id"),
        EventOptions::default(),
    );
    let error = tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .expect("connect timeout must be bounded")
        .expect("stream yields the connect error")
        .expect_err("stalled response headers must fail");
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(error.retryable());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_store_survives_five_concurrent_runs_with_slow_events() {
    concurrent_run_stress(LocalStoreConfig::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_store_survives_five_concurrent_runs_with_slow_events() {
    let directory = tempfile::tempdir().expect("temporary directory");
    concurrent_run_stress(LocalStoreConfig::Sqlite(
        directory.path().join("concurrent-runs.db"),
    ))
    .await;
}

async fn client_tool_http_round_trip(store: LocalStoreConfig) {
    let provider =
        ScriptedProvider::from_turns([client_tool_turn("http-client-call"), completed_turn()]);
    let (base, runtime, server) =
        start_server_with_store(provider, HttpServiceConfig::default(), store).await;
    let muzen = Muzen::http(&base, HttpTransportOptions::default()).expect("HTTP client");
    let session = muzen
        .create_session(client_session_spec(5_000), CreateOptions::default())
        .await
        .expect("client session");
    let run = session
        .run(
            input("client tool"),
            SingleRunOptions {
                limits: client_limits(),
                idempotency_key: None,
                metadata: BTreeMap::new(),
            },
        )
        .await
        .expect("run");

    let mut live = run.events(EventOptions::default());
    let requested = loop {
        let event = live
            .next()
            .await
            .expect("live event")
            .expect("valid live event");
        if event.event_type == "tool.requested" {
            break event;
        }
    };
    drop(live);
    let mut replay = run.events(EventOptions {
        after: Some(requested.sequence - 1),
    });
    let replayed = replay
        .next()
        .await
        .expect("replayed event")
        .expect("valid replayed event");
    assert_eq!(replayed, requested);
    drop(replay);

    muzen
        .answer_tool_call(
            run.id(),
            AnswerToolCallInput {
                call_id: "http-client-call".to_owned(),
                outcome: AnswerToolCallOutcome::Result {
                    result: serde_json::json!({ "source": "client" }),
                },
            },
        )
        .await
        .expect("HTTP tool answer");
    assert_eq!(
        run.wait().await.expect("completed run").status,
        TerminalRunStatus::Completed
    );
    let events = run
        .events(EventOptions::default())
        .try_collect::<Vec<_>>()
        .await
        .expect("all events");
    let requested_index = events
        .iter()
        .position(|event| event.event_type == "tool.requested")
        .expect("tool.requested");
    let completed_index = events
        .iter()
        .position(|event| event.event_type == "tool.completed")
        .expect("tool.completed");
    assert!(requested_index < completed_index);

    server.abort();
    runtime.close().await.expect("runtime close");
}

#[tokio::test]
async fn client_tool_http_round_trip_and_replay_memory_store() {
    client_tool_http_round_trip(LocalStoreConfig::Memory).await;
}

#[tokio::test]
async fn client_tool_http_round_trip_and_replay_sqlite_store() {
    let directory = tempfile::tempdir().expect("temporary directory");
    client_tool_http_round_trip(LocalStoreConfig::Sqlite(
        directory.path().join("client-tool-round-trip.db"),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_client_tools_complete_five_out_of_order_concurrent_runs() {
    let mut turns = (0..5)
        .map(|index| client_tool_turn(format!("concurrent-client-{index}")))
        .collect::<Vec<_>>();
    turns.extend((0..5).map(|_| completed_turn()));
    let provider = ScriptedProvider::from_turns(turns);
    let directory = tempfile::tempdir().expect("temporary directory");
    let (base, runtime, server) = start_server_with_store(
        provider,
        HttpServiceConfig::default(),
        LocalStoreConfig::Sqlite(directory.path().join("client-tool-concurrency.db")),
    )
    .await;
    let muzen = Muzen::http(&base, HttpTransportOptions::default()).expect("HTTP client");
    let sessions = join_all(
        (0..5).map(|_| muzen.create_session(client_session_spec(5_000), CreateOptions::default())),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("sessions");
    let runs = join_all(sessions.into_iter().map(|session| async move {
        session
            .run(
                input("concurrent client tool"),
                SingleRunOptions {
                    limits: client_limits(),
                    idempotency_key: None,
                    metadata: BTreeMap::new(),
                },
            )
            .await
            .expect("run")
    }))
    .await;
    let pending = join_all(runs.iter().cloned().map(|run| async move {
        let mut events = run.events(EventOptions::default());
        loop {
            let event = events
                .next()
                .await
                .expect("pending event")
                .expect("valid pending event");
            if event.event_type == "tool.requested" {
                return (
                    run,
                    event.payload["callId"]
                        .as_str()
                        .expect("call id")
                        .to_owned(),
                );
            }
        }
    }))
    .await;
    for (run, call_id) in pending.iter().rev() {
        muzen
            .answer_tool_call(
                run.id(),
                AnswerToolCallInput {
                    call_id: call_id.clone(),
                    outcome: AnswerToolCallOutcome::Result {
                        result: serde_json::json!({ "callId": call_id }),
                    },
                },
            )
            .await
            .expect("out-of-order answer");
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        for result in join_all(runs.iter().map(Run::wait)).await {
            assert_eq!(
                result.expect("completed concurrent run").status,
                TerminalRunStatus::Completed
            );
        }
    })
    .await
    .expect("concurrent client tools must not deadlock");

    server.abort();
    runtime.close().await.expect("runtime close");
}

#[tokio::test]
async fn lifecycle_idempotency_sse_resume_result_and_messages() {
    let (base, runtime, server) = start_server(
        ScriptedProvider::new([(Duration::from_millis(100), "done")]),
        HttpServiceConfig::default(),
    )
    .await;
    let muzen = Muzen::http(&base, HttpTransportOptions::default()).expect("HTTP client");
    let options = CreateOptions {
        idempotency_key: Some(IdempotencyKey::new("session-key").unwrap()),
    };
    let first = muzen
        .create_session(session_spec(), options.clone())
        .await
        .expect("create");
    let replay = muzen
        .create_session(session_spec(), options)
        .await
        .expect("replay");
    assert_eq!(first.id(), replay.id());

    let run = first
        .run(
            input("review"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: Some(IdempotencyKey::new("run-key").unwrap()),
                metadata: BTreeMap::new(),
            },
        )
        .await
        .expect("run");
    assert_eq!(run.result().await.expect("early result"), None);
    let mut full = run.events(EventOptions::default());
    let first_event = full.next().await.unwrap().expect("first event");
    let resumed = run
        .events(EventOptions {
            after: Some(first_event.sequence),
        })
        .try_collect::<Vec<_>>()
        .await
        .expect("resumed events");
    let remaining = full.try_collect::<Vec<_>>().await.expect("full tail");
    assert_eq!(resumed, remaining);
    assert!(resumed
        .windows(2)
        .all(|events| events[1].sequence == events[0].sequence + 1));
    assert!(resumed
        .last()
        .is_some_and(|event| event.event_type == "run.completed"));
    assert!(run.result().await.expect("result").is_some());
    assert_eq!(
        first
            .messages(MessagePage::default())
            .await
            .expect("messages")
            .items
            .len(),
        2
    );

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{base}/v1/runs/{}/events?after=0", run.id()))
        .header("Last-Event-ID", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: MuzenError = response.json().await.unwrap();
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    server.abort();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn live_send_spawn_and_cancel_cross_http() {
    let (base, runtime, server) = start_server(
        ScriptedProvider::new([
            (Duration::from_millis(200), "parent"),
            (Duration::from_millis(200), "child"),
            (Duration::from_millis(200), "follow-up"),
        ]),
        HttpServiceConfig::default(),
    )
    .await;
    let muzen = Muzen::http(&base, HttpTransportOptions::default()).unwrap();
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .unwrap();
    let run = session
        .run(
            input("start"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let mut events = run.events(EventOptions::default());
    while events.next().await.unwrap().unwrap().event_type != "model.started" {}
    run.send(SendCommand {
        session_id: session.id().clone(),
        input: input("follow up"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: Some(IdempotencyKey::new("send-http").unwrap()),
    })
    .await
    .expect("send");
    let child = run
        .spawn(SpawnCommand {
            parent_session_id: session.id().clone(),
            agent: session_spec().agent,
            input: input("child"),
            idempotency_key: Some(IdempotencyKey::new("spawn-http").unwrap()),
        })
        .await
        .expect("spawn");
    assert_ne!(child.id(), session.id());
    run.cancel(CancelOptions {
        reason: Some("test".to_owned()),
        idempotency_key: Some(IdempotencyKey::new("cancel-http").unwrap()),
    })
    .await
    .expect("cancel");
    assert_eq!(
        run.wait().await.expect("cancelled").status,
        crate::agent_runtime::TerminalRunStatus::Cancelled
    );
    server.abort();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn auth_error_shapes_archive_conflict_and_key_disagreement() {
    let config = HttpServiceConfig {
        bearer_token: Some("secret-token".to_owned()),
        ..HttpServiceConfig::default()
    };
    let (base, runtime, server) = start_server(ScriptedProvider::new([]), config).await;
    let client = reqwest::Client::new();
    for authorization in [None, Some("Bearer wrong")] {
        let mut request = client.get(format!("{base}/v1/capabilities"));
        if let Some(authorization) = authorization {
            request = request.header("Authorization", authorization);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let error: MuzenError = response.json().await.unwrap();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert!(!error.message().is_empty());
    }
    let muzen = Muzen::http(
        &base,
        HttpTransportOptions {
            bearer_token: Some("secret-token".to_owned()),
            ..HttpTransportOptions::default()
        },
    )
    .unwrap();
    let missing = muzen
        .get_run(&RunId::new("missing").unwrap())
        .await
        .err()
        .expect("missing");
    assert_eq!(missing.code(), ErrorCode::NotFound);
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .unwrap();
    session.archive(CommandOptions::default()).await.unwrap();
    assert_eq!(
        session
            .archive(CommandOptions::default())
            .await
            .expect_err("double archive")
            .code(),
        ErrorCode::Conflict
    );
    let body = RunSpec {
        roots: vec![RunRoot::Existing(ExistingSessionRoot {
            session_id: session.id().clone(),
            input: input("x"),
        })],
        limits: limits(),
        idempotency_key: Some(IdempotencyKey::new("body-key").unwrap()),
        metadata: BTreeMap::new(),
    };
    let response = client
        .post(format!("{base}/v1/runs"))
        .bearer_auth("secret-token")
        .header("Idempotency-Key", "header-key")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<MuzenError>().await.unwrap().code(),
        ErrorCode::InvalidInput
    );
    server.abort();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_artifact_is_501_and_keepalives_are_comments() {
    let (base, runtime, server) = start_server(
        ScriptedProvider::new([(Duration::from_millis(100), "done")]),
        HttpServiceConfig {
            keepalive_interval: Duration::from_millis(5),
            ..HttpServiceConfig::default()
        },
    )
    .await;
    let muzen = Muzen::http(&base, HttpTransportOptions::default()).unwrap();
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .unwrap();
    let run = session
        .run(
            input("run"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let mut events = client
        .get(format!("{base}/v1/runs/{}/events", run.id()))
        .send()
        .await
        .unwrap();
    let mut wire = Vec::new();
    while !wire
        .windows(b": keepalive".len())
        .any(|part| part == b": keepalive")
    {
        wire.extend(events.chunk().await.unwrap().expect("SSE chunk"));
    }
    let response = client
        .get(format!("{base}/v1/runs/{}/artifacts/artifact-1", run.id()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        response.json::<MuzenError>().await.unwrap().code(),
        ErrorCode::Unsupported
    );
    run.cancel(CancelOptions::default()).await.unwrap();
    run.wait().await.unwrap();
    server.abort();
    runtime.close().await.unwrap();
}

struct ArtifactRuntime {
    bytes: Vec<u8>,
}

#[async_trait]
impl RuntimeTransport for ArtifactRuntime {
    async fn capabilities(&self) -> Result<Capabilities, MuzenError> {
        unreachable!()
    }
    async fn put_secret(&self, _: PutSecretInput) -> Result<SecretRef, MuzenError> {
        unreachable!()
    }
    async fn delete_secret(&self, _: &SecretRef) -> Result<(), MuzenError> {
        unreachable!()
    }
    async fn create_session(
        &self,
        _: SessionSpec,
        _: CreateOptions,
    ) -> Result<SessionId, MuzenError> {
        unreachable!()
    }
    async fn session_snapshot(&self, _: &SessionId) -> Result<SessionSnapshot, MuzenError> {
        unreachable!()
    }
    async fn messages(
        &self,
        _: &SessionId,
        _: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError> {
        unreachable!()
    }
    async fn archive_session(&self, _: &SessionId, _: CommandOptions) -> Result<(), MuzenError> {
        unreachable!()
    }
    async fn start_run(&self, _: RunSpec) -> Result<RunId, MuzenError> {
        unreachable!()
    }
    async fn run_snapshot(&self, id: &RunId) -> Result<RunSnapshot, MuzenError> {
        Ok(RunSnapshot {
            id: id.clone(),
            status: RunStatus::Running,
            roots: Vec::new(),
            agents: Vec::new(),
            last_sequence: 0,
            created_at: "now".to_owned(),
            updated_at: "now".to_owned(),
        })
    }
    async fn run_result(&self, _: &RunId) -> Result<Option<RunResult>, MuzenError> {
        unreachable!()
    }
    fn events(&self, _: &RunId, _: EventOptions) -> EventStream {
        unreachable!()
    }
    async fn send(&self, _: &RunId, _: SendCommand) -> Result<CommandReceipt, MuzenError> {
        unreachable!()
    }
    async fn spawn(&self, _: &RunId, _: SpawnCommand) -> Result<SessionId, MuzenError> {
        unreachable!()
    }
    async fn cancel(&self, _: &RunId, _: CancelOptions) -> Result<CommandReceipt, MuzenError> {
        unreachable!()
    }
    async fn artifact_chunk(
        &self,
        _: &ArtifactId,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ArtifactChunk, MuzenError> {
        let start = (offset as usize).min(self.bytes.len());
        let end = start
            .saturating_add(max_bytes as usize)
            .min(self.bytes.len());
        Ok(ArtifactChunk {
            data: base64::engine::general_purpose::STANDARD.encode(&self.bytes[start..end]),
            eof: end == self.bytes.len(),
        })
    }
    async fn close(&self) -> Result<(), MuzenError> {
        Ok(())
    }
}

#[tokio::test]
async fn artifact_ranges_return_exact_206_and_416_shapes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(
        Arc::new(ArtifactRuntime {
            bytes: b"abcdefghij".to_vec(),
        }),
        HttpServiceConfig::default(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    for (range, expected, content_range) in [
        ("bytes=2-5", "cdef", "bytes 2-5/10"),
        ("bytes=7-", "hij", "bytes 7-9/10"),
        ("bytes=-3", "hij", "bytes 7-9/10"),
    ] {
        let response = client
            .get(format!("http://{address}/v1/runs/run-1/artifacts/a-1"))
            .header("Range", range)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["Content-Range"], content_range);
        assert_eq!(response.text().await.unwrap(), expected);
    }
    let response = client
        .get(format!("http://{address}/v1/runs/run-1/artifacts/a-1"))
        .header("Range", "bytes=99-")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.json::<MuzenError>().await.unwrap().code(),
        ErrorCode::InvalidInput
    );
    server.abort();
}
