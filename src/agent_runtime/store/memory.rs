use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::support::{
    is_terminal, new_message_id, new_run_id, new_session_id, public_agent_status,
    public_run_status, root_input, terminal_agent_event_type, terminal_event_type, timestamp,
};
use super::{body_digest, receipt, AgentStore, FinishRun, StoredRun, StoredSession};
use crate::agent_runtime::validation::{validate_run_spec, validate_session_spec};
use crate::agent_runtime::{
    AgentEvent, AgentMessage, AgentSnapshot, AgentStatus, CommandReceipt, IdempotencyKey,
    MessagePage, MessageRole, MuzenError, Page, RunId, RunResult, RunRoot, RunSnapshot, RunSpec,
    RunStatus, SessionId, SessionSnapshot, SessionSpec, SessionStatus, TerminalAgentStatus,
    TerminalRunStatus, Usage,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum IdempotencyScope {
    CreateSession,
    CreateRun,
}

#[derive(Debug, Clone)]
struct IdempotencyRecord {
    digest: [u8; 32],
    resource_id: String,
}

#[derive(Debug, Clone)]
struct RunRecord {
    stored: StoredRun,
    events: Vec<AgentEvent>,
    cancel_receipt: Option<CommandReceipt>,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    stored: StoredSession,
    messages: Vec<AgentMessage>,
}

#[derive(Debug, Default)]
struct State {
    sessions: BTreeMap<SessionId, SessionRecord>,
    runs: BTreeMap<RunId, RunRecord>,
    idempotency: BTreeMap<(IdempotencyScope, IdempotencyKey), IdempotencyRecord>,
}

#[derive(Debug, Default)]
pub(crate) struct MemoryAgentStore {
    state: Mutex<State>,
}

impl MemoryAgentStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentStore for MemoryAgentStore {
    async fn create_session(
        &self,
        spec: SessionSpec,
        idempotency_key: Option<&IdempotencyKey>,
    ) -> Result<SessionId, MuzenError> {
        validate_session_spec(&spec)?;
        let digest = body_digest(&spec)?;
        let mut state = self.state.lock();
        if let Some(id) = replay_id(
            &state,
            IdempotencyScope::CreateSession,
            idempotency_key,
            digest,
        )? {
            return SessionId::new(id).map_err(MuzenError::internal);
        }

        let id = new_session_id()?;
        let now = timestamp()?;
        let snapshot = SessionSnapshot {
            id: id.clone(),
            status: SessionStatus::Open,
            active_run_id: None,
            created_at: now.clone(),
            updated_at: now,
            metadata: spec.metadata.clone(),
        };
        state.sessions.insert(
            id.clone(),
            SessionRecord {
                stored: StoredSession {
                    spec,
                    snapshot,
                    run_count: 0,
                    lifetime_usage: Usage::default(),
                },
                messages: Vec::new(),
            },
        );
        remember_idempotency(
            &mut state,
            IdempotencyScope::CreateSession,
            idempotency_key,
            digest,
            id.as_str(),
        );
        Ok(id)
    }

    async fn session(&self, id: &SessionId) -> Result<StoredSession, MuzenError> {
        self.state
            .lock()
            .sessions
            .get(id)
            .map(|record| record.stored.clone())
            .ok_or_else(|| MuzenError::not_found(format!("agent session {id} was not found")))
    }

    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError> {
        let state = self.state.lock();
        let record = state
            .sessions
            .get(id)
            .ok_or_else(|| MuzenError::not_found(format!("agent session {id} was not found")))?;
        let start = match page.after.as_deref() {
            None => 0,
            Some(cursor) => record
                .messages
                .iter()
                .position(|message| message.id == cursor)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    MuzenError::not_found(format!(
                        "message cursor {cursor} was not found in agent session {id}"
                    ))
                })?,
        };
        let limit = page.limit.map_or(100, |limit| limit.get() as usize);
        let end = start.saturating_add(limit).min(record.messages.len());
        let items = record.messages[start..end].to_vec();
        let next = (end < record.messages.len())
            .then(|| items.last().map(|message| message.id.clone()))
            .flatten();
        Ok(Page { items, next })
    }

    async fn archive_session(&self, id: &SessionId) -> Result<(), MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        let session = state
            .sessions
            .get_mut(id)
            .ok_or_else(|| MuzenError::not_found(format!("agent session {id} was not found")))?;
        if session.stored.snapshot.active_run_id.is_some() {
            return Err(MuzenError::conflict(format!(
                "agent session {id} has an active run"
            )));
        }
        session.stored.snapshot.status = SessionStatus::Archived;
        session.stored.snapshot.updated_at = now;
        Ok(())
    }

    async fn create_run(&self, spec: RunSpec) -> Result<RunId, MuzenError> {
        validate_run_spec(&spec)?;
        let digest = body_digest(&spec)?;
        let mut state = self.state.lock();
        if let Some(id) = replay_id(
            &state,
            IdempotencyScope::CreateRun,
            spec.idempotency_key.as_ref(),
            digest,
        )? {
            return RunId::new(id).map_err(MuzenError::internal);
        }

        let mut root_ids = Vec::with_capacity(spec.roots.len());
        let mut new_sessions = Vec::new();
        for root in &spec.roots {
            match root {
                RunRoot::Existing(root) => root_ids.push(root.session_id.clone()),
                RunRoot::New(root) => {
                    let session_digest = body_digest(&root.session)?;
                    let replay = replay_id(
                        &state,
                        IdempotencyScope::CreateSession,
                        root.idempotency_key.as_ref(),
                        session_digest,
                    )?;
                    let id = match replay {
                        Some(id) => SessionId::new(id).map_err(MuzenError::internal)?,
                        None => new_session_id()?,
                    };
                    root_ids.push(id.clone());
                    if state.sessions.get(&id).is_none() {
                        new_sessions.push((
                            id,
                            root.session.clone(),
                            root.idempotency_key.clone(),
                            session_digest,
                        ));
                    }
                }
            }
        }
        ensure_unique_roots(&root_ids)?;
        for id in &root_ids {
            if new_sessions.iter().any(|(new_id, ..)| new_id == id) {
                continue;
            }
            let session = state.sessions.get(id).ok_or_else(|| {
                MuzenError::not_found(format!("root agent session {id} was not found"))
            })?;
            ensure_session_available(&session.stored)?;
            ensure_session_budget(&session.stored)?;
            session
                .stored
                .run_count
                .checked_add(1)
                .ok_or_else(|| MuzenError::internal("session run count overflow"))?;
        }

        let id = new_run_id()?;
        let now = timestamp()?;
        for (session_id, session_spec, idempotency_key, session_digest) in new_sessions {
            let snapshot = SessionSnapshot {
                id: session_id.clone(),
                status: SessionStatus::Open,
                active_run_id: None,
                created_at: now.clone(),
                updated_at: now.clone(),
                metadata: session_spec.metadata.clone(),
            };
            state.sessions.insert(
                session_id.clone(),
                SessionRecord {
                    stored: StoredSession {
                        spec: session_spec,
                        snapshot,
                        run_count: 0,
                        lifetime_usage: Usage::default(),
                    },
                    messages: Vec::new(),
                },
            );
            remember_idempotency(
                &mut state,
                IdempotencyScope::CreateSession,
                idempotency_key.as_ref(),
                session_digest,
                session_id.as_str(),
            );
        }

        let mut agents = Vec::with_capacity(root_ids.len());
        for (index, (session_id, root)) in root_ids.iter().zip(&spec.roots).enumerate() {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                MuzenError::internal("new root session disappeared during run creation")
            })?;
            session.stored.snapshot.active_run_id = Some(id.clone());
            session.stored.snapshot.updated_at = now.clone();
            session.stored.run_count = session
                .stored
                .run_count
                .checked_add(1)
                .expect("run count was checked before mutation");
            session.messages.push(AgentMessage {
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
                model: session.stored.spec.agent.model.clone(),
                usage: Usage::default(),
            });
        }

        let mut events = vec![AgentEvent {
            run_id: id.clone(),
            sequence: 1,
            event_type: "run.queued".to_owned(),
            timestamp: now.clone(),
            session_id: None,
            payload: BTreeMap::new(),
        }];
        for agent in &agents {
            events.push(AgentEvent {
                run_id: id.clone(),
                sequence: events.len() as u64 + 1,
                event_type: "agent.created".to_owned(),
                timestamp: now.clone(),
                session_id: Some(agent.session_id.clone()),
                payload: BTreeMap::new(),
            });
        }
        let last_sequence = events.len() as u64;
        let snapshot = RunSnapshot {
            id: id.clone(),
            status: RunStatus::Queued,
            roots: root_ids,
            agents,
            last_sequence,
            created_at: now.clone(),
            updated_at: now,
        };
        state.runs.insert(
            id.clone(),
            RunRecord {
                stored: StoredRun {
                    spec: spec.clone(),
                    snapshot,
                    result: None,
                },
                events,
                cancel_receipt: None,
            },
        );
        remember_idempotency(
            &mut state,
            IdempotencyScope::CreateRun,
            spec.idempotency_key.as_ref(),
            digest,
            id.as_str(),
        );
        Ok(id)
    }

    async fn run(&self, id: &RunId) -> Result<StoredRun, MuzenError> {
        self.state
            .lock()
            .runs
            .get(id)
            .map(|record| record.stored.clone())
            .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))
    }

    async fn events_after(
        &self,
        id: &RunId,
        after: Option<u64>,
        limit: NonZeroU64,
    ) -> Result<Vec<AgentEvent>, MuzenError> {
        let state = self.state.lock();
        let record = state
            .runs
            .get(id)
            .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?;
        let after = after.unwrap_or(0);
        let limit = usize::try_from(limit.get()).unwrap_or(usize::MAX);
        Ok(record
            .events
            .iter()
            .filter(|event| event.sequence > after)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn mark_run_running(&self, id: &RunId) -> Result<CommandReceipt, MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        let record = run_mut(&mut state, id)?;
        if record.stored.snapshot.status != RunStatus::Queued {
            return Err(MuzenError::conflict(format!(
                "run {id} cannot start from {:?}",
                record.stored.snapshot.status
            )));
        }
        ensure_event_capacity(record, record.stored.snapshot.agents.len() as u64 + 1)?;
        record.stored.snapshot.status = RunStatus::Running;
        append_event(record, now.clone(), "run.started", None, BTreeMap::new())?;
        for agent in &mut record.stored.snapshot.agents {
            agent.status = AgentStatus::Running;
        }
        let session_ids = record
            .stored
            .snapshot
            .agents
            .iter()
            .map(|agent| agent.session_id.clone())
            .collect::<Vec<_>>();
        let mut last = None;
        for session_id in session_ids {
            last = Some(append_event(
                record,
                now.clone(),
                "agent.started",
                Some(session_id),
                BTreeMap::new(),
            )?);
        }
        last.ok_or_else(|| MuzenError::internal("a run must contain at least one agent"))
    }

    async fn request_cancel(
        &self,
        id: &RunId,
        reason: Option<&str>,
    ) -> Result<CommandReceipt, MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        let record = run_mut(&mut state, id)?;
        if let Some(receipt) = &record.cancel_receipt {
            return Ok(receipt.clone());
        }
        if is_terminal(record.stored.snapshot.status) {
            return Err(MuzenError::conflict(format!(
                "run {id} is already terminal"
            )));
        }
        let mut payload = BTreeMap::new();
        if let Some(reason) = reason {
            payload.insert("reason".to_owned(), Value::String(reason.to_owned()));
        }
        let receipt = append_event(record, now, "run.cancel_requested", None, payload)?;
        record.cancel_receipt = Some(receipt.clone());
        Ok(receipt)
    }

    async fn finish_run(&self, id: &RunId, finish: FinishRun) -> Result<RunResult, MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        let (roots, usage_updates) = validate_finish(&state, id, &finish)?;
        let result = {
            let record = run_mut(&mut state, id)?;
            let status = public_run_status(finish.status);
            record.stored.snapshot.status = status;
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
                append_event(
                    record,
                    now.clone(),
                    terminal_agent_event_type(output.status),
                    Some(output.session_id.clone()),
                    BTreeMap::new(),
                )?;
            }
            let result = RunResult {
                run_id: id.clone(),
                status: finish.status,
                outputs: finish.outputs,
                usage: finish.usage,
                artifacts: finish.artifacts,
                metadata: finish.metadata,
            };
            record.stored.result = Some(result.clone());
            let event_type = terminal_event_type(finish.status);
            append_event(record, now.clone(), event_type, None, BTreeMap::new())?;
            result
        };
        for session_id in roots {
            let session = state
                .sessions
                .get_mut(&session_id)
                .expect("finish validation proved root session existence");
            session.stored.snapshot.active_run_id = None;
            session.stored.snapshot.updated_at = now.clone();
        }
        for (session_id, usage) in usage_updates {
            let session = state
                .sessions
                .get_mut(&session_id)
                .expect("finish validation proved output session existence");
            session.stored.lifetime_usage = usage;
        }
        Ok(result)
    }
}

fn replay_id(
    state: &State,
    scope: IdempotencyScope,
    key: Option<&IdempotencyKey>,
    digest: [u8; 32],
) -> Result<Option<String>, MuzenError> {
    let Some(key) = key else {
        return Ok(None);
    };
    let Some(record) = state.idempotency.get(&(scope, key.clone())) else {
        return Ok(None);
    };
    if record.digest != digest {
        return Err(MuzenError::conflict(format!(
            "idempotency key {key} was already used with a different body"
        )));
    }
    Ok(Some(record.resource_id.clone()))
}

fn remember_idempotency(
    state: &mut State,
    scope: IdempotencyScope,
    key: Option<&IdempotencyKey>,
    digest: [u8; 32],
    resource_id: &str,
) {
    if let Some(key) = key {
        state.idempotency.insert(
            (scope, key.clone()),
            IdempotencyRecord {
                digest,
                resource_id: resource_id.to_owned(),
            },
        );
    }
}

fn ensure_session_available(session: &StoredSession) -> Result<(), MuzenError> {
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

fn ensure_session_budget(session: &StoredSession) -> Result<(), MuzenError> {
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
    let lifetime_tokens = session
        .lifetime_usage
        .input_tokens
        .checked_add(session.lifetime_usage.output_tokens)
        .ok_or_else(|| MuzenError::internal("session lifetime token usage overflow"))?;
    if budget
        .max_lifetime_tokens
        .is_some_and(|limit| lifetime_tokens >= limit.get())
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

fn ensure_unique_roots(root_ids: &[SessionId]) -> Result<(), MuzenError> {
    let unique = root_ids.iter().collect::<BTreeSet<_>>();
    if unique.len() != root_ids.len() {
        return Err(MuzenError::conflict(
            "an agent session cannot appear more than once in a run",
        ));
    }
    Ok(())
}

fn run_mut<'a>(state: &'a mut State, id: &RunId) -> Result<&'a mut RunRecord, MuzenError> {
    state
        .runs
        .get_mut(id)
        .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))
}

fn validate_finish(
    state: &State,
    id: &RunId,
    finish: &FinishRun,
) -> Result<(Vec<SessionId>, Vec<(SessionId, Usage)>), MuzenError> {
    let record = state
        .runs
        .get(id)
        .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?;
    if is_terminal(record.stored.snapshot.status) {
        return Err(MuzenError::conflict(format!(
            "run {id} already has a terminal result"
        )));
    }
    ensure_event_capacity(record, finish.outputs.len() as u64 + 1)?;
    for root_id in &record.stored.snapshot.roots {
        let session = state.sessions.get(root_id).ok_or_else(|| {
            MuzenError::internal("run root session disappeared before completion")
        })?;
        if session.stored.snapshot.active_run_id.as_ref() != Some(id) {
            return Err(MuzenError::internal(format!(
                "run {id} lost ownership of root session {root_id}"
            )));
        }
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
    validate_aggregation(record, finish)?;

    let mut updates = BTreeMap::<SessionId, Usage>::new();
    for output in &finish.outputs {
        let session = state
            .sessions
            .get(&output.session_id)
            .ok_or_else(|| MuzenError::internal("run output references a missing agent session"))?;
        let current = updates
            .get(&output.session_id)
            .unwrap_or(&session.stored.lifetime_usage);
        let usage = Usage {
            input_tokens: current
                .input_tokens
                .checked_add(output.usage.input_tokens)
                .ok_or_else(|| MuzenError::internal("session input token usage overflow"))?,
            output_tokens: current
                .output_tokens
                .checked_add(output.usage.output_tokens)
                .ok_or_else(|| MuzenError::internal("session output token usage overflow"))?,
            tool_calls: current
                .tool_calls
                .checked_add(output.usage.tool_calls)
                .ok_or_else(|| MuzenError::internal("session tool call usage overflow"))?,
        };
        updates.insert(output.session_id.clone(), usage);
    }
    Ok((
        record.stored.snapshot.roots.clone(),
        updates.into_iter().collect(),
    ))
}

fn validate_aggregation(record: &RunRecord, finish: &FinishRun) -> Result<(), MuzenError> {
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
    } else if record.cancel_receipt.is_some() {
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

fn append_event(
    record: &mut RunRecord,
    timestamp: String,
    event_type: &str,
    session_id: Option<SessionId>,
    payload: BTreeMap<String, Value>,
) -> Result<CommandReceipt, MuzenError> {
    let sequence = record
        .stored
        .snapshot
        .last_sequence
        .checked_add(1)
        .ok_or_else(|| MuzenError::internal("run event sequence overflow"))?;
    record.stored.snapshot.last_sequence = sequence;
    record.stored.snapshot.updated_at = timestamp.clone();
    record.events.push(AgentEvent {
        run_id: record.stored.snapshot.id.clone(),
        sequence,
        event_type: event_type.to_owned(),
        timestamp,
        session_id,
        payload,
    });
    receipt(sequence)
}

fn ensure_event_capacity(record: &RunRecord, additional: u64) -> Result<(), MuzenError> {
    record
        .stored
        .snapshot
        .last_sequence
        .checked_add(additional)
        .ok_or_else(|| MuzenError::internal("run event sequence overflow"))?;
    Ok(())
}
