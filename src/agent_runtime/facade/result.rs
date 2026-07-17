use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::agent_runtime::{
    AgentInput, AgentSession, AnswerToolCallInput, AnswerToolCallOutcome, ClientToolCallError,
    ErrorCode, EventOptions, ExecutionErrorCode, Muzen, MuzenError, Run, RunId, RunLimits,
    RunResult, SessionId, SingleRunOptions, TerminalAgentStatus, Usage,
};

use super::{Tool, LOCAL_TOOLS_PROVIDER_ID};

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
    client: &Muzen,
    tools: Arc<Vec<Tool>>,
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
    let mut pump = ToolPump::spawn(client.clone(), run.clone(), tools);
    let raw = run.wait().await;
    pump.stop().await;
    let raw = raw?;
    result_from_run(raw, session.id(), has_output)
}

struct ToolPump {
    task: Option<JoinHandle<Result<(), MuzenError>>>,
}

impl ToolPump {
    fn spawn(client: Muzen, run: Run, tools: Arc<Vec<Tool>>) -> Self {
        let task = (!tools.is_empty())
            .then(|| tokio::spawn(async move { pump_run_tools(client, run, tools).await }));
        Self { task }
    }

    async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for ToolPump {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(super) async fn pump_run_tools(
    client: Muzen,
    run: Run,
    tools: Arc<Vec<Tool>>,
) -> Result<(), MuzenError> {
    let mut after = None;
    let mut retry_cursor = None;
    loop {
        let mut events = run.events(EventOptions { after });
        let reconnect = loop {
            let event = match events.next().await {
                Some(Ok(event)) => event,
                Some(Err(error)) => break error,
                None => {
                    break MuzenError::unavailable("run event stream ended before a terminal event")
                }
            };
            if super::super::client::is_terminal_run_event(&event.event_type) {
                return Ok(());
            }
            if event.event_type != "tool.requested"
                || event.payload.get("provider").and_then(Value::as_str)
                    != Some(LOCAL_TOOLS_PROVIDER_ID)
            {
                after = Some(event.sequence);
                retry_cursor = None;
                continue;
            }
            match answer_requested_tool(&client, run.id(), &tools, &event.payload).await {
                Ok(()) => {
                    after = Some(event.sequence);
                    retry_cursor = None;
                }
                Err(error) => break error,
            }
        };
        if !reconnect.retryable() || retry_cursor == Some(after) {
            return Err(reconnect);
        }
        retry_cursor = Some(after);
        tokio::task::yield_now().await;
    }
}

async fn answer_requested_tool(
    client: &Muzen,
    run_id: &RunId,
    tools: &[Tool],
    payload: &BTreeMap<String, Value>,
) -> Result<(), MuzenError> {
    let call_id = payload
        .get("callId")
        .and_then(Value::as_str)
        .ok_or_else(|| MuzenError::internal("tool.requested event omitted callId"))?;
    let name = payload
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| MuzenError::internal("tool.requested event omitted tool"))?;
    let arguments = payload.get("arguments").cloned().unwrap_or(Value::Null);
    let outcome = match tools.iter().find(|tool| tool.name() == name) {
        Some(tool) => match tool.invoke(arguments).await {
            Ok(result) => AnswerToolCallOutcome::Result { result },
            Err(error) => tool_error(error.message().to_owned()),
        },
        None => tool_error(format!("unknown local tool: {name}")),
    };
    match client
        .answer_tool_call(
            run_id,
            AnswerToolCallInput {
                call_id: call_id.to_owned(),
                outcome,
            },
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(error) if matches!(error.code(), ErrorCode::Conflict | ErrorCode::NotFound) => Ok(()),
        Err(error) => Err(error),
    }
}

fn tool_error(message: String) -> AnswerToolCallOutcome {
    AnswerToolCallOutcome::Error {
        error: ClientToolCallError {
            message,
            retryable: Some(false),
        },
    }
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
