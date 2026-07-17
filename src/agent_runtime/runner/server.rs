use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::super::client::{is_terminal_run_event, RuntimeTransport};
use super::super::local::LocalRuntime;
use super::super::{
    AgentEvent, EventOptions, LocalRuntimeConfig, MuzenError, RunStatus, SessionId, SessionStatus,
};
use super::wire::{
    ArtifactReadParams, EmptyParams, Notification, Request, Response, RpcError,
    RunAnswerToolCallParams, RunCancelParams, RunEventParams, RunEventsParams, RunEventsResult,
    RunParams, RunSendParams, RunSpawnParams, RunStartParams, SecretDeleteParams,
    SessionArchiveParams, SessionCreateParams, SessionMessagesParams, SessionParams,
    UnsubscribeParams, JSONRPC_VERSION, MUZEN_ERROR_CODE,
};

type EventStream = Pin<Box<dyn Stream<Item = Result<AgentEvent, MuzenError>> + Send>>;

#[derive(Clone, Copy, Default)]
pub(crate) struct ServerOptions {
    pub(crate) max_replay_batch: Option<NonZeroU32>,
}

struct ServerState {
    inner: Arc<dyn RuntimeTransport>,
    outbound: mpsc::UnboundedSender<Value>,
    subscriptions: Mutex<BTreeMap<String, CancellationToken>>,
    archive_replays: tokio::sync::Mutex<BTreeMap<SessionId, Option<super::super::IdempotencyKey>>>,
    max_replay_batch: NonZeroU32,
}

struct Dispatch {
    result: Value,
    subscription: Option<SubscriptionStart>,
}

struct SubscriptionStart {
    id: String,
    run_id: super::super::RunId,
    stream: EventStream,
    cancellation: CancellationToken,
}

/// Serves the local runtime over process stdin/stdout until stdin reaches EOF.
pub async fn serve_stdio(config: LocalRuntimeConfig) -> Result<(), MuzenError> {
    let runtime = LocalRuntime::connect(config).await?;
    let (server_read, mut stdin_bridge) = tokio::io::duplex(64 * 1024);
    let (mut stdout_bridge, server_write) = tokio::io::duplex(64 * 1024);
    let handle = tokio::runtime::Handle::current();
    let stdin_handle = handle.clone();
    let stdin_task = tokio::task::spawn_blocking(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if stdin_handle
                .block_on(stdin_bridge.write_all(&buffer[..read]))
                .is_err()
            {
                break;
            }
        }
    });
    let stdout_task = tokio::task::spawn_blocking(move || {
        let mut stdout = std::io::stdout().lock();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match handle.block_on(stdout_bridge.read(&mut buffer)) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if stdout.write_all(&buffer[..read]).is_err() || stdout.flush().is_err() {
                break;
            }
        }
    });
    let result = serve_transport(runtime, server_read, server_write).await;
    let _ = stdin_task.await;
    let _ = stdout_task.await;
    result
}

pub(crate) async fn serve_transport<R, W>(
    inner: impl RuntimeTransport + 'static,
    reader: R,
    writer: W,
) -> Result<(), MuzenError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_transport_with_options(inner, reader, writer, ServerOptions::default()).await
}

pub(crate) async fn serve_transport_with_options<R, W>(
    inner: impl RuntimeTransport + 'static,
    reader: R,
    writer: W,
    options: ServerOptions,
) -> Result<(), MuzenError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let inner: Arc<dyn RuntimeTransport> = Arc::new(inner);
    let capabilities = inner.capabilities().await?;
    let max_replay_batch = options
        .max_replay_batch
        .unwrap_or(capabilities.max_replay_batch);
    let (outbound, outbound_rx) = mpsc::unbounded_channel();
    let writer_task = tokio::spawn(writer_loop(writer, outbound_rx));
    let state = Arc::new(ServerState {
        inner: Arc::clone(&inner),
        outbound,
        subscriptions: Mutex::new(BTreeMap::new()),
        archive_replays: tokio::sync::Mutex::new(BTreeMap::new()),
        max_replay_batch,
    });
    let mut requests = JoinSet::new();
    let mut lines = BufReader::new(reader).lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                Err(error) => send_error(
                    &state.outbound,
                    Value::Null,
                    RpcError::protocol(-32700, format!("parse error: {error}")),
                ),
                Ok(value) => match serde_json::from_value::<Request>(value) {
                    Ok(request) if request.jsonrpc == JSONRPC_VERSION && valid_id(&request.id) => {
                        let state = Arc::clone(&state);
                        requests.spawn(async move { handle_request(state, request).await });
                    }
                    Ok(request) => send_error(
                        &state.outbound,
                        request.id,
                        RpcError::protocol(-32600, "invalid JSON-RPC request"),
                    ),
                    Err(error) => send_error(
                        &state.outbound,
                        Value::Null,
                        RpcError::protocol(-32600, format!("invalid request: {error}")),
                    ),
                },
            },
            Ok(None) => break,
            Err(error) => {
                close_subscriptions(&state);
                while requests.join_next().await.is_some() {}
                let _ = inner.close().await;
                return Err(MuzenError::unavailable(format!(
                    "runner read failed: {error}"
                )));
            }
        }
    }

    close_subscriptions(&state);
    while requests.join_next().await.is_some() {}
    let close_result = inner.close().await;
    drop(state);
    let _ = writer_task.await;
    close_result
}

async fn writer_loop<W>(
    mut writer: W,
    mut outbound: mpsc::UnboundedReceiver<Value>,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(value) = outbound.recv().await {
        let mut line = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
        line.push(b'\n');
        writer.write_all(&line).await?;
        writer.flush().await?;
    }
    writer.shutdown().await
}

async fn handle_request(state: Arc<ServerState>, request: Request) {
    let id = request.id;
    match dispatch(&state, &request.method, request.params).await {
        Ok(dispatch) => {
            let response = Response {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                id,
                result: Some(dispatch.result),
                error: None,
            };
            if let Ok(value) = serde_json::to_value(response) {
                let _ = state.outbound.send(value);
            }
            if let Some(subscription) = dispatch.subscription {
                start_subscription(state, subscription);
            }
        }
        Err(error) => send_error(&state.outbound, id, error),
    }
}

async fn dispatch(
    state: &Arc<ServerState>,
    method: &str,
    params: Option<Value>,
) -> Result<Dispatch, RpcError> {
    macro_rules! result {
        ($expression:expr) => {{
            let value = $expression.await.map_err(muzen_rpc_error)?;
            Dispatch {
                result: serialize_result(value)?,
                subscription: None,
            }
        }};
    }

    Ok(match method {
        "muzen.capabilities" => {
            parse_params::<EmptyParams>(params)?;
            let mut capabilities = state.inner.capabilities().await.map_err(muzen_rpc_error)?;
            capabilities.max_replay_batch = state.max_replay_batch;
            Dispatch {
                result: serialize_result(capabilities)?,
                subscription: None,
            }
        }
        "secret.put" => {
            let input = parse_params(params)?;
            result!(state.inner.put_secret(input))
        }
        "secret.delete" => {
            let params: SecretDeleteParams = parse_params(params)?;
            result!(state.inner.delete_secret(&params.secret))
        }
        "session.create" => {
            let params: SessionCreateParams = parse_params(params)?;
            result!(state
                .inner
                .create_session(params.spec, params.options.unwrap_or_default()))
        }
        "session.get" => {
            let params: SessionParams = parse_params(params)?;
            result!(state.inner.session_snapshot(&params.session_id))
        }
        "session.messages" => {
            let params: SessionMessagesParams = parse_params(params)?;
            result!(state
                .inner
                .messages(&params.session_id, params.page.unwrap_or_default()))
        }
        "session.archive" => {
            let params: SessionArchiveParams = parse_params(params)?;
            let options = params.options.unwrap_or_default();
            let mut archive_replays = state.archive_replays.lock().await;
            if let Some(previous_key) = archive_replays.get(&params.session_id) {
                if previous_key.is_some() && previous_key == &options.idempotency_key {
                    Dispatch {
                        result: Value::Null,
                        subscription: None,
                    }
                } else {
                    return Err(muzen_rpc_error(MuzenError::conflict(
                        "session is already archived",
                    )));
                }
            } else {
                let snapshot = state
                    .inner
                    .session_snapshot(&params.session_id)
                    .await
                    .map_err(muzen_rpc_error)?;
                if snapshot.status == SessionStatus::Archived {
                    return Err(muzen_rpc_error(MuzenError::conflict(
                        "session is already archived",
                    )));
                }
                state
                    .inner
                    .archive_session(&params.session_id, options.clone())
                    .await
                    .map_err(muzen_rpc_error)?;
                archive_replays.insert(params.session_id, options.idempotency_key);
                Dispatch {
                    result: Value::Null,
                    subscription: None,
                }
            }
        }
        "run.start" => {
            let params: RunStartParams = parse_params(params)?;
            result!(state.inner.start_run(params.spec))
        }
        "run.get" => {
            let params: RunParams = parse_params(params)?;
            result!(state.inner.run_snapshot(&params.run_id))
        }
        "run.result" => {
            let params: RunParams = parse_params(params)?;
            result!(state.inner.run_result(&params.run_id))
        }
        "run.events" => {
            let params: RunEventsParams = parse_params(params)?;
            dispatch_events(state, params).await?
        }
        "run.unsubscribe" => {
            let params: UnsubscribeParams = parse_params(params)?;
            if let Some(cancellation) = state.subscriptions.lock().remove(&params.subscription_id) {
                cancellation.cancel();
            }
            Dispatch {
                result: Value::Null,
                subscription: None,
            }
        }
        "run.send" => {
            let params: RunSendParams = parse_params(params)?;
            result!(state.inner.send(&params.run_id, params.command))
        }
        "run.spawn" => {
            let params: RunSpawnParams = parse_params(params)?;
            result!(state.inner.spawn(&params.run_id, params.command))
        }
        "run.cancel" => {
            let params: RunCancelParams = parse_params(params)?;
            result!(state
                .inner
                .cancel(&params.run_id, params.options.unwrap_or_default()))
        }
        "run.answer_tool_call" => {
            let params: RunAnswerToolCallParams = parse_params(params)?;
            result!(state.inner.answer_tool_call(&params.run_id, params.input))
        }
        "artifact.read" => {
            let params: ArtifactReadParams = parse_params(params)?;
            if params.max_bytes == 0 {
                return Err(RpcError::protocol(-32602, "maxBytes must be positive"));
            }
            result!(state.inner.artifact_chunk(
                &params.artifact_id,
                params.offset,
                params.max_bytes
            ))
        }
        _ => {
            return Err(RpcError::protocol(
                -32601,
                format!("method not found: {method}"),
            ));
        }
    })
}

async fn dispatch_events(
    state: &Arc<ServerState>,
    params: RunEventsParams,
) -> Result<Dispatch, RpcError> {
    if params.subscription_id.is_empty() {
        return Err(RpcError::protocol(
            -32602,
            "subscriptionId must not be empty",
        ));
    }
    if state
        .subscriptions
        .lock()
        .contains_key(&params.subscription_id)
    {
        return Err(muzen_rpc_error(MuzenError::conflict(
            "subscriptionId is already active",
        )));
    }
    let snapshot = state
        .inner
        .run_snapshot(&params.run_id)
        .await
        .map_err(muzen_rpc_error)?;
    let terminal_at_snapshot = matches!(
        snapshot.status,
        RunStatus::Completed | RunStatus::Partial | RunStatus::Failed | RunStatus::Cancelled
    );
    let mut cursor = params.after.unwrap_or(0);
    let mut stream = state.inner.events(
        &params.run_id,
        EventOptions {
            after: params.after,
        },
    );
    let mut events = Vec::new();
    let mut terminal_event = false;
    let limit = state.max_replay_batch.get() as usize;
    while cursor < snapshot.last_sequence && events.len() < limit {
        match stream.next().await {
            Some(Ok(event)) => {
                cursor = event.sequence;
                terminal_event = is_terminal_run_event(&event.event_type);
                events.push(event);
                if terminal_event {
                    break;
                }
            }
            Some(Err(error)) => return Err(muzen_rpc_error(error)),
            None => break,
        }
    }
    let live_edge = cursor >= snapshot.last_sequence;
    let subscribed = live_edge && !terminal_at_snapshot && !terminal_event;
    let subscription = if subscribed {
        let cancellation = CancellationToken::new();
        state
            .subscriptions
            .lock()
            .insert(params.subscription_id.clone(), cancellation.clone());
        Some(SubscriptionStart {
            id: params.subscription_id.clone(),
            run_id: params.run_id,
            stream,
            cancellation,
        })
    } else {
        None
    };
    let result = RunEventsResult {
        events,
        subscribed,
        subscription_id: subscribed.then_some(params.subscription_id),
    };
    Ok(Dispatch {
        result: serialize_result(result)?,
        subscription,
    })
}

fn start_subscription(state: Arc<ServerState>, mut subscription: SubscriptionStart) {
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                _ = subscription.cancellation.cancelled() => break,
                next = subscription.stream.next() => next,
            };
            let Some(event) = next else { break };
            let event = match event {
                Ok(event) => event,
                Err(_) => break,
            };
            let terminal = is_terminal_run_event(&event.event_type);
            let notification = Notification {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                method: "run.event".to_owned(),
                params: serialize_result(RunEventParams {
                    subscription_id: subscription.id.clone(),
                    run_id: subscription.run_id.clone(),
                    event,
                })
                .unwrap_or(Value::Null),
            };
            if serde_json::to_value(notification)
                .ok()
                .is_none_or(|value| state.outbound.send(value).is_err())
            {
                break;
            }
            if terminal {
                break;
            }
        }
        state.subscriptions.lock().remove(&subscription.id);
    });
}

fn parse_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| RpcError::protocol(-32602, format!("invalid params: {error}")))
}

fn serialize_result<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value)
        .map_err(|error| muzen_rpc_error(MuzenError::internal(error.to_string())))
}

fn muzen_rpc_error(error: MuzenError) -> RpcError {
    RpcError {
        code: MUZEN_ERROR_CODE,
        message: error.message().to_owned(),
        data: serde_json::to_value(error).ok(),
    }
}

fn send_error(outbound: &mpsc::UnboundedSender<Value>, id: Value, error: RpcError) {
    let response = Response {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        id,
        result: None,
        error: Some(error),
    };
    if let Ok(value) = serde_json::to_value(response) {
        let _ = outbound.send(value);
    }
}

fn valid_id(id: &Value) -> bool {
    matches!(id, Value::Null | Value::String(_) | Value::Number(_))
}

fn close_subscriptions(state: &ServerState) {
    let subscriptions = std::mem::take(&mut *state.subscriptions.lock());
    for cancellation in subscriptions.into_values() {
        cancellation.cancel();
    }
}
