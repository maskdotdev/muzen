use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use futures::{stream, Stream, StreamExt};

use super::validation::{
    validate_run_spec, validate_secret_input, validate_send_command, validate_session_spec,
    validate_spawn_command,
};
use super::{
    AgentEvent, AgentInput, AgentMessage, ArtifactChunk, ArtifactId, ArtifactRef, CancelOptions,
    Capabilities, CommandOptions, CommandReceipt, CreateOptions, EventOptions, MessagePage,
    MuzenError, Page, PutSecretInput, RunId, RunResult, RunRoot, RunSnapshot, RunSpec, SecretRef,
    SendCommand, SessionId, SessionSnapshot, SessionSpec, SingleRunOptions, SpawnCommand,
};

type EventStream = Pin<Box<dyn Stream<Item = Result<AgentEvent, MuzenError>> + Send>>;

#[async_trait]
pub(crate) trait RuntimeTransport: Send + Sync {
    async fn capabilities(&self) -> Result<Capabilities, MuzenError>;
    async fn put_secret(&self, input: PutSecretInput) -> Result<SecretRef, MuzenError>;
    async fn delete_secret(&self, secret: &SecretRef) -> Result<(), MuzenError>;
    async fn create_session(
        &self,
        spec: SessionSpec,
        options: CreateOptions,
    ) -> Result<SessionId, MuzenError>;
    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot, MuzenError>;
    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError>;
    async fn archive_session(
        &self,
        id: &SessionId,
        options: CommandOptions,
    ) -> Result<(), MuzenError>;
    async fn start_run(&self, spec: RunSpec) -> Result<RunId, MuzenError>;
    async fn run_snapshot(&self, id: &RunId) -> Result<RunSnapshot, MuzenError>;
    async fn run_result(&self, id: &RunId) -> Result<Option<RunResult>, MuzenError>;
    fn events(&self, id: &RunId, options: EventOptions) -> EventStream;
    async fn send(&self, id: &RunId, command: SendCommand) -> Result<CommandReceipt, MuzenError>;
    async fn spawn(&self, id: &RunId, command: SpawnCommand) -> Result<SessionId, MuzenError>;
    async fn cancel(
        &self,
        id: &RunId,
        options: CancelOptions,
    ) -> Result<CommandReceipt, MuzenError>;
    async fn artifact_ref(
        &self,
        run_id: &RunId,
        artifact_id: &ArtifactId,
    ) -> Result<ArtifactRef, MuzenError>;
    async fn artifact_chunk(
        &self,
        run_id: &RunId,
        artifact_id: &ArtifactId,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ArtifactChunk, MuzenError>;
    async fn close(&self) -> Result<(), MuzenError>;
}

#[derive(Clone)]
pub struct Muzen {
    transport: Arc<dyn RuntimeTransport>,
}

impl Muzen {
    pub async fn local(config: super::LocalRuntimeConfig) -> Result<Self, MuzenError> {
        let transport = super::local::LocalRuntime::connect(config).await?;
        Ok(Self {
            transport: Arc::new(transport),
        })
    }

    pub async fn capabilities(&self) -> Result<Capabilities, MuzenError> {
        self.transport.capabilities().await
    }

    pub async fn put_secret(&self, input: PutSecretInput) -> Result<SecretRef, MuzenError> {
        validate_secret_input(&input)?;
        self.transport.put_secret(input).await
    }

    pub async fn delete_secret(&self, secret: &SecretRef) -> Result<(), MuzenError> {
        self.transport.delete_secret(secret).await
    }

    pub async fn create_session(
        &self,
        spec: SessionSpec,
        options: CreateOptions,
    ) -> Result<AgentSession, MuzenError> {
        validate_session_spec(&spec)?;
        let id = self.transport.create_session(spec, options).await?;
        Ok(AgentSession::new(id, Arc::clone(&self.transport)))
    }

    pub async fn get_session(&self, id: &SessionId) -> Result<AgentSession, MuzenError> {
        self.transport.session_snapshot(id).await?;
        Ok(AgentSession::new(id.clone(), Arc::clone(&self.transport)))
    }

    pub async fn start_run(&self, spec: RunSpec) -> Result<Run, MuzenError> {
        validate_run_spec(&spec)?;
        let id = self.transport.start_run(spec).await?;
        Ok(Run::new(id, Arc::clone(&self.transport)))
    }

    pub async fn get_run(&self, id: &RunId) -> Result<Run, MuzenError> {
        self.transport.run_snapshot(id).await?;
        Ok(Run::new(id.clone(), Arc::clone(&self.transport)))
    }

    pub async fn close(&self) -> Result<(), MuzenError> {
        self.transport.close().await
    }
}

#[derive(Clone)]
pub struct AgentSession {
    id: SessionId,
    transport: Arc<dyn RuntimeTransport>,
}

impl AgentSession {
    fn new(id: SessionId, transport: Arc<dyn RuntimeTransport>) -> Self {
        Self { id, transport }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub async fn snapshot(&self) -> Result<SessionSnapshot, MuzenError> {
        self.transport.session_snapshot(&self.id).await
    }

    pub async fn messages(&self, page: MessagePage) -> Result<Page<AgentMessage>, MuzenError> {
        self.transport.messages(&self.id, page).await
    }

    pub async fn run(
        &self,
        input: AgentInput,
        options: SingleRunOptions,
    ) -> Result<Run, MuzenError> {
        let spec = RunSpec {
            roots: vec![RunRoot::Existing(super::ExistingSessionRoot {
                session_id: self.id.clone(),
                input,
            })],
            limits: options.limits,
            idempotency_key: options.idempotency_key,
            metadata: options.metadata,
        };
        validate_run_spec(&spec)?;
        let id = self.transport.start_run(spec).await?;
        Ok(Run::new(id, Arc::clone(&self.transport)))
    }

    pub async fn archive(&self, options: CommandOptions) -> Result<(), MuzenError> {
        self.transport.archive_session(&self.id, options).await
    }
}

#[derive(Clone)]
pub struct Run {
    id: RunId,
    transport: Arc<dyn RuntimeTransport>,
}

impl Run {
    fn new(id: RunId, transport: Arc<dyn RuntimeTransport>) -> Self {
        Self { id, transport }
    }

    pub fn id(&self) -> &RunId {
        &self.id
    }

    pub async fn snapshot(&self) -> Result<RunSnapshot, MuzenError> {
        self.transport.run_snapshot(&self.id).await
    }

    pub fn events(
        &self,
        options: EventOptions,
    ) -> impl Stream<Item = Result<AgentEvent, MuzenError>> + Send + 'static {
        self.transport.events(&self.id, options)
    }

    pub async fn wait(&self) -> Result<RunResult, MuzenError> {
        if let Some(result) = self.result().await? {
            return Ok(result);
        }
        let mut events = self.events(EventOptions { after: None });
        while let Some(event) = events.next().await.transpose()? {
            if is_terminal_run_event(&event.event_type) {
                if let Some(result) = self.result().await? {
                    return Ok(result);
                }
            }
        }
        self.result()
            .await?
            .ok_or_else(|| MuzenError::internal("run event stream ended without a durable result"))
    }

    pub async fn result(&self) -> Result<Option<RunResult>, MuzenError> {
        self.transport.run_result(&self.id).await
    }

    pub async fn send(&self, command: SendCommand) -> Result<CommandReceipt, MuzenError> {
        validate_send_command(&command)?;
        self.transport.send(&self.id, command).await
    }

    pub async fn spawn(&self, command: SpawnCommand) -> Result<AgentSession, MuzenError> {
        validate_spawn_command(&command)?;
        let id = self.transport.spawn(&self.id, command).await?;
        Ok(AgentSession::new(id, Arc::clone(&self.transport)))
    }

    pub async fn cancel(&self, options: CancelOptions) -> Result<CommandReceipt, MuzenError> {
        self.transport.cancel(&self.id, options).await
    }

    pub async fn artifact(&self, id: &ArtifactId) -> Result<Artifact, MuzenError> {
        let reference = self.transport.artifact_ref(&self.id, id).await?;
        Ok(Artifact {
            reference,
            run_id: self.id.clone(),
            transport: Arc::clone(&self.transport),
        })
    }
}

pub(crate) fn is_terminal_run_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "run.completed" | "run.partial" | "run.failed" | "run.cancelled"
    )
}

pub struct Artifact {
    reference: ArtifactRef,
    run_id: RunId,
    transport: Arc<dyn RuntimeTransport>,
}

impl Artifact {
    pub fn reference(&self) -> &ArtifactRef {
        &self.reference
    }

    pub fn data(&self) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, MuzenError>> + Send>> {
        const CHUNK_BYTES: u32 = 64 * 1024;
        let state = (
            Arc::clone(&self.transport),
            self.run_id.clone(),
            self.reference.id.clone(),
            0_u64,
            false,
        );
        Box::pin(stream::try_unfold(
            state,
            |(transport, run_id, artifact_id, offset, done)| async move {
                if done {
                    return Ok(None);
                }
                let chunk = transport
                    .artifact_chunk(&run_id, &artifact_id, offset, CHUNK_BYTES)
                    .await?;
                let bytes = decode_base64(&chunk.data)?;
                if bytes.is_empty() {
                    if chunk.eof {
                        return Ok(None);
                    }
                    return Err(MuzenError::internal(
                        "artifact transport returned an empty non-terminal chunk",
                    ));
                }
                let next_offset = offset.saturating_add(bytes.len() as u64);
                Ok(Some((
                    bytes,
                    (transport, run_id, artifact_id, next_offset, chunk.eof),
                )))
            },
        ))
    }
}

fn decode_base64(value: &str) -> Result<Vec<u8>, MuzenError> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| MuzenError::internal("artifact transport returned invalid base64"))
}
