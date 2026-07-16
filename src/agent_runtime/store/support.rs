use time::macros::format_description;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::agent_runtime::{
    AgentInput, AgentStatus, MuzenError, RunId, RunRoot, RunStatus, SessionId, TerminalAgentStatus,
    TerminalRunStatus,
};

pub(super) fn root_input(root: &RunRoot) -> &AgentInput {
    match root {
        RunRoot::Existing(root) => &root.input,
        RunRoot::New(root) => &root.input,
    }
}

pub(super) fn public_run_status(status: TerminalRunStatus) -> RunStatus {
    match status {
        TerminalRunStatus::Completed => RunStatus::Completed,
        TerminalRunStatus::Partial => RunStatus::Partial,
        TerminalRunStatus::Failed => RunStatus::Failed,
        TerminalRunStatus::Cancelled => RunStatus::Cancelled,
    }
}

pub(super) fn public_agent_status(status: TerminalAgentStatus) -> AgentStatus {
    match status {
        TerminalAgentStatus::Completed => AgentStatus::Completed,
        TerminalAgentStatus::Failed => AgentStatus::Failed,
        TerminalAgentStatus::Cancelled => AgentStatus::Cancelled,
        TerminalAgentStatus::BudgetExhausted => AgentStatus::BudgetExhausted,
    }
}

pub(super) fn terminal_agent_event_type(status: TerminalAgentStatus) -> &'static str {
    match status {
        TerminalAgentStatus::Completed => "agent.completed",
        TerminalAgentStatus::Failed => "agent.failed",
        TerminalAgentStatus::Cancelled => "agent.cancelled",
        TerminalAgentStatus::BudgetExhausted => "agent.budget_exhausted",
    }
}

pub(super) fn terminal_event_type(status: TerminalRunStatus) -> &'static str {
    match status {
        TerminalRunStatus::Completed => "run.completed",
        TerminalRunStatus::Partial => "run.partial",
        TerminalRunStatus::Failed => "run.failed",
        TerminalRunStatus::Cancelled => "run.cancelled",
    }
}

pub(super) fn is_terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Partial | RunStatus::Failed | RunStatus::Cancelled
    )
}

pub(crate) fn timestamp() -> Result<String, MuzenError> {
    const RFC3339_MILLISECONDS: &[time::format_description::BorrowedFormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    let now = OffsetDateTime::now_utc();
    now.replace_nanosecond((now.nanosecond() / 1_000_000) * 1_000_000)
        .map_err(|error| MuzenError::internal(format!("failed to truncate timestamp: {error}")))?
        .format(RFC3339_MILLISECONDS)
        .map_err(|error| MuzenError::internal(format!("failed to format timestamp: {error}")))
}

pub(super) fn new_session_id() -> Result<SessionId, MuzenError> {
    SessionId::new(format!("ses_{}", Uuid::new_v4().simple())).map_err(MuzenError::internal)
}

pub(super) fn new_run_id() -> Result<RunId, MuzenError> {
    RunId::new(format!("run_{}", Uuid::new_v4().simple())).map_err(MuzenError::internal)
}

pub(crate) fn new_message_id() -> String {
    format!("msg_{}", Uuid::new_v4().simple())
}
