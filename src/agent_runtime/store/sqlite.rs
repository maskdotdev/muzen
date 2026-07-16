use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use libsql::{params, Builder, Connection};

use super::support::{
    is_terminal, new_message_id, new_run_id, new_session_id, public_agent_status,
    public_run_status, root_input, terminal_agent_event_type, terminal_event_type, timestamp,
};
use super::{body_digest, receipt, AgentStore, FinishRun, RunActivity, StoredRun, StoredSession};
use crate::agent_runtime::validation::{validate_run_spec, validate_session_spec};
use crate::agent_runtime::{
    AgentEvent, AgentMessage, AgentSnapshot, AgentStatus, CommandReceipt, IdempotencyKey,
    MessagePage, MessageRole, MuzenError, Page, RunId, RunResult, RunRoot, RunSnapshot, RunSpec,
    RunStatus, SessionId, SessionSnapshot, SessionSpec, SessionStatus, TerminalAgentStatus,
    TerminalRunStatus, Usage,
};

mod persistence;

use persistence::*;

const SCHEMA_VERSION: i64 = 1;

pub(crate) struct SqliteAgentStore {
    _database: libsql::Database,
    connection: tokio::sync::Mutex<Connection>,
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
        let store = Self {
            _database: database,
            connection: tokio::sync::Mutex::new(connection),
        };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), MuzenError> {
        let connection = self.connection.lock().await;
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
        Ok(())
    }
}

#[async_trait]
impl AgentStore for SqliteAgentStore {
    async fn create_session(
        &self,
        spec: SessionSpec,
        idempotency_key: Option<&IdempotencyKey>,
    ) -> Result<SessionId, MuzenError> {
        validate_session_spec(&spec)?;
        let digest = body_digest(&spec)?;
        let connection = self.connection.lock().await;
        let transaction = immediate(&connection).await?;
        if let Some(id) = replay_id(&transaction, "session.create", idempotency_key, digest).await?
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
            idempotency_key,
            digest,
            id.as_str(),
        )
        .await?;
        transaction.commit().await.map_err(sql_error)?;
        Ok(id)
    }

    async fn session(&self, id: &SessionId) -> Result<StoredSession, MuzenError> {
        let connection = self.connection.lock().await;
        session_required(&connection, id).await
    }

    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError> {
        let connection = self.connection.lock().await;
        session_required(&connection, id).await?;
        let after = match page.after.as_deref() {
            None => -1,
            Some(cursor) => message_ordinal(&connection, id, cursor)
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

    async fn archive_session(&self, id: &SessionId) -> Result<(), MuzenError> {
        let connection = self.connection.lock().await;
        let transaction = immediate(&connection).await?;
        let mut session = session_required(&transaction, id).await?;
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

    async fn create_run(&self, spec: RunSpec) -> Result<RunId, MuzenError> {
        create_run(self, spec).await
    }

    async fn run(&self, id: &RunId) -> Result<StoredRun, MuzenError> {
        let connection = self.connection.lock().await;
        run_required(&connection, id)
            .await
            .map(|record| record.stored)
    }

    async fn events_after(
        &self,
        id: &RunId,
        after: Option<u64>,
        limit: NonZeroU64,
    ) -> Result<Vec<AgentEvent>, MuzenError> {
        let connection = self.connection.lock().await;
        run_required(&connection, id).await?;
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

    async fn mark_run_running(&self, id: &RunId) -> Result<CommandReceipt, MuzenError> {
        transition_running(self, id).await
    }

    async fn append_activity(
        &self,
        id: &RunId,
        activity: RunActivity,
    ) -> Result<CommandReceipt, MuzenError> {
        append_activity(self, id, activity).await
    }

    async fn cancel_requested(&self, id: &RunId) -> Result<bool, MuzenError> {
        let connection = self.connection.lock().await;
        Ok(run_required(&connection, id)
            .await?
            .cancel_sequence
            .is_some())
    }

    async fn request_cancel(
        &self,
        id: &RunId,
        reason: Option<&str>,
    ) -> Result<CommandReceipt, MuzenError> {
        request_cancel(self, id, reason).await
    }

    async fn finish_run(&self, id: &RunId, finish: FinishRun) -> Result<RunResult, MuzenError> {
        finish_run(self, id, finish).await
    }
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

async fn create_run(store: &SqliteAgentStore, spec: RunSpec) -> Result<RunId, MuzenError> {
    validate_run_spec(&spec)?;
    let digest = body_digest(&spec)?;
    let connection = store.connection.lock().await;
    let transaction = immediate(&connection).await?;
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
    store: &SqliteAgentStore,
    id: &RunId,
) -> Result<CommandReceipt, MuzenError> {
    let connection = store.connection.lock().await;
    let transaction = immediate(&connection).await?;
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
    receipt(events.last().expect("run has agents").sequence)
}

async fn append_activity(
    store: &SqliteAgentStore,
    id: &RunId,
    activity: RunActivity,
) -> Result<CommandReceipt, MuzenError> {
    let connection = store.connection.lock().await;
    let transaction = immediate(&connection).await?;
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

async fn request_cancel(
    store: &SqliteAgentStore,
    id: &RunId,
    reason: Option<&str>,
) -> Result<CommandReceipt, MuzenError> {
    let connection = store.connection.lock().await;
    let transaction = immediate(&connection).await?;
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
    store: &SqliteAgentStore,
    id: &RunId,
    finish: FinishRun,
) -> Result<RunResult, MuzenError> {
    let connection = store.connection.lock().await;
    let transaction = immediate(&connection).await?;
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
    for output in &finish.outputs {
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
    for root in &record.stored.snapshot.roots {
        let session = sessions.get_mut(root).expect("root session was loaded");
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
