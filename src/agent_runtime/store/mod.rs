use std::collections::BTreeMap;
use std::num::NonZeroU64;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_runtime::{
    AgentEvent, AgentMessage, AgentOutput, AgentStatus, ArtifactRef, CommandReceipt,
    IdempotencyKey, MessageDelivery, MessagePage, MuzenError, Page, RunId, RunResult, RunSnapshot,
    RunSpec, SendCommand, SessionId, SessionSnapshot, SessionSpec, SpawnCommand, TerminalRunStatus,
    Usage,
};

pub(crate) mod memory;
pub(crate) mod sqlite;
pub(crate) mod support;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredSession {
    pub(crate) spec: SessionSpec,
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) run_count: u64,
    pub(crate) lifetime_usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredRun {
    pub(crate) spec: RunSpec,
    pub(crate) snapshot: RunSnapshot,
    pub(crate) result: Option<RunResult>,
    #[serde(default)]
    pub(crate) outputs: Vec<AgentOutput>,
    #[serde(default)]
    pub(crate) accepted_input_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingSend {
    pub(crate) sequence: u64,
    pub(crate) session_id: SessionId,
    pub(crate) input: crate::agent_runtime::AgentInput,
    pub(crate) delivery: MessageDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentAdvance {
    Pending(MessageDelivery),
    Finished,
}

#[derive(Debug, Clone)]
pub(crate) struct FinishRun {
    pub(crate) status: TerminalRunStatus,
    pub(crate) outputs: Vec<AgentOutput>,
    pub(crate) usage: Usage,
    pub(crate) artifacts: Vec<ArtifactRef>,
    pub(crate) metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActivityEvent {
    pub(crate) event_type: String,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) payload: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RunActivity {
    pub(crate) events: Vec<ActivityEvent>,
    pub(crate) messages: Vec<AgentMessage>,
}

#[async_trait]
pub(crate) trait AgentStore: Send + Sync {
    async fn create_session(
        &self,
        spec: SessionSpec,
        idempotency_key: Option<&IdempotencyKey>,
    ) -> Result<SessionId, MuzenError>;

    async fn session(&self, id: &SessionId) -> Result<StoredSession, MuzenError>;

    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError>;

    async fn archive_session(&self, id: &SessionId) -> Result<(), MuzenError>;

    async fn create_run(&self, spec: RunSpec) -> Result<RunId, MuzenError>;

    async fn run(&self, id: &RunId) -> Result<StoredRun, MuzenError>;

    async fn events_after(
        &self,
        id: &RunId,
        after: Option<u64>,
        limit: NonZeroU64,
    ) -> Result<Vec<AgentEvent>, MuzenError>;

    async fn mark_run_running(&self, id: &RunId) -> Result<CommandReceipt, MuzenError>;

    async fn set_agent_status(
        &self,
        id: &RunId,
        session_id: &SessionId,
        status: AgentStatus,
    ) -> Result<Option<CommandReceipt>, MuzenError>;

    async fn accept_send(
        &self,
        id: &RunId,
        command: SendCommand,
    ) -> Result<CommandReceipt, MuzenError>;

    async fn pending_send(
        &self,
        id: &RunId,
        session_id: &SessionId,
    ) -> Result<Option<PendingSend>, MuzenError>;

    async fn deliver_send(
        &self,
        id: &RunId,
        session_id: &SessionId,
        delivery: MessageDelivery,
    ) -> Result<bool, MuzenError>;

    async fn spawn_agent(&self, id: &RunId, command: SpawnCommand)
        -> Result<SessionId, MuzenError>;

    async fn advance_agent(
        &self,
        id: &RunId,
        output: AgentOutput,
        allow_pending: bool,
    ) -> Result<AgentAdvance, MuzenError>;

    async fn append_activity(
        &self,
        id: &RunId,
        activity: RunActivity,
    ) -> Result<CommandReceipt, MuzenError>;

    async fn cancel_requested(&self, id: &RunId) -> Result<bool, MuzenError>;

    async fn request_cancel(
        &self,
        id: &RunId,
        reason: Option<&str>,
    ) -> Result<CommandReceipt, MuzenError>;

    async fn finish_run(&self, id: &RunId, finish: FinishRun) -> Result<RunResult, MuzenError>;
}

fn body_digest<T: Serialize>(value: &T) -> Result<[u8; 32], MuzenError> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        MuzenError::internal(format!("failed to encode idempotent body: {error}"))
    })?;
    Ok(*blake3::hash(&encoded).as_bytes())
}

fn receipt(sequence: u64) -> Result<CommandReceipt, MuzenError> {
    let sequence = NonZeroU64::new(sequence)
        .ok_or_else(|| MuzenError::internal("event sequence must start at one"))?;
    Ok(CommandReceipt { sequence })
}

#[cfg(test)]
mod conformance;

#[cfg(test)]
mod tests;
