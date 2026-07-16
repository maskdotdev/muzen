use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

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
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum IdempotencyScope {
    CreateSession,
    CreateRun,
    RunSend,
    RunSpawn,
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
    sends: Vec<(PendingSend, bool)>,
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
                    outputs: Vec::new(),
                    accepted_input_bytes: spec.roots.iter().try_fold(0_u64, |total, root| {
                        total
                            .checked_add(decoded_input_bytes(root_input(root))?)
                            .ok_or_else(|| MuzenError::internal("run input byte count overflow"))
                    })?,
                },
                events,
                cancel_receipt: None,
                sends: Vec::new(),
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

    async fn set_agent_status(
        &self,
        id: &RunId,
        session_id: &SessionId,
        status: AgentStatus,
    ) -> Result<Option<CommandReceipt>, MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        let record = run_mut(&mut state, id)?;
        ensure_nonterminal_run(record, id)?;
        ensure_event_capacity(record, 2)?;
        let index = agent_index(record, id, session_id)?;
        if is_terminal_agent(record.stored.snapshot.agents[index].status) {
            return Err(MuzenError::conflict(format!(
                "agent session {session_id} is terminal"
            )));
        }
        if record.stored.snapshot.agents[index].status == status {
            return Ok(None);
        }
        record.stored.snapshot.agents[index].status = status;
        let event_type = match status {
            AgentStatus::Running => Some("agent.started"),
            AgentStatus::Waiting => Some("agent.waiting"),
            _ => None,
        };
        let mut last = event_type
            .map(|event_type| {
                append_event(
                    record,
                    now.clone(),
                    event_type,
                    Some(session_id.clone()),
                    BTreeMap::new(),
                )
            })
            .transpose()?;
        let run_status = aggregate_live_status(record);
        if run_status != record.stored.snapshot.status {
            record.stored.snapshot.status = run_status;
            if run_status == RunStatus::Waiting {
                last = Some(append_event(
                    record,
                    now,
                    "run.waiting",
                    None,
                    BTreeMap::new(),
                )?);
            }
        }
        Ok(last)
    }

    async fn accept_send(
        &self,
        id: &RunId,
        command: SendCommand,
    ) -> Result<CommandReceipt, MuzenError> {
        validate_send_command(&command)?;
        let digest = body_digest(&(id, &command))?;
        let mut state = self.state.lock();
        if let Some(sequence) = replay_id(
            &state,
            IdempotencyScope::RunSend,
            command.idempotency_key.as_ref(),
            digest,
        )? {
            return receipt(
                sequence
                    .parse()
                    .map_err(|_| MuzenError::internal("invalid send replay sequence"))?,
            );
        }
        validate_input_budget(&state, id, &command.input)?;
        let input_bytes = decoded_input_bytes(&command.input)?;
        let now = timestamp()?;
        let record = run_mut(&mut state, id)?;
        ensure_nonterminal_run(record, id)?;
        let index = agent_index(record, id, &command.session_id)?;
        let status = record.stored.snapshot.agents[index].status;
        if is_terminal_agent(status) {
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
        let mut payload = BTreeMap::new();
        payload.insert(
            "delivery".to_owned(),
            serde_json::to_value(command.delivery).expect("delivery serializes"),
        );
        let accepted = append_event(
            record,
            now,
            "message.accepted",
            Some(command.session_id.clone()),
            payload,
        )?;
        record.stored.accepted_input_bytes = record
            .stored
            .accepted_input_bytes
            .checked_add(input_bytes)
            .ok_or_else(|| MuzenError::internal("run input byte count overflow"))?;
        record.sends.push((
            PendingSend {
                sequence: accepted.sequence.get(),
                session_id: command.session_id.clone(),
                input: command.input,
                delivery: command.delivery,
            },
            false,
        ));
        remember_idempotency(
            &mut state,
            IdempotencyScope::RunSend,
            command.idempotency_key.as_ref(),
            digest,
            &accepted.sequence.get().to_string(),
        );
        Ok(accepted)
    }

    async fn pending_send(
        &self,
        id: &RunId,
        session_id: &SessionId,
    ) -> Result<Option<PendingSend>, MuzenError> {
        let state = self.state.lock();
        let record = state
            .runs
            .get(id)
            .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?;
        ensure_tracked(record, id, session_id)?;
        Ok(next_pending(&record.sends, session_id).cloned())
    }

    async fn deliver_send(
        &self,
        id: &RunId,
        session_id: &SessionId,
        delivery: MessageDelivery,
    ) -> Result<bool, MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        let input = {
            let record = run_mut(&mut state, id)?;
            ensure_nonterminal_run(record, id)?;
            let Some((send, delivered)) = record.sends.iter_mut().find(|(send, delivered)| {
                !*delivered && &send.session_id == session_id && send.delivery == delivery
            }) else {
                return Ok(false);
            };
            *delivered = true;
            send.input.clone()
        };
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| MuzenError::internal("tracked session disappeared"))?;
        session.stored.snapshot.updated_at = now.clone();
        session.messages.push(user_message(session_id, input, now));
        Ok(true)
    }

    async fn spawn_agent(
        &self,
        id: &RunId,
        command: SpawnCommand,
    ) -> Result<SessionId, MuzenError> {
        validate_spawn_command(&command)?;
        let digest = body_digest(&(id, &command))?;
        let mut state = self.state.lock();
        if let Some(child) = replay_id(
            &state,
            IdempotencyScope::RunSpawn,
            command.idempotency_key.as_ref(),
            digest,
        )? {
            return SessionId::new(child).map_err(MuzenError::internal);
        }
        validate_input_budget(&state, id, &command.input)?;
        let (parent_spec, child_path) = validate_spawn(&state, id, &command)?;
        let input_bytes = decoded_input_bytes(&command.input)?;
        let child_id = new_session_id()?;
        let now = timestamp()?;
        let child_spec = SessionSpec {
            agent: command.agent,
            models: parent_spec.models,
            tool_providers: parent_spec.tool_providers,
            workspace: parent_spec.workspace,
            session_budget: parent_spec.session_budget,
            metadata: BTreeMap::new(),
        };
        validate_session_spec(&child_spec)?;
        state.sessions.insert(
            child_id.clone(),
            SessionRecord {
                stored: StoredSession {
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
                },
                messages: vec![user_message(&child_id, command.input, now.clone())],
            },
        );
        let record = run_mut(&mut state, id)?;
        ensure_event_capacity(record, 1)?;
        record.stored.accepted_input_bytes = record
            .stored
            .accepted_input_bytes
            .checked_add(input_bytes)
            .ok_or_else(|| MuzenError::internal("run input byte count overflow"))?;
        record.stored.snapshot.agents.push(AgentSnapshot {
            session_id: child_id.clone(),
            parent_session_id: Some(command.parent_session_id),
            path: child_path,
            status: AgentStatus::Queued,
            model: child_spec.agent.model.clone(),
            usage: Usage::default(),
        });
        record
            .stored
            .snapshot
            .agents
            .sort_by(|left, right| left.path.cmp(&right.path));
        append_event(
            record,
            now,
            "agent.created",
            Some(child_id.clone()),
            BTreeMap::new(),
        )?;
        remember_idempotency(
            &mut state,
            IdempotencyScope::RunSpawn,
            command.idempotency_key.as_ref(),
            digest,
            child_id.as_str(),
        );
        Ok(child_id)
    }

    async fn advance_agent(
        &self,
        id: &RunId,
        output: AgentOutput,
        allow_pending: bool,
    ) -> Result<AgentAdvance, MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        let record = run_mut(&mut state, id)?;
        ensure_nonterminal_run(record, id)?;
        ensure_event_capacity(record, 1)?;
        let index = agent_index(record, id, &output.session_id)?;
        if allow_pending {
            if let Some(pending) = next_pending(&record.sends, &output.session_id) {
                return Ok(AgentAdvance::Pending(pending.delivery));
            }
        }
        if is_terminal_agent(record.stored.snapshot.agents[index].status) {
            return Ok(AgentAdvance::Finished);
        }
        record.stored.snapshot.agents[index].status = public_agent_status(output.status);
        record.stored.snapshot.agents[index].usage = output.usage.clone();
        record.stored.outputs.push(output.clone());
        record
            .stored
            .outputs
            .sort_by(|left, right| left.path.cmp(&right.path));
        append_event(
            record,
            now,
            terminal_agent_event_type(output.status),
            Some(output.session_id),
            BTreeMap::new(),
        )?;
        Ok(AgentAdvance::Finished)
    }

    async fn append_activity(
        &self,
        id: &RunId,
        activity: RunActivity,
    ) -> Result<CommandReceipt, MuzenError> {
        let now = timestamp()?;
        let mut state = self.state.lock();
        validate_activity(&state, id, &activity)?;
        let receipt = {
            let record = run_mut(&mut state, id)?;
            ensure_event_capacity(record, activity.events.len() as u64)?;
            let mut last = None;
            for event in activity.events {
                last = Some(append_event(
                    record,
                    now.clone(),
                    &event.event_type,
                    event.session_id,
                    event.payload,
                )?);
            }
            last.expect("activity validation requires an event")
        };
        for message in activity.messages {
            let session = state
                .sessions
                .get_mut(&message.session_id)
                .expect("activity validation proved session existence");
            session.stored.snapshot.updated_at = now.clone();
            session.messages.push(message);
        }
        Ok(receipt)
    }

    async fn cancel_requested(&self, id: &RunId) -> Result<bool, MuzenError> {
        let state = self.state.lock();
        Ok(state
            .runs
            .get(id)
            .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?
            .cancel_receipt
            .is_some())
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
            let newly_terminal = record
                .stored
                .snapshot
                .agents
                .iter()
                .zip(&finish.outputs)
                .filter(|(agent, _)| !is_terminal_agent(agent.status))
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

fn validate_activity(state: &State, id: &RunId, activity: &RunActivity) -> Result<(), MuzenError> {
    let record = state
        .runs
        .get(id)
        .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?;
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
    for event in &activity.events {
        if let Some(session_id) = &event.session_id {
            ensure_tracked(record, id, session_id)?;
        }
    }
    for message in &activity.messages {
        ensure_tracked(record, id, &message.session_id)?;
        if !state.sessions.contains_key(&message.session_id) {
            return Err(MuzenError::internal(
                "run activity message references a missing agent session",
            ));
        }
    }
    Ok(())
}

fn ensure_tracked(
    record: &RunRecord,
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

fn ensure_nonterminal_run(record: &RunRecord, id: &RunId) -> Result<(), MuzenError> {
    if is_terminal(record.stored.snapshot.status) {
        Err(MuzenError::conflict(format!(
            "run {id} is already terminal"
        )))
    } else {
        Ok(())
    }
}

fn is_terminal_agent(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed
            | AgentStatus::Failed
            | AgentStatus::Cancelled
            | AgentStatus::BudgetExhausted
    )
}

fn agent_index(
    record: &RunRecord,
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

fn next_pending<'a>(
    sends: &'a [(PendingSend, bool)],
    session_id: &SessionId,
) -> Option<&'a PendingSend> {
    sends
        .iter()
        .filter(|(send, delivered)| !*delivered && &send.session_id == session_id)
        .map(|(send, _)| send)
        .min_by_key(|send| {
            (
                if send.delivery == MessageDelivery::Steer {
                    0
                } else {
                    1
                },
                send.sequence,
            )
        })
}

fn aggregate_live_status(record: &RunRecord) -> RunStatus {
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

fn user_message(
    session_id: &SessionId,
    input: crate::agent_runtime::AgentInput,
    now: String,
) -> AgentMessage {
    AgentMessage {
        id: new_message_id(),
        session_id: session_id.clone(),
        role: MessageRole::User,
        content: input.content,
        created_at: now,
    }
}

fn validate_input_budget(
    state: &State,
    id: &RunId,
    input: &crate::agent_runtime::AgentInput,
) -> Result<(), MuzenError> {
    let record = state
        .runs
        .get(id)
        .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?;
    ensure_nonterminal_run(record, id)?;
    ensure_event_capacity(record, 1)?;
    let bytes = record
        .stored
        .accepted_input_bytes
        .checked_add(decoded_input_bytes(input)?)
        .ok_or_else(|| MuzenError::internal("run input byte count overflow"))?;
    if bytes > record.stored.spec.limits.max_input_bytes.get() {
        return Err(MuzenError::resource_exhausted(
            "run maxInputBytes exhausted",
        ));
    }
    Ok(())
}

fn validate_spawn(
    state: &State,
    id: &RunId,
    command: &SpawnCommand,
) -> Result<(SessionSpec, Vec<u32>), MuzenError> {
    let record = state
        .runs
        .get(id)
        .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?;
    ensure_nonterminal_run(record, id)?;
    if record.stored.snapshot.agents.len() >= record.stored.spec.limits.max_agents.get() as usize {
        return Err(MuzenError::resource_exhausted("run maxAgents exhausted"));
    }
    let parent_index = agent_index(record, id, &command.parent_session_id)?;
    let parent = &record.stored.snapshot.agents[parent_index];
    if is_terminal_agent(parent.status) {
        return Err(MuzenError::conflict(format!(
            "agent session {} is terminal",
            command.parent_session_id
        )));
    }
    let child_depth = parent.path.len() as u32;
    if child_depth > record.stored.spec.limits.max_depth {
        return Err(MuzenError::resource_exhausted("run maxDepth exhausted"));
    }
    let parent_session = state
        .sessions
        .get(&command.parent_session_id)
        .ok_or_else(|| MuzenError::internal("parent session disappeared"))?;
    if !parent_session
        .stored
        .spec
        .models
        .iter()
        .any(|model| model.id == command.agent.model)
    {
        return Err(MuzenError::permission_denied(
            "child model is outside parent authority",
        ));
    }
    validate_child_budget(&parent_session.stored.spec.agent, &command.agent)?;
    for grant in &command.agent.tools {
        let Some(parent_grant) = parent_session
            .stored
            .spec
            .agent
            .tools
            .iter()
            .find(|candidate| candidate.provider == grant.provider && candidate.tool == grant.tool)
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
    let next_index = record
        .stored
        .snapshot
        .agents
        .iter()
        .filter(|agent| agent.parent_session_id.as_ref() == Some(&command.parent_session_id))
        .count() as u32;
    let mut path = parent.path.clone();
    path.push(next_index);
    Ok((parent_session.stored.spec.clone(), path))
}

fn validate_child_budget(
    parent: &crate::agent_runtime::AgentDefinition,
    child: &crate::agent_runtime::AgentDefinition,
) -> Result<(), MuzenError> {
    let Some(parent_budget) = &parent.budget else {
        return Ok(());
    };
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
        Err(MuzenError::permission_denied(
            "child budget exceeds parent authority",
        ))
    } else {
        Ok(())
    }
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
        record
            .stored
            .snapshot
            .agents
            .iter()
            .map(|agent| agent.session_id.clone())
            .collect(),
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
