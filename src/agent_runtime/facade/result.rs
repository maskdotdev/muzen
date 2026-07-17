use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::agent_runtime::{
    AgentInput, AgentSession, ErrorCode, ExecutionErrorCode, MuzenError, RunId, RunLimits,
    RunResult, SessionId, SingleRunOptions, TerminalAgentStatus, Usage,
};

/// Terminal output for one root agent run.
#[derive(Debug, Clone)]
pub struct AgentResult {
    pub text: String,
    pub output: Value,
    pub usage: Usage,
    pub status: TerminalAgentStatus,
    pub run_id: RunId,
    pub session_id: SessionId,
    pub raw: RunResult,
}

impl AgentResult {
    /// Deserializes the exposed structured output into the caller's type.
    pub fn output<T: DeserializeOwned>(&self) -> Result<T, MuzenError> {
        serde_json::from_value(self.output.clone()).map_err(|error| {
            MuzenError::invalid_input(format!("agent output could not be deserialized: {error}"))
        })
    }

    /// Returns this result only when its root agent completed successfully.
    pub fn into_ok(self) -> Result<Self, MuzenError> {
        self.raise_for_status()?;
        Ok(self)
    }

    pub fn raise_for_status(&self) -> Result<&Self, MuzenError> {
        if self.status == TerminalAgentStatus::Completed {
            return Ok(self);
        }
        let failed = self
            .raw
            .outputs
            .iter()
            .find(|output| output.status == self.status);
        let execution_error = failed.and_then(|output| output.error.as_ref());
        let message = execution_error
            .map(|error| error.message.clone())
            .unwrap_or_else(|| {
                format!(
                    "agent ended with status {}",
                    terminal_status_name(self.status)
                )
            });
        let code = match self.status {
            TerminalAgentStatus::BudgetExhausted => ErrorCode::ResourceExhausted,
            TerminalAgentStatus::Cancelled => ErrorCode::Conflict,
            TerminalAgentStatus::Completed | TerminalAgentStatus::Failed => ErrorCode::Internal,
        };
        let mut details = json!({ "status": terminal_status_name(self.status) });
        if let Some(error) = execution_error {
            details["executionCode"] = serde_json::to_value(error.code)
                .unwrap_or_else(|_| Value::String(execution_code_name(error.code).to_owned()));
        }
        Err(MuzenError::new(code, message)
            .with_retryable(execution_error.is_some_and(|error| error.retryable))
            .with_details(details))
    }
}

pub(super) async fn run_in_session(
    session: &AgentSession,
    input: AgentInput,
    limits: RunLimits,
    has_output: bool,
) -> Result<AgentResult, MuzenError> {
    let run = session
        .run(
            input,
            SingleRunOptions {
                limits,
                idempotency_key: None,
                metadata: BTreeMap::new(),
            },
        )
        .await?;
    let raw = run.wait().await?;
    result_from_run(raw, session.id(), has_output)
}

pub(super) fn result_from_run(
    raw: RunResult,
    session_id: &SessionId,
    has_output: bool,
) -> Result<AgentResult, MuzenError> {
    let root = raw
        .outputs
        .iter()
        .find(|output| &output.session_id == session_id && output.path.is_empty())
        .or_else(|| {
            raw.outputs
                .iter()
                .find(|output| &output.session_id == session_id)
        })
        .or_else(|| raw.outputs.first())
        .ok_or_else(|| MuzenError::internal("run completed without an agent output"))?;
    let value = root.output.clone().unwrap_or(Value::Null);
    let text = match &value {
        Value::String(text) => text.clone(),
        value => serde_json::to_string(value).map_err(|error| {
            MuzenError::internal(format!("failed to encode agent output: {error}"))
        })?,
    };
    let output = if has_output {
        value
    } else {
        Value::String(text.clone())
    };
    Ok(AgentResult {
        text,
        output,
        usage: root.usage.clone(),
        status: root.status,
        run_id: raw.run_id.clone(),
        session_id: session_id.clone(),
        raw,
    })
}

fn terminal_status_name(status: TerminalAgentStatus) -> &'static str {
    match status {
        TerminalAgentStatus::Completed => "completed",
        TerminalAgentStatus::Failed => "failed",
        TerminalAgentStatus::Cancelled => "cancelled",
        TerminalAgentStatus::BudgetExhausted => "budget_exhausted",
    }
}

fn execution_code_name(code: ExecutionErrorCode) -> &'static str {
    match code {
        ExecutionErrorCode::ModelError => "model_error",
        ExecutionErrorCode::ToolError => "tool_error",
        ExecutionErrorCode::SecretUnavailable => "secret_unavailable",
        ExecutionErrorCode::WorkspaceError => "workspace_error",
        ExecutionErrorCode::BudgetExhausted => "budget_exhausted",
        ExecutionErrorCode::Cancelled => "cancelled",
    }
}
