mod engine;
mod provider;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{stream, Stream};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::client::RuntimeTransport;
use super::store::memory::MemoryAgentStore;
use super::store::sqlite::SqliteAgentStore;
use super::store::AgentStore;
use super::{
    AgentEvent, AgentMessage, ArtifactChunk, ArtifactId, ArtifactRef, CancelOptions, Capabilities,
    CommandOptions, CommandReceipt, CreateOptions, EventOptions, MessagePage, ModelProtocol,
    MuzenError, Page, PutSecretInput, RunId, RunResult, RunSnapshot, RunSpec, SecretRef,
    SendCommand, SessionId, SessionSnapshot, SessionSpec, SpawnCommand,
};

pub use provider::{ModelProvider, ModelProviderError, ModelRequest, ModelStop, ModelTurn};

const REPLAY_BATCH: u64 = 256;

#[derive(Debug, Clone)]
pub enum LocalStoreConfig {
    Memory,
    Sqlite(PathBuf),
}

pub struct LocalRuntimeConfig {
    pub provider: Arc<dyn ModelProvider>,
    pub store: LocalStoreConfig,
    pub close_timeout: Duration,
}

impl LocalRuntimeConfig {
    pub fn memory(provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            provider,
            store: LocalStoreConfig::Memory,
            close_timeout: Duration::from_secs(5),
        }
    }

    pub fn sqlite(provider: Arc<dyn ModelProvider>, path: impl Into<PathBuf>) -> Self {
        Self {
            provider,
            store: LocalStoreConfig::Sqlite(path.into()),
            close_timeout: Duration::from_secs(5),
        }
    }
}

pub(crate) struct LocalRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn AgentStore>,
    provider: Arc<dyn ModelProvider>,
    notifications: Mutex<BTreeMap<RunId, watch::Sender<u64>>>,
    scheduled: Mutex<BTreeSet<RunId>>,
    tasks: Mutex<BTreeMap<RunId, JoinHandle<()>>>,
    accepting: AtomicBool,
    close_timeout: Duration,
}

impl LocalRuntime {
    pub(crate) async fn connect(config: LocalRuntimeConfig) -> Result<Self, MuzenError> {
        let store: Arc<dyn AgentStore> = match config.store {
            LocalStoreConfig::Memory => Arc::new(MemoryAgentStore::new()),
            LocalStoreConfig::Sqlite(path) => Arc::new(SqliteAgentStore::connect(path).await?),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                provider: config.provider,
                notifications: Mutex::new(BTreeMap::new()),
                scheduled: Mutex::new(BTreeSet::new()),
                tasks: Mutex::new(BTreeMap::new()),
                accepting: AtomicBool::new(true),
                close_timeout: config.close_timeout,
            }),
        })
    }

    async fn ensure_scheduled(&self, id: &RunId) -> Result<(), MuzenError> {
        let stored = self.inner.store.run(id).await?;
        let should_schedule = stored.result.is_none() && {
            self.inner.notify(id, stored.snapshot.last_sequence);
            self.inner.scheduled.lock().insert(id.clone())
        };
        if should_schedule {
            let inner = Arc::clone(&self.inner);
            let task_id = id.clone();
            let handle = tokio::spawn(async move {
                engine::execute(inner, task_id).await;
            });
            self.inner.tasks.lock().insert(id.clone(), handle);
        }
        Ok(())
    }
}

impl Inner {
    fn receiver(&self, run_id: &RunId) -> Option<watch::Receiver<u64>> {
        self.notifications
            .lock()
            .get(run_id)
            .map(watch::Sender::subscribe)
    }

    fn receiver_or_create(&self, run_id: &RunId, sequence: u64) -> watch::Receiver<u64> {
        let mut notifications = self.notifications.lock();
        notifications
            .entry(run_id.clone())
            .or_insert_with(|| watch::channel(sequence).0)
            .subscribe()
    }

    fn notify(&self, run_id: &RunId, sequence: u64) {
        let mut notifications = self.notifications.lock();
        notifications
            .entry(run_id.clone())
            .or_insert_with(|| watch::channel(sequence).0)
            .send_replace(sequence);
    }

    async fn notify_snapshot(&self, run_id: &RunId) {
        if let Ok(run) = self.store.run(run_id).await {
            self.notify(run_id, run.snapshot.last_sequence);
        }
    }

    async fn cleanup_terminal(&self, run_id: &RunId) {
        if !self
            .store
            .run(run_id)
            .await
            .is_ok_and(|run| run.result.is_some())
        {
            return;
        }
        self.tasks.lock().remove(run_id);
        self.scheduled.lock().remove(run_id);
        self.notifications.lock().remove(run_id);
    }
}

struct EventState {
    inner: Arc<Inner>,
    run_id: RunId,
    cursor: u64,
    receiver: Option<watch::Receiver<u64>>,
    buffered: VecDeque<AgentEvent>,
    done: bool,
}

#[async_trait]
impl RuntimeTransport for LocalRuntime {
    async fn capabilities(&self) -> Result<Capabilities, MuzenError> {
        Ok(Capabilities {
            protocol_version: "1".to_owned(),
            workspace_bases: Vec::new(),
            tool_provider_kinds: Vec::new(),
            model_protocols: vec![
                ModelProtocol::Responses,
                ModelProtocol::ChatCompletions,
                ModelProtocol::Messages,
            ],
            max_replay_batch: NonZeroU32::new(REPLAY_BATCH as u32).expect("non-zero batch"),
        })
    }

    async fn put_secret(&self, _input: PutSecretInput) -> Result<SecretRef, MuzenError> {
        Err(MuzenError::unsupported(
            "local in-process runtime does not support secrets yet",
        ))
    }

    async fn delete_secret(&self, _secret: &SecretRef) -> Result<(), MuzenError> {
        Err(MuzenError::unsupported(
            "local in-process runtime does not support secrets yet",
        ))
    }

    async fn create_session(
        &self,
        spec: SessionSpec,
        options: CreateOptions,
    ) -> Result<SessionId, MuzenError> {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(MuzenError::conflict("local runtime is closed"));
        }
        self.inner
            .store
            .create_session(spec, options.idempotency_key.as_ref())
            .await
    }

    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot, MuzenError> {
        Ok(self.inner.store.session(id).await?.snapshot)
    }

    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError> {
        self.inner.store.messages(id, page).await
    }

    async fn archive_session(
        &self,
        id: &SessionId,
        _options: CommandOptions,
    ) -> Result<(), MuzenError> {
        self.inner.store.archive_session(id).await
    }

    async fn start_run(&self, spec: RunSpec) -> Result<RunId, MuzenError> {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(MuzenError::conflict("local runtime is closed"));
        }
        let id = self.inner.store.create_run(spec).await?;
        self.ensure_scheduled(&id).await?;
        self.inner.cleanup_terminal(&id).await;
        Ok(id)
    }

    async fn run_snapshot(&self, id: &RunId) -> Result<RunSnapshot, MuzenError> {
        Ok(self.inner.store.run(id).await?.snapshot)
    }

    async fn run_result(&self, id: &RunId) -> Result<Option<RunResult>, MuzenError> {
        Ok(self.inner.store.run(id).await?.result)
    }

    fn events(
        &self,
        id: &RunId,
        options: EventOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<AgentEvent, MuzenError>> + Send>> {
        let inner = Arc::clone(&self.inner);
        let run_id = id.clone();
        let cursor = options.after.unwrap_or(0);
        let receiver = inner.receiver(&run_id);
        let state = EventState {
            inner,
            run_id,
            cursor,
            receiver,
            buffered: VecDeque::new(),
            done: false,
        };
        Box::pin(stream::try_unfold(state, |mut state| async move {
            loop {
                if state.done {
                    return Ok(None);
                }
                if let Some(event) = state.buffered.pop_front() {
                    state.cursor = event.sequence;
                    state.done = super::client::is_terminal_run_event(&event.event_type);
                    return Ok(Some((event, state)));
                }
                let run = state.inner.store.run(&state.run_id).await?;
                let terminal = run.result.is_some();
                if terminal {
                    state.inner.notifications.lock().remove(&state.run_id);
                }
                state.buffered = state
                    .inner
                    .store
                    .events_after(
                        &state.run_id,
                        Some(state.cursor),
                        NonZeroU64::new(REPLAY_BATCH).expect("non-zero batch"),
                    )
                    .await?
                    .into();
                if !state.buffered.is_empty() {
                    continue;
                }
                if terminal {
                    return Ok(None);
                }
                let Some(receiver) = state.receiver.as_mut() else {
                    state.receiver = Some(
                        state
                            .inner
                            .receiver_or_create(&state.run_id, run.snapshot.last_sequence),
                    );
                    continue;
                };
                if receiver.changed().await.is_err() {
                    if state.inner.store.run(&state.run_id).await?.result.is_some() {
                        state.receiver = None;
                        continue;
                    }
                    return Err(MuzenError::internal(
                        "local run notification closed before completion",
                    ));
                }
            }
        }))
    }

    async fn send(&self, id: &RunId, command: SendCommand) -> Result<CommandReceipt, MuzenError> {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(MuzenError::conflict("local runtime is closed"));
        }
        let receipt = self.inner.store.accept_send(id, command).await?;
        self.inner.notify_snapshot(id).await;
        self.ensure_scheduled(id).await?;
        Ok(receipt)
    }

    async fn spawn(&self, id: &RunId, command: SpawnCommand) -> Result<SessionId, MuzenError> {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(MuzenError::conflict("local runtime is closed"));
        }
        let child = self.inner.store.spawn_agent(id, command).await?;
        self.inner.notify_snapshot(id).await;
        self.ensure_scheduled(id).await?;
        Ok(child)
    }

    async fn cancel(
        &self,
        id: &RunId,
        options: CancelOptions,
    ) -> Result<CommandReceipt, MuzenError> {
        let receipt = self
            .inner
            .store
            .request_cancel(id, options.reason.as_deref())
            .await?;
        self.inner.notify_snapshot(id).await;
        Ok(receipt)
    }

    async fn artifact_ref(
        &self,
        _run_id: &RunId,
        _artifact_id: &ArtifactId,
    ) -> Result<ArtifactRef, MuzenError> {
        Err(MuzenError::unsupported(
            "local in-process runtime does not support artifacts yet",
        ))
    }

    async fn artifact_chunk(
        &self,
        _run_id: &RunId,
        _artifact_id: &ArtifactId,
        _offset: u64,
        _max_bytes: u32,
    ) -> Result<ArtifactChunk, MuzenError> {
        Err(MuzenError::unsupported(
            "local in-process runtime does not support artifacts yet",
        ))
    }

    async fn close(&self) -> Result<(), MuzenError> {
        if !self.inner.accepting.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let run_ids = self.inner.tasks.lock().keys().cloned().collect::<Vec<_>>();
        for run_id in run_ids {
            if self.inner.store.run(&run_id).await?.result.is_none() {
                self.inner
                    .store
                    .request_cancel(&run_id, Some("runtime closing"))
                    .await?;
                self.inner.notify_snapshot(&run_id).await;
            }
        }
        let handles = std::mem::take(&mut *self.inner.tasks.lock())
            .into_values()
            .collect::<Vec<_>>();
        tokio::time::timeout(self.inner.close_timeout, futures::future::join_all(handles))
            .await
            .map_err(|_| MuzenError::internal("timed out closing local runtime"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
