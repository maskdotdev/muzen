use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::pin::Pin;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::Stream;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::super::client::RuntimeTransport;
use super::super::{
    AgentEvent, AgentMessage, ArtifactChunk, ArtifactId, CancelOptions, Capabilities,
    CommandOptions, CommandReceipt, CreateOptions, EventOptions, MessagePage, Muzen, MuzenError,
    Page, PutSecretInput, RunId, RunResult, RunSnapshot, RunSpec, SecretRef, SendCommand,
    SessionId, SessionSnapshot, SessionSpec, SpawnCommand,
};
use super::wire::{
    put_secret_params, ArtifactReadParams, EmptyParams, Notification, OutboundRequest, Response,
    RunCancelParams, RunEventParams, RunEventsParams, RunEventsResult, RunParams, RunSendParams,
    RunSpawnParams, RunStartParams, SecretDeleteParams, SessionArchiveParams, SessionCreateParams,
    SessionMessagesParams, SessionParams, UnsubscribeParams, JSONRPC_VERSION, MUZEN_ERROR_CODE,
};

type PendingResult = Result<Value, MuzenError>;

pub(crate) struct RunnerTransport {
    state: Arc<ClientState>,
}

struct ClientState {
    next_request_id: AtomicU64,
    next_subscription_id: AtomicU64,
    writer: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    pending: Mutex<BTreeMap<u64, oneshot::Sender<PendingResult>>>,
    subscriptions: Mutex<BTreeMap<String, mpsc::UnboundedSender<Result<AgentEvent, MuzenError>>>>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: AtomicBool,
}

impl RunnerTransport {
    pub(crate) fn connect<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let state = Arc::new(ClientState {
            next_request_id: AtomicU64::new(1),
            next_subscription_id: AtomicU64::new(1),
            writer: Mutex::new(Some(writer_tx)),
            pending: Mutex::new(BTreeMap::new()),
            subscriptions: Mutex::new(BTreeMap::new()),
            writer_task: Mutex::new(None),
            reader_task: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let writer_state = Arc::clone(&state);
        let writer_task = tokio::spawn(async move {
            if let Err(error) = client_writer_loop(writer, writer_rx).await {
                fail_transport(&writer_state, format!("runner write failed: {error}"));
            }
        });
        let reader_state = Arc::clone(&state);
        let reader_task = tokio::spawn(async move {
            client_reader_loop(reader, reader_state).await;
        });
        *state.writer_task.lock() = Some(writer_task);
        *state.reader_task.lock() = Some(reader_task);
        Self { state }
    }

    async fn request<T, P>(&self, method: &str, params: P) -> Result<T, MuzenError>
    where
        T: DeserializeOwned,
        P: Serialize,
    {
        request(&self.state, method, params).await
    }
}

impl Clone for RunnerTransport {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

#[async_trait]
impl RuntimeTransport for RunnerTransport {
    async fn capabilities(&self) -> Result<Capabilities, MuzenError> {
        self.request("muzen.capabilities", EmptyParams::default())
            .await
    }

    async fn put_secret(&self, input: PutSecretInput) -> Result<SecretRef, MuzenError> {
        self.request("secret.put", put_secret_params(input)).await
    }

    async fn delete_secret(&self, secret: &SecretRef) -> Result<(), MuzenError> {
        self.request(
            "secret.delete",
            SecretDeleteParams {
                secret: secret.clone(),
                idempotency_key: None,
            },
        )
        .await
    }

    async fn create_session(
        &self,
        spec: SessionSpec,
        options: CreateOptions,
    ) -> Result<SessionId, MuzenError> {
        self.request(
            "session.create",
            SessionCreateParams {
                spec,
                options: (options != CreateOptions::default()).then_some(options),
            },
        )
        .await
    }

    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot, MuzenError> {
        self.request(
            "session.get",
            SessionParams {
                session_id: id.clone(),
            },
        )
        .await
    }

    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError> {
        self.request(
            "session.messages",
            SessionMessagesParams {
                session_id: id.clone(),
                page: (page != MessagePage::default()).then_some(page),
            },
        )
        .await
    }

    async fn archive_session(
        &self,
        id: &SessionId,
        options: CommandOptions,
    ) -> Result<(), MuzenError> {
        self.request(
            "session.archive",
            SessionArchiveParams {
                session_id: id.clone(),
                options: (options != CommandOptions::default()).then_some(options),
            },
        )
        .await
    }

    async fn start_run(&self, spec: RunSpec) -> Result<RunId, MuzenError> {
        self.request("run.start", RunStartParams { spec }).await
    }

    async fn run_snapshot(&self, id: &RunId) -> Result<RunSnapshot, MuzenError> {
        self.request("run.get", RunParams { run_id: id.clone() })
            .await
    }

    async fn run_result(&self, id: &RunId) -> Result<Option<RunResult>, MuzenError> {
        self.request("run.result", RunParams { run_id: id.clone() })
            .await
    }

    fn events(
        &self,
        id: &RunId,
        options: EventOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<AgentEvent, MuzenError>> + Send>> {
        let subscription_id = format!(
            "sub-{}",
            self.state
                .next_subscription_id
                .fetch_add(1, Ordering::Relaxed)
        );
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        self.state
            .subscriptions
            .lock()
            .insert(subscription_id.clone(), events_tx.clone());
        let state = Arc::clone(&self.state);
        let run_id = id.clone();
        let worker_subscription_id = subscription_id.clone();
        let worker = tokio::spawn(async move {
            let mut after = options.after;
            loop {
                let previous_after = after;
                let response = request::<RunEventsResult, _>(
                    &state,
                    "run.events",
                    RunEventsParams {
                        run_id: run_id.clone(),
                        after,
                        subscription_id: worker_subscription_id.clone(),
                    },
                )
                .await;
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        let _ = events_tx.send(Err(error));
                        state.subscriptions.lock().remove(&worker_subscription_id);
                        return;
                    }
                };
                for event in response.events {
                    after = Some(event.sequence);
                    if events_tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
                if response.subscribed {
                    return;
                }
                if after == previous_after {
                    state.subscriptions.lock().remove(&worker_subscription_id);
                    return;
                }
            }
        });
        Box::pin(EventReceiverStream {
            receiver: events_rx,
            state: Arc::clone(&self.state),
            subscription_id,
            worker,
        })
    }

    async fn send(&self, id: &RunId, command: SendCommand) -> Result<CommandReceipt, MuzenError> {
        self.request(
            "run.send",
            RunSendParams {
                run_id: id.clone(),
                command,
            },
        )
        .await
    }

    async fn spawn(&self, id: &RunId, command: SpawnCommand) -> Result<SessionId, MuzenError> {
        self.request(
            "run.spawn",
            RunSpawnParams {
                run_id: id.clone(),
                command,
            },
        )
        .await
    }

    async fn cancel(
        &self,
        id: &RunId,
        options: CancelOptions,
    ) -> Result<CommandReceipt, MuzenError> {
        self.request(
            "run.cancel",
            RunCancelParams {
                run_id: id.clone(),
                options: (options != CancelOptions::default()).then_some(options),
            },
        )
        .await
    }

    async fn artifact_chunk(
        &self,
        artifact_id: &ArtifactId,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ArtifactChunk, MuzenError> {
        self.request(
            "artifact.read",
            ArtifactReadParams {
                artifact_id: artifact_id.clone(),
                offset,
                max_bytes,
            },
        )
        .await
    }

    async fn close(&self) -> Result<(), MuzenError> {
        if self.state.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.state.writer.lock().take();
        let writer_task = self.state.writer_task.lock().take();
        if let Some(task) = writer_task {
            let _ = task.await;
        }
        let reader_task = self.state.reader_task.lock().take();
        if let Some(task) = reader_task {
            task.abort();
            let _ = task.await;
        }
        fail_transport(&self.state, "runner transport closed");
        Ok(())
    }
}

struct EventReceiverStream {
    receiver: mpsc::UnboundedReceiver<Result<AgentEvent, MuzenError>>,
    state: Arc<ClientState>,
    subscription_id: String,
    worker: tokio::task::JoinHandle<()>,
}

impl Stream for EventReceiverStream {
    type Item = Result<AgentEvent, MuzenError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

impl Drop for EventReceiverStream {
    fn drop(&mut self) {
        self.worker.abort();
        self.state
            .subscriptions
            .lock()
            .remove(&self.subscription_id);
        if self.state.closed.load(Ordering::Acquire) {
            return;
        }
        let state = Arc::clone(&self.state);
        let subscription_id = self.subscription_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = request::<(), _>(
                    &state,
                    "run.unsubscribe",
                    UnsubscribeParams { subscription_id },
                )
                .await;
            });
        }
    }
}

async fn request<T, P>(state: &Arc<ClientState>, method: &str, params: P) -> Result<T, MuzenError>
where
    T: DeserializeOwned,
    P: Serialize,
{
    if state.closed.load(Ordering::Acquire) {
        return Err(MuzenError::unavailable("runner transport is closed"));
    }
    let id = state.next_request_id.fetch_add(1, Ordering::Relaxed);
    let params = serde_json::to_value(params)
        .map_err(|error| MuzenError::internal(format!("failed to serialize {method}: {error}")))?;
    let request = OutboundRequest {
        jsonrpc: JSONRPC_VERSION,
        id,
        method,
        params,
    };
    let value = serde_json::to_value(request)
        .map_err(|error| MuzenError::internal(format!("failed to serialize {method}: {error}")))?;
    let (result_tx, result_rx) = oneshot::channel();
    state.pending.lock().insert(id, result_tx);
    let writer = state.writer.lock().as_ref().cloned();
    let Some(writer) = writer else {
        state.pending.lock().remove(&id);
        return Err(MuzenError::unavailable("runner transport is closed"));
    };
    if writer.send(value).is_err() {
        state.pending.lock().remove(&id);
        return Err(MuzenError::unavailable("runner writer is unavailable"));
    }
    let value = result_rx
        .await
        .map_err(|_| MuzenError::unavailable("runner connection was lost"))??;
    serde_json::from_value(value).map_err(|error| {
        MuzenError::internal(format!("invalid {method} result from runner: {error}"))
    })
}

async fn client_writer_loop<W>(
    mut writer: W,
    mut messages: mpsc::UnboundedReceiver<Value>,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(value) = messages.recv().await {
        let mut line = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
        line.push(b'\n');
        writer.write_all(&line).await?;
        writer.flush().await?;
    }
    writer.shutdown().await
}

async fn client_reader_loop<R>(reader: R, state: Arc<ClientState>)
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => line,
            Ok(None) => {
                fail_transport(&state, "runner connection closed");
                return;
            }
            Err(error) => {
                fail_transport(&state, format!("runner read failed: {error}"));
                return;
            }
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                fail_transport(&state, format!("runner sent malformed JSON: {error}"));
                return;
            }
        };
        if value.get("method").is_some() {
            handle_notification(&state, value);
        } else if !handle_response(&state, value) {
            fail_transport(&state, "runner sent an invalid response");
            return;
        }
    }
}

fn handle_response(state: &ClientState, value: Value) -> bool {
    let response: Response = match serde_json::from_value::<Response>(value) {
        Ok(response) if response.jsonrpc == JSONRPC_VERSION => response,
        _ => return false,
    };
    let Some(id) = response.id.as_u64() else {
        return false;
    };
    let Some(waiter) = state.pending.lock().remove(&id) else {
        return true;
    };
    let result = if let Some(error) = response.error {
        Err(rpc_muzen_error(error.code, &error.message, error.data))
    } else {
        Ok(response.result.unwrap_or(Value::Null))
    };
    let _ = waiter.send(result);
    true
}

fn handle_notification(state: &ClientState, value: Value) {
    let notification: Notification = match serde_json::from_value::<Notification>(value) {
        Ok(notification)
            if notification.jsonrpc == JSONRPC_VERSION && notification.method == "run.event" =>
        {
            notification
        }
        _ => return,
    };
    let params: RunEventParams = match serde_json::from_value(notification.params) {
        Ok(params) => params,
        Err(_) => return,
    };
    let terminal = super::super::client::is_terminal_run_event(&params.event.event_type);
    let subscriptions = state.subscriptions.lock();
    if let Some(sender) = subscriptions.get(&params.subscription_id) {
        let _ = sender.send(Ok(params.event));
    }
    drop(subscriptions);
    if terminal {
        state.subscriptions.lock().remove(&params.subscription_id);
    }
}

fn rpc_muzen_error(code: i64, message: &str, data: Option<Value>) -> MuzenError {
    if code == MUZEN_ERROR_CODE {
        if let Some(data) = data {
            if let Ok(error) = serde_json::from_value(data) {
                return error;
            }
        }
        return MuzenError::internal(format!("runner returned malformed MuzenError: {message}"));
    }
    match code {
        -32601 => MuzenError::unsupported(message),
        -32600 | -32602 | -32700 => MuzenError::invalid_input(message),
        _ => MuzenError::internal(message),
    }
}

fn fail_transport(state: &ClientState, message: impl Into<String>) {
    state.closed.store(true, Ordering::Release);
    state.writer.lock().take();
    let error = MuzenError::unavailable(message.into());
    for (_, waiter) in std::mem::take(&mut *state.pending.lock()) {
        let _ = waiter.send(Err(error.clone()));
    }
    for (_, sender) in std::mem::take(&mut *state.subscriptions.lock()) {
        let _ = sender.send(Err(error.clone()));
    }
}

/// Child process handle returned by [`spawn_local_runner`].
pub struct RunnerChild {
    child: Child,
}

impl RunnerChild {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }

    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait()
    }
}

/// Spawns a `muzen-agent-runner`-compatible executable and connects its stdio.
pub async fn spawn_local_runner<I, S>(program: S, args: I) -> std::io::Result<(Muzen, RunnerChild)>
where
    I: IntoIterator,
    I::Item: AsRef<OsStr>,
    S: AsRef<OsStr>,
{
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut child_stdin = child.stdin.take().expect("piped child stdin");
    let mut child_stdout = child.stdout.take().expect("piped child stdout");
    let (client_read, mut stdout_bridge) = tokio::io::duplex(64 * 1024);
    let (mut stdin_bridge, client_write) = tokio::io::duplex(64 * 1024);
    let handle = tokio::runtime::Handle::current();
    let stdout_handle = handle.clone();
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match child_stdout.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if stdout_handle
                .block_on(stdout_bridge.write_all(&buffer[..read]))
                .is_err()
            {
                break;
            }
        }
    });
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match handle.block_on(stdin_bridge.read(&mut buffer)) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if child_stdin.write_all(&buffer[..read]).is_err() || child_stdin.flush().is_err() {
                break;
            }
        }
    });
    Ok((
        Muzen::runner(client_read, client_write),
        RunnerChild { child },
    ))
}
