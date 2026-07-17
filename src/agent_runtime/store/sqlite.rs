use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use libsql::{params, Builder, Connection};
#[cfg(test)]
use tokio::sync::Semaphore;

use super::support::{
    is_terminal, new_message_id, new_run_id, new_session_id, public_agent_status,
    public_run_status, root_input, terminal_agent_event_type, terminal_event_type, timestamp,
};
use super::{
    body_digest, receipt, AgentAdvance, AgentStore, FinishRun, PendingSend, RunActivity, StoredRun,
    StoredSession,
};
use crate::agent_runtime::validation::{
    decoded_input_bytes, validate_run_spec, validate_send_command, validate_session_spec,
    validate_spawn_command,
};
use crate::agent_runtime::{
    AgentEvent, AgentMessage, AgentOutput, AgentSnapshot, AgentStatus, CommandReceipt,
    IdempotencyKey, MessageDelivery, MessagePage, MessageRole, MuzenError, Page, RunId, RunResult,
    RunRoot, RunSnapshot, RunSpec, RunStatus, SendCommand, SessionId, SessionSnapshot, SessionSpec,
    SessionStatus, SpawnCommand, TerminalAgentStatus, TerminalRunStatus, Usage,
};

mod actor;
mod persistence;

use actor::ConnectionActor;
use persistence::*;

const SCHEMA_VERSION: i64 = 1;

pub(crate) struct SqliteAgentStore {
    connection: ConnectionActor,
    #[cfg(test)]
    operation_probe: Arc<OperationProbe>,
}

#[cfg(test)]
#[derive(Debug)]
struct OperationProbe {
    append_activity_armed: AtomicBool,
    append_activity_started: Semaphore,
}

#[cfg(test)]
impl Default for OperationProbe {
    fn default() -> Self {
        Self {
            append_activity_armed: AtomicBool::new(false),
            append_activity_started: Semaphore::new(0),
        }
    }
}

#[cfg(test)]
impl OperationProbe {
    fn arm_append_activity(&self) {
        while let Ok(permit) = self.append_activity_started.try_acquire() {
            permit.forget();
        }
        self.append_activity_armed.store(true, Ordering::Release);
    }

    fn signal_append_activity(&self) {
        if self.append_activity_armed.swap(false, Ordering::AcqRel) {
            self.append_activity_started.add_permits(1);
        }
    }

    async fn wait_append_activity_started(&self) {
        self.append_activity_started
            .acquire()
            .await
            .expect("SQLite operation probe remains open")
            .forget();
    }
}

impl std::fmt::Debug for SqliteAgentStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteAgentStore")
            .finish_non_exhaustive()
    }
}

impl SqliteAgentStore {
    pub(crate) async fn connect(path: impl AsRef<Path>) -> Result<Self, MuzenError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                MuzenError::internal(format!("failed to create agent store directory: {error}"))
            })?;
        }
        let database = Builder::new_local(path).build().await.map_err(sql_error)?;
        let connection = database.connect().map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        connection
            .execute_batch(SCHEMA_SQL)
            .await
            .map_err(sql_error)?;
        let mut rows = connection
            .query("SELECT version FROM muzen_agent_meta WHERE id = 1", ())
            .await
            .map_err(sql_error)?;
        let version = rows
            .next()
            .await
            .map_err(sql_error)?
            .map(|row| row.get::<i64>(0).map_err(sql_error))
            .transpose()?;
        if version != Some(SCHEMA_VERSION) {
            return Err(MuzenError::internal(format!(
                "unsupported agent store schema version {version:?}"
            )));
        }
        Ok(Self {
            connection: ConnectionActor::start(database, connection),
            #[cfg(test)]
            operation_probe: Arc::new(OperationProbe::default()),
        })
    }
}

#[async_trait]
impl AgentStore for SqliteAgentStore {
    #[cfg(test)]
    fn arm_append_activity_probe(&self) -> bool {
        self.operation_probe.arm_append_activity();
        true
    }

    #[cfg(test)]
    async fn wait_append_activity_started(&self) {
        self.operation_probe.wait_append_activity_started().await;
    }

    async fn create_session(
        &self,
        spec: SessionSpec,
        idempotency_key: Option<&IdempotencyKey>,
    ) -> Result<SessionId, MuzenError> {
        validate_session_spec(&spec)?;
        let digest = body_digest(&spec)?;
        let idempotency_key = idempotency_key.cloned();
        self.connection
            .call(move |connection| {
                Box::pin(create_session(connection, spec, idempotency_key, digest))
            })
            .await
    }

    async fn session(&self, id: &SessionId) -> Result<StoredSession, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { session_required(connection, &id).await })
            })
            .await
    }

    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| Box::pin(messages(connection, id, page)))
            .await
    }

    async fn archive_session(&self, id: &SessionId) -> Result<(), MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| Box::pin(archive_session(connection, id)))
            .await
    }

    async fn create_run(&self, spec: RunSpec) -> Result<RunId, MuzenError> {
        self.connection
            .call(move |connection| Box::pin(create_run(connection, spec)))
            .await
    }

    async fn run(&self, id: &RunId) -> Result<StoredRun, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move {
                    run_required(connection, &id)
                        .await
                        .map(|record| record.stored)
                })
            })
            .await
    }

    async fn events_after(
        &self,
        id: &RunId,
        after: Option<u64>,
        limit: NonZeroU64,
    ) -> Result<Vec<AgentEvent>, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| Box::pin(events_after(connection, id, after, limit)))
            .await
    }

    async fn mark_run_running(&self, id: &RunId) -> Result<CommandReceipt, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { transition_running(connection, &id).await })
            })
            .await
    }

    async fn set_agent_status(
        &self,
        id: &RunId,
        session_id: &SessionId,
        status: AgentStatus,
    ) -> Result<Option<CommandReceipt>, MuzenError> {
        let id = id.clone();
        let session_id = session_id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(
                    async move { set_agent_status(connection, &id, &session_id, status).await },
                )
            })
            .await
    }

    async fn accept_send(
        &self,
        id: &RunId,
        command: SendCommand,
    ) -> Result<CommandReceipt, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { accept_send(connection, &id, command).await })
            })
            .await
    }

    async fn pending_send(
        &self,
        id: &RunId,
        session_id: &SessionId,
    ) -> Result<Option<PendingSend>, MuzenError> {
        let id = id.clone();
        let session_id = session_id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { pending_send(connection, &id, &session_id).await })
            })
            .await
    }

    async fn deliver_send(
        &self,
        id: &RunId,
        session_id: &SessionId,
        delivery: MessageDelivery,
    ) -> Result<bool, MuzenError> {
        let id = id.clone();
        let session_id = session_id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { deliver_send(connection, &id, &session_id, delivery).await })
            })
            .await
    }

    async fn spawn_agent(
        &self,
        id: &RunId,
        command: SpawnCommand,
    ) -> Result<SessionId, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { spawn_agent(connection, &id, command).await })
            })
            .await
    }

    async fn advance_agent(
        &self,
        id: &RunId,
        output: AgentOutput,
        allow_pending: bool,
    ) -> Result<AgentAdvance, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { advance_agent(connection, &id, output, allow_pending).await })
            })
            .await
    }

    async fn append_activity(
        &self,
        id: &RunId,
        activity: RunActivity,
    ) -> Result<CommandReceipt, MuzenError> {
        let id = id.clone();
        #[cfg(test)]
        let operation_probe = Arc::clone(&self.operation_probe);
        self.connection
            .call(move |connection| {
                Box::pin(async move {
                    #[cfg(test)]
                    operation_probe.signal_append_activity();
                    append_activity(connection, &id, activity).await
                })
            })
            .await
    }

    async fn cancel_requested(&self, id: &RunId) -> Result<bool, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move {
                    Ok(run_required(connection, &id)
                        .await?
                        .cancel_sequence
                        .is_some())
                })
            })
            .await
    }

    async fn request_cancel(
        &self,
        id: &RunId,
        reason: Option<&str>,
    ) -> Result<CommandReceipt, MuzenError> {
        let id = id.clone();
        let reason = reason.map(str::to_owned);
        self.connection
            .call(move |connection| {
                Box::pin(async move { request_cancel(connection, &id, reason.as_deref()).await })
            })
            .await
    }

    async fn finish_run(&self, id: &RunId, finish: FinishRun) -> Result<RunResult, MuzenError> {
        let id = id.clone();
        self.connection
            .call(move |connection| {
                Box::pin(async move { finish_run(connection, &id, finish).await })
            })
            .await
    }
}

async fn create_session(
    connection: &Connection,
    spec: SessionSpec,
    idempotency_key: Option<IdempotencyKey>,
    digest: [u8; 32],
) -> Result<SessionId, MuzenError> {
    let transaction = immediate(connection).await?;
    if let Some(id) = replay_id(
        &transaction,
        "session.create",
        idempotency_key.as_ref(),
        digest,
    )
    .await?
    {
        transaction.commit().await.map_err(sql_error)?;
        return SessionId::new(id).map_err(MuzenError::internal);
    }

    let id = new_session_id()?;
    let now = timestamp()?;
    let stored = StoredSession {
        spec: spec.clone(),
        snapshot: SessionSnapshot {
            id: id.clone(),
            status: SessionStatus::Open,
            active_run_id: None,
            created_at: now.clone(),
            updated_at: now,
            metadata: spec.metadata,
        },
        run_count: 0,
        lifetime_usage: Usage::default(),
    };
    insert_session(&transaction, &stored).await?;
    remember_id(
        &transaction,
        "session.create",
        idempotency_key.as_ref(),
        digest,
        id.as_str(),
    )
    .await?;
    transaction.commit().await.map_err(sql_error)?;
    Ok(id)
}

async fn messages(
    connection: &Connection,
    id: SessionId,
    page: MessagePage,
) -> Result<Page<AgentMessage>, MuzenError> {
    session_required(connection, &id).await?;
    let after = match page.after.as_deref() {
        None => -1,
        Some(cursor) => message_ordinal(connection, &id, cursor)
            .await?
            .ok_or_else(|| {
                MuzenError::not_found(format!(
                    "message cursor {cursor} was not found in agent session {id}"
                ))
            })?,
    };
    let limit = page.limit.map_or(100_i64, |limit| i64::from(limit.get()));
    let mut rows = connection
        .query(
            "SELECT message FROM muzen_agent_messages
             WHERE session_id = ?1 AND ordinal > ?2
             ORDER BY ordinal ASC LIMIT ?3",
            params![id.as_str(), after, limit + 1],
        )
        .await
        .map_err(sql_error)?;
    let mut items = Vec::<AgentMessage>::new();
    while let Some(row) = rows.next().await.map_err(sql_error)? {
        items.push(from_json(
            row.get::<String>(0).map_err(sql_error)?,
            "message",
        )?);
    }
    let has_more = items.len() > limit as usize;
    if has_more {
        items.pop();
    }
    let next = has_more
        .then(|| items.last().map(|message| message.id.clone()))
        .flatten();
    Ok(Page { items, next })
}

async fn archive_session(connection: &Connection, id: SessionId) -> Result<(), MuzenError> {
    let transaction = immediate(connection).await?;
    let mut session = session_required(&transaction, &id).await?;
    if session.snapshot.active_run_id.is_some() {
        return Err(MuzenError::conflict(format!(
            "agent session {id} has an active run"
        )));
    }
    session.snapshot.status = SessionStatus::Archived;
    session.snapshot.updated_at = timestamp()?;
    update_session(&transaction, &session).await?;
    transaction.commit().await.map_err(sql_error)?;
    Ok(())
}

async fn events_after(
    connection: &Connection,
    id: RunId,
    after: Option<u64>,
    limit: NonZeroU64,
) -> Result<Vec<AgentEvent>, MuzenError> {
    run_required(connection, &id).await?;
    let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
    let after = i64::try_from(after.unwrap_or(0)).unwrap_or(i64::MAX);
    let mut rows = connection
        .query(
            "SELECT event FROM muzen_agent_events
             WHERE run_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC LIMIT ?3",
            params![id.as_str(), after, limit],
        )
        .await
        .map_err(sql_error)?;
    let mut events = Vec::new();
    while let Some(row) = rows.next().await.map_err(sql_error)? {
        events.push(from_json(
            row.get::<String>(0).map_err(sql_error)?,
            "event",
        )?);
    }
    Ok(events)
}

#[derive(Debug)]
struct PersistedRun {
    stored: StoredRun,
    cancel_sequence: Option<u64>,
}

#[derive(Debug)]
struct PendingSession {
    stored: StoredSession,
    key: Option<IdempotencyKey>,
    digest: [u8; 32],
}

async fn create_run(connection: &Connection, spec: RunSpec) -> Result<RunId, MuzenError> {
    validate_run_spec(&spec)?;
    let digest = body_digest(&spec)?;
    let transaction = immediate(connection).await?;
    if let Some(id) = replay_id(
        &transaction,
        "run.create",
        spec.idempotency_key.as_ref(),
        digest,
    )
    .await?
    {
        transaction.commit().await.map_err(sql_error)?;
        return RunId::new(id).map_err(MuzenError::internal);
    }

    let now = timestamp()?;
    let mut pending = Vec::new();
    let mut root_ids = Vec::with_capacity(spec.roots.len());
    for root in &spec.roots {
        match root {
            RunRoot::Existing(root) => root_ids.push(root.session_id.clone()),
            RunRoot::New(root) => {
                let session_digest = body_digest(&root.session)?;
                let replay = replay_id(
                    &transaction,
                    "session.create",
                    root.idempotency_key.as_ref(),
                    session_digest,
                )
                .await?;
                let id = match replay {
                    Some(id) => SessionId::new(id).map_err(MuzenError::internal)?,
                    None => new_session_id()?,
                };
                if replay_session_missing(&transaction, &id).await? {
                    pending.push(PendingSession {
                        stored: StoredSession {
                            spec: root.session.clone(),
                            snapshot: SessionSnapshot {
                                id: id.clone(),
                                status: SessionStatus::Open,
                                active_run_id: None,
                                created_at: now.clone(),
                                updated_at: now.clone(),
                                metadata: root.session.metadata.clone(),
                            },
                            run_count: 0,
                            lifetime_usage: Usage::default(),
                        },
                        key: root.idempotency_key.clone(),
                        digest: session_digest,
                    });
                }
                root_ids.push(id);
            }
        }
    }
    if root_ids.iter().collect::<BTreeSet<_>>().len() != root_ids.len() {
        return Err(MuzenError::conflict(
            "an agent session cannot appear more than once in a run",
        ));
    }

    let mut sessions = BTreeMap::new();
    for id in &root_ids {
        let stored = match pending
            .iter()
            .find(|pending| pending.stored.snapshot.id == *id)
        {
            Some(pending) => pending.stored.clone(),
            None => session_required(&transaction, id).await?,
        };
        ensure_available(&stored)?;
        ensure_budget(&stored)?;
        stored
            .run_count
            .checked_add(1)
            .ok_or_else(|| MuzenError::internal("session run count overflow"))?;
        sessions.insert(id.clone(), stored);
    }

    let id = new_run_id()?;
    let mut agents = Vec::with_capacity(root_ids.len());
    let mut messages = Vec::with_capacity(root_ids.len());
    for (index, (session_id, root)) in root_ids.iter().zip(&spec.roots).enumerate() {
        let session = sessions
            .get_mut(session_id)
            .expect("all root sessions were loaded");
        session.snapshot.active_run_id = Some(id.clone());
        session.snapshot.updated_at = now.clone();
        session.run_count += 1;
        messages.push(AgentMessage {
            id: new_message_id(),
            session_id: session_id.clone(),
            role: MessageRole::User,
            content: root_input(root).content.clone(),
            created_at: now.clone(),
        });
        agents.push(AgentSnapshot {
            session_id: session_id.clone(),
            parent_session_id: None,
            path: vec![index as u32],
            status: AgentStatus::Queued,
            model: session.spec.agent.model.clone(),
            usage: Usage::default(),
        });
    }
    let mut events = vec![event(&id, 1, "run.queued", now.clone(), None)];
    for agent in &agents {
        events.push(event(
            &id,
            events.len() as u64 + 1,
            "agent.created",
            now.clone(),
            Some(agent.session_id.clone()),
        ));
    }
    let stored = StoredRun {
        spec: spec.clone(),
        snapshot: RunSnapshot {
            id: id.clone(),
            status: RunStatus::Queued,
            roots: root_ids,
            agents,
            last_sequence: events.len() as u64,
            created_at: now.clone(),
            updated_at: now,
        },
        result: None,
        outputs: Vec::new(),
        accepted_input_bytes: spec.roots.iter().try_fold(0_u64, |total, root| {
            total
                .checked_add(decoded_input_bytes(root_input(root))?)
                .ok_or_else(|| MuzenError::internal("run input byte count overflow"))
        })?,
    };

    for pending in &pending {
        insert_session(&transaction, &pending.stored).await?;
        remember_id(
            &transaction,
            "session.create",
            pending.key.as_ref(),
            pending.digest,
            pending.stored.snapshot.id.as_str(),
        )
        .await?;
    }
    for session in sessions.values() {
        update_session(&transaction, session).await?;
    }
    for message in &messages {
        insert_message(&transaction, message).await?;
    }
    insert_run(&transaction, &stored).await?;
    for event in &events {
        insert_event(&transaction, event).await?;
    }
    remember_id(
        &transaction,
        "run.create",
        spec.idempotency_key.as_ref(),
        digest,
        id.as_str(),
    )
    .await?;
    transaction.commit().await.map_err(sql_error)?;
    Ok(id)
}

async fn transition_running(
    connection: &Connection,
    id: &RunId,
) -> Result<CommandReceipt, MuzenError> {
    let transaction = immediate(connection).await?;
    let mut record = run_required(&transaction, id).await?;
    if record.stored.snapshot.status != RunStatus::Queued {
        return Err(MuzenError::conflict(format!(
            "run {id} cannot start from {:?}",
            record.stored.snapshot.status
        )));
    }
    ensure_sequence_capacity(
        &record.stored,
        record.stored.snapshot.agents.len() as u64 + 1,
    )?;
    let now = timestamp()?;
    let mut events = Vec::new();
    events.push(next_event(
        &mut record.stored,
        "run.started",
        now.clone(),
        None,
    )?);
    let session_ids = record
        .stored
        .snapshot
        .agents
        .iter()
        .map(|agent| agent.session_id.clone())
        .collect::<Vec<_>>();
    for agent in &mut record.stored.snapshot.agents {
        agent.status = AgentStatus::Running;
    }
    for session_id in session_ids {
        events.push(next_event(
            &mut record.stored,
            "agent.started",
            now.clone(),
            Some(session_id),
        )?);
    }
    record.stored.snapshot.status = RunStatus::Running;
    update_run(&transaction, &record).await?;
    for event in &events {
        insert_event(&transaction, event).await?;
    }
    transaction.commit().await.map_err(sql_error)?;
    receipt(events.last().expect("run started event").sequence)
}

async fn set_agent_status(
    connection: &Connection,
    id: &RunId,
    session_id: &SessionId,
    status: AgentStatus,
) -> Result<Option<CommandReceipt>, MuzenError> {
    let transaction = immediate(connection).await?;
    let mut record = run_required(&transaction, id).await?;
    ensure_live_run(&record, id)?;
    let index = agent_position(&record, id, session_id)?;
    if terminal_agent(record.stored.snapshot.agents[index].status) {
        return Err(MuzenError::conflict(format!(
            "agent session {session_id} is terminal"
        )));
    }
    if record.stored.snapshot.agents[index].status == status {
        transaction.commit().await.map_err(sql_error)?;
        return Ok(None);
    }
    record.stored.snapshot.agents[index].status = status;
    let now = timestamp()?;
    let mut events = Vec::new();
    if let Some(event_type) = match status {
        AgentStatus::Running => Some("agent.started"),
        AgentStatus::Waiting => Some("agent.waiting"),
        _ => None,
    } {
        events.push(next_event(
            &mut record.stored,
            event_type,
            now.clone(),
            Some(session_id.clone()),
        )?);
    }
    let run_status = aggregate_status(&record);
    if run_status != record.stored.snapshot.status {
        record.stored.snapshot.status = run_status;
        if run_status == RunStatus::Waiting {
            events.push(next_event(&mut record.stored, "run.waiting", now, None)?);
        }
    }
    update_run(&transaction, &record).await?;
    for event in &events {
        insert_event(&transaction, event).await?;
    }
    transaction.commit().await.map_err(sql_error)?;
    events
        .last()
        .map(|event| receipt(event.sequence))
        .transpose()
}

async fn accept_send(
    connection: &Connection,
    id: &RunId,
    command: SendCommand,
) -> Result<CommandReceipt, MuzenError> {
    validate_send_command(&command)?;
    let digest = body_digest(&(id, &command))?;
    let transaction = immediate(connection).await?;
    if let Some(sequence) = replay_id(
        &transaction,
        "run.send",
        command.idempotency_key.as_ref(),
        digest,
    )
    .await?
    {
        transaction.commit().await.map_err(sql_error)?;
        return receipt(
            sequence
                .parse()
                .map_err(|_| MuzenError::internal("invalid send replay sequence"))?,
        );
    }
    let mut record = run_required(&transaction, id).await?;
    ensure_live_run(&record, id)?;
    ensure_input_budget(&record, &command.input)?;
    let index = agent_position(&record, id, &command.session_id)?;
    let status = record.stored.snapshot.agents[index].status;
    if terminal_agent(status) {
        return Err(MuzenError::conflict(format!(
            "agent session {} is terminal",
            command.session_id
        )));
    }
    if command.delivery == MessageDelivery::Steer
        && !matches!(status, AgentStatus::Queued | AgentStatus::Running)
    {
        return Err(MuzenError::conflict(
            "steer is accepted only while the agent is executing",
        ));
    }
    let now = timestamp()?;
    let mut event = next_event(
        &mut record.stored,
        "message.accepted",
        now,
        Some(command.session_id.clone()),
    )?;
    event.payload.insert(
        "delivery".to_owned(),
        serde_json::to_value(command.delivery).expect("delivery serializes"),
    );
    record.stored.accepted_input_bytes += decoded_input_bytes(&command.input)?;
    let pending = PendingSend {
        sequence: event.sequence,
        session_id: command.session_id.clone(),
        input: command.input,
        delivery: command.delivery,
    };
    update_run(&transaction, &record).await?;
    insert_event(&transaction, &event).await?;
    transaction.execute(
        "INSERT INTO muzen_agent_sends (run_id, sequence, session_id, delivery, record, delivered) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        params![id.as_str(), i64::try_from(event.sequence).map_err(|_| MuzenError::internal("send sequence overflow"))?, command.session_id.as_str(), delivery_name(command.delivery), to_json(&pending, "pending send")?],
    ).await.map_err(sql_error)?;
    remember_id(
        &transaction,
        "run.send",
        command.idempotency_key.as_ref(),
        digest,
        &event.sequence.to_string(),
    )
    .await?;
    transaction.commit().await.map_err(sql_error)?;
    receipt(event.sequence)
}

async fn pending_send(
    connection: &Connection,
    id: &RunId,
    session_id: &SessionId,
) -> Result<Option<PendingSend>, MuzenError> {
    let record = run_required(connection, id).await?;
    ensure_tracked(&record, id, session_id)?;
    pending_send_on(connection, id, session_id).await
}

async fn pending_send_on(
    connection: &Connection,
    id: &RunId,
    session_id: &SessionId,
) -> Result<Option<PendingSend>, MuzenError> {
    let mut rows = connection.query(
        "SELECT record FROM muzen_agent_sends WHERE run_id = ?1 AND session_id = ?2 AND delivered = 0 ORDER BY CASE delivery WHEN 'steer' THEN 0 ELSE 1 END, sequence LIMIT 1",
        params![id.as_str(), session_id.as_str()],
    ).await.map_err(sql_error)?;
    rows.next()
        .await
        .map_err(sql_error)?
        .map(|row| from_json(row.get::<String>(0).map_err(sql_error)?, "pending send"))
        .transpose()
}

async fn deliver_send(
    connection: &Connection,
    id: &RunId,
    session_id: &SessionId,
    delivery: MessageDelivery,
) -> Result<bool, MuzenError> {
    let transaction = immediate(connection).await?;
    let record = run_required(&transaction, id).await?;
    ensure_live_run(&record, id)?;
    ensure_tracked(&record, id, session_id)?;
    let mut rows = transaction.query(
        "SELECT sequence, record FROM muzen_agent_sends WHERE run_id = ?1 AND session_id = ?2 AND delivery = ?3 AND delivered = 0 ORDER BY sequence LIMIT 1",
        params![id.as_str(), session_id.as_str(), delivery_name(delivery)],
    ).await.map_err(sql_error)?;
    let Some(row) = rows.next().await.map_err(sql_error)? else {
        transaction.commit().await.map_err(sql_error)?;
        return Ok(false);
    };
    let sequence = row.get::<i64>(0).map_err(sql_error)?;
    let pending: PendingSend = from_json(row.get::<String>(1).map_err(sql_error)?, "pending send")?;
    drop(rows);
    let now = timestamp()?;
    insert_message(
        &transaction,
        &AgentMessage {
            id: new_message_id(),
            session_id: session_id.clone(),
            role: MessageRole::User,
            content: pending.input.content,
            created_at: now.clone(),
        },
    )
    .await?;
    let mut session = session_required(&transaction, session_id).await?;
    session.snapshot.updated_at = now;
    update_session(&transaction, &session).await?;
    transaction
        .execute(
            "UPDATE muzen_agent_sends SET delivered = 1 WHERE run_id = ?1 AND sequence = ?2",
            params![id.as_str(), sequence],
        )
        .await
        .map_err(sql_error)?;
    transaction.commit().await.map_err(sql_error)?;
    Ok(true)
}

async fn spawn_agent(
    connection: &Connection,
    id: &RunId,
    command: SpawnCommand,
) -> Result<SessionId, MuzenError> {
    validate_spawn_command(&command)?;
    let digest = body_digest(&(id, &command))?;
    let transaction = immediate(connection).await?;
    if let Some(child) = replay_id(
        &transaction,
        "run.spawn",
        command.idempotency_key.as_ref(),
        digest,
    )
    .await?
    {
        transaction.commit().await.map_err(sql_error)?;
        return SessionId::new(child).map_err(MuzenError::internal);
    }
    let mut record = run_required(&transaction, id).await?;
    ensure_live_run(&record, id)?;
    ensure_input_budget(&record, &command.input)?;
    if record.stored.snapshot.agents.len() >= record.stored.spec.limits.max_agents.get() as usize {
        return Err(MuzenError::resource_exhausted("run maxAgents exhausted"));
    }
    let parent_index = agent_position(&record, id, &command.parent_session_id)?;
    let parent = &record.stored.snapshot.agents[parent_index];
    if terminal_agent(parent.status) {
        return Err(MuzenError::conflict(format!(
            "agent session {} is terminal",
            command.parent_session_id
        )));
    }
    if parent.path.len() as u32 > record.stored.spec.limits.max_depth {
        return Err(MuzenError::resource_exhausted("run maxDepth exhausted"));
    }
    let parent_session = session_required(&transaction, &command.parent_session_id).await?;
    validate_child_authority(&parent_session.spec, &command.agent)?;
    let direct_children = record
        .stored
        .snapshot
        .agents
        .iter()
        .filter(|agent| agent.parent_session_id.as_ref() == Some(&command.parent_session_id))
        .count() as u32;
    let mut path = parent.path.clone();
    path.push(direct_children);
    let child_id = new_session_id()?;
    let now = timestamp()?;
    let input_bytes = decoded_input_bytes(&command.input)?;
    let child_spec = SessionSpec {
        agent: command.agent,
        models: parent_session.spec.models,
        tool_providers: parent_session.spec.tool_providers,
        workspace: parent_session.spec.workspace,
        session_budget: parent_session.spec.session_budget,
        metadata: BTreeMap::new(),
    };
    validate_session_spec(&child_spec)?;
    let stored_session = StoredSession {
        spec: child_spec.clone(),
        snapshot: SessionSnapshot {
            id: child_id.clone(),
            status: SessionStatus::Open,
            active_run_id: Some(id.clone()),
            created_at: now.clone(),
            updated_at: now.clone(),
            metadata: child_spec.metadata.clone(),
        },
        run_count: 1,
        lifetime_usage: Usage::default(),
    };
    insert_session(&transaction, &stored_session).await?;
    insert_message(
        &transaction,
        &AgentMessage {
            id: new_message_id(),
            session_id: child_id.clone(),
            role: MessageRole::User,
            content: command.input.content,
            created_at: now.clone(),
        },
    )
    .await?;
    record.stored.accepted_input_bytes += input_bytes;
    record.stored.snapshot.agents.push(AgentSnapshot {
        session_id: child_id.clone(),
        parent_session_id: Some(command.parent_session_id),
        path,
        status: AgentStatus::Queued,
        model: child_spec.agent.model.clone(),
        usage: Usage::default(),
    });
    record
        .stored
        .snapshot
        .agents
        .sort_by(|left, right| left.path.cmp(&right.path));
    let event = next_event(
        &mut record.stored,
        "agent.created",
        now,
        Some(child_id.clone()),
    )?;
    update_run(&transaction, &record).await?;
    insert_event(&transaction, &event).await?;
    remember_id(
        &transaction,
        "run.spawn",
        command.idempotency_key.as_ref(),
        digest,
        child_id.as_str(),
    )
    .await?;
    transaction.commit().await.map_err(sql_error)?;
    Ok(child_id)
}

async fn advance_agent(
    connection: &Connection,
    id: &RunId,
    output: AgentOutput,
    allow_pending: bool,
) -> Result<AgentAdvance, MuzenError> {
    let transaction = immediate(connection).await?;
    let mut record = run_required(&transaction, id).await?;
    ensure_live_run(&record, id)?;
    let index = agent_position(&record, id, &output.session_id)?;
    if allow_pending {
        if let Some(pending) = pending_send_on(&transaction, id, &output.session_id).await? {
            transaction.commit().await.map_err(sql_error)?;
            return Ok(AgentAdvance::Pending(pending.delivery));
        }
    }
    if terminal_agent(record.stored.snapshot.agents[index].status) {
        transaction.commit().await.map_err(sql_error)?;
        return Ok(AgentAdvance::Finished);
    }
    record.stored.snapshot.agents[index].status = public_agent_status(output.status);
    record.stored.snapshot.agents[index].usage = output.usage.clone();
    record.stored.outputs.push(output.clone());
    record
        .stored
        .outputs
        .sort_by(|left, right| left.path.cmp(&right.path));
    let event = next_event(
        &mut record.stored,
        terminal_agent_event_type(output.status),
        timestamp()?,
        Some(output.session_id),
    )?;
    update_run(&transaction, &record).await?;
    insert_event(&transaction, &event).await?;
    transaction.commit().await.map_err(sql_error)?;
    Ok(AgentAdvance::Finished)
}

async fn append_activity(
    connection: &Connection,
    id: &RunId,
    activity: RunActivity,
) -> Result<CommandReceipt, MuzenError> {
    let transaction = immediate(connection).await?;
    let mut record = run_required(&transaction, id).await?;
    if is_terminal(record.stored.snapshot.status) {
        return Err(MuzenError::conflict(format!(
            "run {id} is already terminal"
        )));
    }
    if activity.events.is_empty() {
        return Err(MuzenError::internal(
            "run activity must contain at least one event",
        ));
    }
    ensure_sequence_capacity(&record.stored, activity.events.len() as u64)?;
    for event in &activity.events {
        if let Some(session_id) = &event.session_id {
            ensure_tracked(&record, id, session_id)?;
        }
    }
    let mut sessions = BTreeMap::new();
    for message in &activity.messages {
        ensure_tracked(&record, id, &message.session_id)?;
        if !sessions.contains_key(&message.session_id) {
            sessions.insert(
                message.session_id.clone(),
                session_required(&transaction, &message.session_id).await?,
            );
        }
    }
    let now = timestamp()?;
    let mut events = Vec::with_capacity(activity.events.len());
    for pending in activity.events {
        let mut event = next_event(
            &mut record.stored,
            &pending.event_type,
            now.clone(),
            pending.session_id,
        )?;
        event.payload = pending.payload;
        events.push(event);
    }
    update_run(&transaction, &record).await?;
    for message in &activity.messages {
        insert_message(&transaction, message).await?;
        sessions
            .get_mut(&message.session_id)
            .expect("activity session was loaded")
            .snapshot
            .updated_at = now.clone();
    }
    for session in sessions.values() {
        update_session(&transaction, session).await?;
    }
    for event in &events {
        insert_event(&transaction, event).await?;
    }
    transaction.commit().await.map_err(sql_error)?;
    receipt(events.last().expect("activity requires an event").sequence)
}

fn ensure_tracked(
    record: &PersistedRun,
    id: &RunId,
    session_id: &SessionId,
) -> Result<(), MuzenError> {
    if record
        .stored
        .snapshot
        .agents
        .iter()
        .any(|agent| &agent.session_id == session_id)
    {
        Ok(())
    } else {
        Err(MuzenError::not_found(format!(
            "agent session {session_id} is not tracked by run {id}"
        )))
    }
}

fn ensure_live_run(record: &PersistedRun, id: &RunId) -> Result<(), MuzenError> {
    if is_terminal(record.stored.snapshot.status) {
        Err(MuzenError::conflict(format!(
            "run {id} is already terminal"
        )))
    } else {
        Ok(())
    }
}

fn terminal_agent(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed
            | AgentStatus::Failed
            | AgentStatus::Cancelled
            | AgentStatus::BudgetExhausted
    )
}

fn agent_position(
    record: &PersistedRun,
    id: &RunId,
    session_id: &SessionId,
) -> Result<usize, MuzenError> {
    record
        .stored
        .snapshot
        .agents
        .iter()
        .position(|agent| &agent.session_id == session_id)
        .ok_or_else(|| {
            MuzenError::not_found(format!(
                "agent session {session_id} is not tracked by run {id}"
            ))
        })
}

fn aggregate_status(record: &PersistedRun) -> RunStatus {
    if record
        .stored
        .snapshot
        .agents
        .iter()
        .any(|agent| matches!(agent.status, AgentStatus::Queued | AgentStatus::Running))
    {
        RunStatus::Running
    } else if record
        .stored
        .snapshot
        .agents
        .iter()
        .any(|agent| agent.status == AgentStatus::Waiting)
    {
        RunStatus::Waiting
    } else {
        RunStatus::Running
    }
}

fn ensure_input_budget(
    record: &PersistedRun,
    input: &crate::agent_runtime::AgentInput,
) -> Result<(), MuzenError> {
    let total = record
        .stored
        .accepted_input_bytes
        .checked_add(decoded_input_bytes(input)?)
        .ok_or_else(|| MuzenError::internal("run input byte count overflow"))?;
    if total > record.stored.spec.limits.max_input_bytes.get() {
        Err(MuzenError::resource_exhausted(
            "run maxInputBytes exhausted",
        ))
    } else {
        Ok(())
    }
}

fn delivery_name(delivery: MessageDelivery) -> &'static str {
    match delivery {
        MessageDelivery::Steer => "steer",
        MessageDelivery::FollowUp => "follow_up",
    }
}

fn validate_child_authority(
    parent: &SessionSpec,
    child: &crate::agent_runtime::AgentDefinition,
) -> Result<(), MuzenError> {
    if !parent.models.iter().any(|model| model.id == child.model) {
        return Err(MuzenError::permission_denied(
            "child model is outside parent authority",
        ));
    }
    if let Some(parent_budget) = &parent.agent.budget {
        let Some(child_budget) = &child.budget else {
            return Err(MuzenError::permission_denied(
                "child budget exceeds parent authority",
            ));
        };
        if child_budget.max_turns > parent_budget.max_turns
            || child_budget.max_tool_calls > parent_budget.max_tool_calls
            || child_budget.max_prompt_tokens > parent_budget.max_prompt_tokens
            || child_budget.max_output_tokens > parent_budget.max_output_tokens
        {
            return Err(MuzenError::permission_denied(
                "child budget exceeds parent authority",
            ));
        }
    }
    for grant in &child.tools {
        let Some(parent_grant) =
            parent.agent.tools.iter().find(|candidate| {
                candidate.provider == grant.provider && candidate.tool == grant.tool
            })
        else {
            return Err(MuzenError::permission_denied(
                "child tool grant is outside parent authority",
            ));
        };
        if grant
            .effects
            .iter()
            .any(|effect| !parent_grant.effects.contains(effect))
            || matches!((grant.max_calls, parent_grant.max_calls), (Some(child), Some(parent)) if child > parent)
            || matches!((grant.max_calls, parent_grant.max_calls), (None, Some(_)))
        {
            return Err(MuzenError::permission_denied(
                "child tool grant exceeds parent authority",
            ));
        }
    }
    Ok(())
}

async fn request_cancel(
    connection: &Connection,
    id: &RunId,
    reason: Option<&str>,
) -> Result<CommandReceipt, MuzenError> {
    let transaction = immediate(connection).await?;
    let mut record = run_required(&transaction, id).await?;
    if let Some(sequence) = record.cancel_sequence {
        transaction.commit().await.map_err(sql_error)?;
        return receipt(sequence);
    }
    if is_terminal(record.stored.snapshot.status) {
        return Err(MuzenError::conflict(format!(
            "run {id} is already terminal"
        )));
    }
    ensure_sequence_capacity(&record.stored, 1)?;
    let now = timestamp()?;
    let mut event = next_event(&mut record.stored, "run.cancel_requested", now, None)?;
    if let Some(reason) = reason {
        event.payload.insert(
            "reason".to_owned(),
            serde_json::Value::String(reason.to_owned()),
        );
    }
    record.cancel_sequence = Some(event.sequence);
    update_run(&transaction, &record).await?;
    insert_event(&transaction, &event).await?;
    transaction.commit().await.map_err(sql_error)?;
    receipt(event.sequence)
}

async fn finish_run(
    connection: &Connection,
    id: &RunId,
    finish: FinishRun,
) -> Result<RunResult, MuzenError> {
    let transaction = immediate(connection).await?;
    let mut record = run_required(&transaction, id).await?;
    if is_terminal(record.stored.snapshot.status) {
        return Err(MuzenError::conflict(format!(
            "run {id} already has a terminal result"
        )));
    }
    if finish.outputs.len() != record.stored.snapshot.agents.len() {
        return Err(MuzenError::conflict(format!(
            "run {id} must contain exactly one output for every tracked agent"
        )));
    }
    for (agent, output) in record.stored.snapshot.agents.iter().zip(&finish.outputs) {
        if agent.session_id != output.session_id || agent.path != output.path {
            return Err(MuzenError::conflict(format!(
                "run {id} outputs must cover every tracked agent in AgentPath order"
            )));
        }
    }
    validate_aggregation(&record, &finish)?;
    ensure_sequence_capacity(&record.stored, finish.outputs.len() as u64 + 1)?;

    let mut sessions = BTreeMap::new();
    for agent in &record.stored.snapshot.agents {
        sessions
            .entry(agent.session_id.clone())
            .or_insert(session_required(&transaction, &agent.session_id).await?);
    }
    for root in &record.stored.snapshot.roots {
        let session = sessions
            .get(root)
            .ok_or_else(|| MuzenError::internal("run root session disappeared"))?;
        if session.snapshot.active_run_id.as_ref() != Some(id) {
            return Err(MuzenError::internal(format!(
                "run {id} lost ownership of root session {root}"
            )));
        }
    }
    for output in &finish.outputs {
        let session = sessions
            .get_mut(&output.session_id)
            .expect("output session was loaded");
        session.lifetime_usage = add_usage(&session.lifetime_usage, &output.usage)?;
    }

    let now = timestamp()?;
    let mut events = Vec::new();
    let newly_terminal = record
        .stored
        .snapshot
        .agents
        .iter()
        .zip(&finish.outputs)
        .filter(|(agent, _)| !terminal_agent(agent.status))
        .map(|(_, output)| output.clone())
        .collect::<Vec<_>>();
    for (agent, output) in record
        .stored
        .snapshot
        .agents
        .iter_mut()
        .zip(&finish.outputs)
    {
        agent.status = public_agent_status(output.status);
        agent.usage = output.usage.clone();
    }
    for output in &newly_terminal {
        events.push(next_event(
            &mut record.stored,
            terminal_agent_event_type(output.status),
            now.clone(),
            Some(output.session_id.clone()),
        )?);
    }
    record.stored.snapshot.status = public_run_status(finish.status);
    events.push(next_event(
        &mut record.stored,
        terminal_event_type(finish.status),
        now.clone(),
        None,
    )?);
    let result = RunResult {
        run_id: id.clone(),
        status: finish.status,
        outputs: finish.outputs,
        usage: finish.usage,
        artifacts: finish.artifacts,
        metadata: finish.metadata,
    };
    record.stored.result = Some(result.clone());
    for agent in &record.stored.snapshot.agents {
        let session = sessions
            .get_mut(&agent.session_id)
            .expect("agent session was loaded");
        session.snapshot.active_run_id = None;
        session.snapshot.updated_at = now.clone();
    }
    update_run(&transaction, &record).await?;
    for session in sessions.values() {
        update_session(&transaction, session).await?;
    }
    for event in &events {
        insert_event(&transaction, event).await?;
    }
    transaction.commit().await.map_err(sql_error)?;
    Ok(result)
}

fn validate_aggregation(record: &PersistedRun, finish: &FinishRun) -> Result<(), MuzenError> {
    let completed_agents = finish
        .outputs
        .iter()
        .filter(|output| output.status == TerminalAgentStatus::Completed)
        .count();
    let completed_roots = finish
        .outputs
        .iter()
        .filter(|output| {
            output.status == TerminalAgentStatus::Completed
                && record.stored.snapshot.roots.contains(&output.session_id)
        })
        .count();
    let expected = if completed_agents == finish.outputs.len() {
        TerminalRunStatus::Completed
    } else if completed_roots > 0 {
        TerminalRunStatus::Partial
    } else if record.cancel_sequence.is_some() {
        TerminalRunStatus::Cancelled
    } else {
        TerminalRunStatus::Failed
    };
    if finish.status != expected {
        return Err(MuzenError::conflict(format!(
            "run terminal status {:?} does not match aggregate status {:?}",
            finish.status, expected
        )));
    }
    Ok(())
}

fn ensure_available(session: &StoredSession) -> Result<(), MuzenError> {
    if session.snapshot.status != SessionStatus::Open {
        return Err(MuzenError::conflict(format!(
            "agent session {} is archived",
            session.snapshot.id
        )));
    }
    if let Some(run_id) = &session.snapshot.active_run_id {
        return Err(MuzenError::conflict(format!(
            "agent session {} is already active in run {run_id}",
            session.snapshot.id
        )));
    }
    Ok(())
}

fn ensure_budget(session: &StoredSession) -> Result<(), MuzenError> {
    let Some(budget) = &session.spec.session_budget else {
        return Ok(());
    };
    if budget
        .max_runs
        .is_some_and(|limit| session.run_count >= limit.get())
    {
        return Err(MuzenError::resource_exhausted(format!(
            "agent session {} exhausted its run budget",
            session.snapshot.id
        )));
    }
    let tokens = session
        .lifetime_usage
        .input_tokens
        .checked_add(session.lifetime_usage.output_tokens)
        .ok_or_else(|| MuzenError::internal("session lifetime token usage overflow"))?;
    if budget
        .max_lifetime_tokens
        .is_some_and(|limit| tokens >= limit.get())
    {
        return Err(MuzenError::resource_exhausted(format!(
            "agent session {} exhausted its lifetime token budget",
            session.snapshot.id
        )));
    }
    if budget
        .max_lifetime_tool_calls
        .is_some_and(|limit| session.lifetime_usage.tool_calls >= limit.get())
    {
        return Err(MuzenError::resource_exhausted(format!(
            "agent session {} exhausted its lifetime tool-call budget",
            session.snapshot.id
        )));
    }
    Ok(())
}

fn add_usage(current: &Usage, delta: &Usage) -> Result<Usage, MuzenError> {
    Ok(Usage {
        input_tokens: current
            .input_tokens
            .checked_add(delta.input_tokens)
            .ok_or_else(|| MuzenError::internal("session input token usage overflow"))?,
        output_tokens: current
            .output_tokens
            .checked_add(delta.output_tokens)
            .ok_or_else(|| MuzenError::internal("session output token usage overflow"))?,
        tool_calls: current
            .tool_calls
            .checked_add(delta.tool_calls)
            .ok_or_else(|| MuzenError::internal("session tool call usage overflow"))?,
    })
}

fn ensure_sequence_capacity(stored: &StoredRun, additional: u64) -> Result<(), MuzenError> {
    stored
        .snapshot
        .last_sequence
        .checked_add(additional)
        .ok_or_else(|| MuzenError::internal("run event sequence overflow"))?;
    Ok(())
}

fn next_event(
    stored: &mut StoredRun,
    event_type: &str,
    timestamp: String,
    session_id: Option<SessionId>,
) -> Result<AgentEvent, MuzenError> {
    let sequence = stored
        .snapshot
        .last_sequence
        .checked_add(1)
        .ok_or_else(|| MuzenError::internal("run event sequence overflow"))?;
    stored.snapshot.last_sequence = sequence;
    stored.snapshot.updated_at = timestamp.clone();
    Ok(event(
        &stored.snapshot.id,
        sequence,
        event_type,
        timestamp,
        session_id,
    ))
}

fn event(
    run_id: &RunId,
    sequence: u64,
    event_type: &str,
    timestamp: String,
    session_id: Option<SessionId>,
) -> AgentEvent {
    AgentEvent {
        run_id: run_id.clone(),
        sequence,
        event_type: event_type.to_owned(),
        timestamp,
        session_id,
        payload: BTreeMap::new(),
    }
}
