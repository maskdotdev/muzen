use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures::{future, StreamExt, TryStreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::super::local::LocalRuntime;
use super::super::{
    AgentInput, ContentBlock, CreateOptions, ErrorCode, EventOptions, IdempotencyKey,
    LocalRuntimeConfig, MessageDelivery, MessagePage, ModelProvider, ModelProviderError,
    ModelRequest, ModelStop, ModelTurn, Muzen, PutSecretInput, RunLimits, RunResult, SendCommand,
    SessionSpec, SingleRunOptions, TerminalRunStatus, Usage,
};
use super::server::{serve_transport, serve_transport_with_options, ServerOptions};

struct ScriptedProvider {
    turns: Mutex<VecDeque<(Duration, ModelTurn)>>,
}

impl ScriptedProvider {
    fn new(turns: impl IntoIterator<Item = (Duration, ModelTurn)>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into_iter().collect()),
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

fn turn(text: &str, delay: Duration) -> (Duration, ModelTurn) {
    (
        delay,
        ModelTurn {
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                tool_calls: 0,
            },
            stop: ModelStop::EndTurn,
        },
    )
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
        max_active_agents: NonZeroU32::new(2).expect("limit"),
        max_agents: NonZeroU32::new(2).expect("limit"),
        max_depth: 0,
        max_input_bytes: NonZeroU64::new(4096).expect("limit"),
        max_total_tokens: None,
        max_total_tool_calls: Some(0),
        deadline_ms: None,
    }
}

async fn runner_pair(
    provider: Arc<ScriptedProvider>,
    max_replay_batch: Option<NonZeroU32>,
) -> (
    Muzen,
    tokio::task::JoinHandle<Result<(), super::super::MuzenError>>,
) {
    let runtime = LocalRuntime::connect(LocalRuntimeConfig::memory(provider))
        .await
        .expect("runtime");
    let (client_io, server_io) = tokio::io::duplex(256 * 1024);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = tokio::spawn(serve_transport_with_options(
        runtime,
        server_read,
        server_write,
        ServerOptions { max_replay_batch },
    ));
    (Muzen::runner(client_read, client_write), server)
}

async fn finish_server(
    muzen: Muzen,
    server: tokio::task::JoinHandle<Result<(), super::super::MuzenError>>,
) {
    muzen.close().await.expect("close client");
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server ended")
        .expect("server task")
        .expect("clean server shutdown");
}

#[tokio::test]
async fn lifecycle_events_and_messages_work_through_runner_client() {
    let (muzen, server) = runner_pair(
        ScriptedProvider::new([turn("wire result", Duration::from_millis(60))]),
        None,
    )
    .await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("hello over JSON-RPC"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let events_run = run.clone();
    let (events, result) = tokio::join!(
        async move {
            events_run
                .events(EventOptions::default())
                .try_collect::<Vec<_>>()
                .await
                .expect("events")
        },
        run.wait()
    );
    let result: RunResult = result.expect("durable result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert!(events.len() >= 6);
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    let messages = session
        .messages(MessagePage::default())
        .await
        .expect("messages");
    assert!(messages.items.len() >= 2);
    finish_server(muzen, server).await;
}

#[tokio::test]
async fn concurrent_requests_do_not_corrupt_active_subscription() {
    let (muzen, server) = runner_pair(
        ScriptedProvider::new([
            turn("first", Duration::from_millis(100)),
            turn("done", Duration::ZERO),
        ]),
        None,
    )
    .await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("concurrent"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let events_run = run.clone();
    let events_task = tokio::spawn(async move {
        events_run
            .events(EventOptions::default())
            .try_collect::<Vec<_>>()
            .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    run.send(SendCommand {
        session_id: session.id().clone(),
        input: input("steer while subscribed"),
        delivery: MessageDelivery::Steer,
        idempotency_key: None,
    })
    .await
    .expect("concurrent send response");
    let snapshots = (0..64).map(|_| {
        let run = run.clone();
        async move { run.snapshot().await }
    });
    for snapshot in future::join_all(snapshots).await {
        snapshot.expect("concurrent snapshot response");
    }
    run.wait().await.expect("result");
    let events = events_task.await.expect("event task").expect("events");
    assert!(events
        .last()
        .is_some_and(|event| event.event_type == "run.completed"));
    finish_server(muzen, server).await;
}

#[tokio::test]
async fn truncated_replay_continues_from_after_until_terminal_event() {
    let (muzen, server) = runner_pair(
        ScriptedProvider::new([turn("done", Duration::ZERO)]),
        NonZeroU32::new(2),
    )
    .await;
    assert_eq!(
        muzen
            .capabilities()
            .await
            .expect("capabilities")
            .max_replay_batch
            .get(),
        2
    );
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("replay"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    run.wait().await.expect("result");
    let all = run
        .events(EventOptions::default())
        .try_collect::<Vec<_>>()
        .await
        .expect("continued replay");
    let after = all[2].sequence;
    let suffix = run
        .events(EventOptions { after: Some(after) })
        .try_collect::<Vec<_>>()
        .await
        .expect("suffix replay");
    assert_eq!(
        suffix
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        all.iter()
            .filter(|event| event.sequence > after)
            .map(|event| event.sequence)
            .collect::<Vec<_>>()
    );
    finish_server(muzen, server).await;
}

#[tokio::test]
async fn dropping_stream_unsubscribes_and_second_stream_replays_gap_free() {
    let (muzen, server) = runner_pair(
        ScriptedProvider::new([turn("done", Duration::from_millis(100))]),
        None,
    )
    .await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("unsubscribe"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let mut first_stream = run.events(EventOptions::default());
    let first = first_stream
        .next()
        .await
        .expect("first event")
        .expect("valid first event");
    drop(first_stream);
    run.wait().await.expect("result");
    let replay = run
        .events(EventOptions {
            after: Some(first.sequence),
        })
        .try_collect::<Vec<_>>()
        .await
        .expect("second replay");
    assert_eq!(
        replay.first().expect("later event").sequence,
        first.sequence + 1
    );
    assert_eq!(
        replay.last().expect("terminal event").event_type,
        "run.completed"
    );
    finish_server(muzen, server).await;
}

#[tokio::test]
async fn muzen_errors_map_back_to_client_error_codes() {
    let (muzen, server) = runner_pair(ScriptedProvider::new([]), None).await;
    let missing = super::super::RunId::new("run_missing").expect("id");
    let error = match muzen.get_run(&missing).await {
        Ok(_) => panic!("missing run unexpectedly existed"),
        Err(error) => error,
    };
    assert_eq!(error.code(), ErrorCode::NotFound);
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    session
        .archive(Default::default())
        .await
        .expect("first archive");
    let error = session
        .archive(Default::default())
        .await
        .expect_err("double archive");
    assert_eq!(error.code(), ErrorCode::Conflict);
    finish_server(muzen, server).await;
}

#[tokio::test]
async fn secrets_round_trip_and_idempotency_replays_over_wire() {
    let (muzen, server) = runner_pair(ScriptedProvider::new([]), None).await;
    let input = PutSecretInput {
        value: base64::engine::general_purpose::STANDARD.encode("runner secret"),
        idempotency_key: Some(IdempotencyKey::new("secret-key").expect("key")),
    };
    let first = muzen.put_secret(input.clone()).await.expect("put secret");
    let replay = muzen.put_secret(input).await.expect("replay put");
    assert_eq!(first, replay);
    muzen.delete_secret(&first).await.expect("delete");
    muzen
        .delete_secret(&first)
        .await
        .expect("idempotent delete");
    finish_server(muzen, server).await;
}

#[tokio::test]
async fn transport_loss_while_waiting_surfaces_retryable_unavailable() {
    let (muzen, server) = runner_pair(
        ScriptedProvider::new([turn("late", Duration::from_millis(200))]),
        None,
    )
    .await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("disconnect"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let wait = tokio::spawn(async move { run.wait().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    server.abort();
    let _ = server.await;
    let error = tokio::time::timeout(Duration::from_secs(1), wait)
        .await
        .expect("wait resolved")
        .expect("wait task")
        .expect_err("transport loss must fail the current wait");
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(error.retryable());
    muzen.close().await.expect("close disconnected client");
}

#[tokio::test]
async fn malformed_json_and_reserved_errors_keep_connection_usable() {
    let runtime = LocalRuntime::connect(LocalRuntimeConfig::memory(ScriptedProvider::new([])))
        .await
        .expect("runtime");
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = tokio::spawn(serve_transport(runtime, server_read, server_write));
    let mut lines = BufReader::new(client_read).lines();

    client_write
        .write_all(b"{broken\n")
        .await
        .expect("write malformed");
    let parse_error: Value = serde_json::from_str(
        &lines
            .next_line()
            .await
            .expect("read")
            .expect("parse response"),
    )
    .expect("JSON response");
    assert_eq!(parse_error["error"]["code"], -32700);

    client_write
        .write_all(
            serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "muzen.capabilities",
                "params": {}
            }))
            .expect("request")
            .as_bytes(),
        )
        .await
        .expect("write capabilities");
    client_write.write_all(b"\n").await.expect("newline");
    let capabilities: Value = serde_json::from_str(
        &lines
            .next_line()
            .await
            .expect("read")
            .expect("capabilities"),
    )
    .expect("JSON response");
    assert_eq!(capabilities["result"]["protocolVersion"], "1");

    client_write
        .write_all(
            serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "artifact.read",
                "params": {"artifactId": "artifact_missing", "offset": 0, "maxBytes": 32}
            }))
            .expect("request")
            .as_bytes(),
        )
        .await
        .expect("write artifact read");
    client_write.write_all(b"\n").await.expect("newline");
    let unsupported: Value =
        serde_json::from_str(&lines.next_line().await.expect("read").expect("unsupported"))
            .expect("JSON response");
    assert_eq!(unsupported["error"]["code"], -32000);
    assert_eq!(unsupported["error"]["data"]["code"], "unsupported");
    assert_eq!(unsupported["error"]["data"]["retryable"], false);

    drop(client_write);
    drop(lines);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("EOF closes server")
        .expect("server task")
        .expect("clean EOF");
}
