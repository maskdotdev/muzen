use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread::{self, JoinHandle as ThreadJoinHandle};
use std::time::Duration;

use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::runtime::RuntimeFlavor;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::agent_runtime::{
    AgentInput, AgentSession, AnswerToolCallInput, AnswerToolCallOutcome, CancelOptions,
    ClientToolCallError, ErrorCode, EventOptions, ExecutionErrorCode, Muzen, MuzenError, Run,
    RunId, RunLimits, RunResult, SessionId, SingleRunOptions, TerminalAgentStatus, Usage,
};

use super::{Tool, LOCAL_TOOLS_PROVIDER_ID};

const MAX_EVENT_RESUME_ATTEMPTS: usize = 5;
const EVENT_RESUME_BACKOFF: [Duration; MAX_EVENT_RESUME_ATTEMPTS] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];

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
    let raw = if pump.finished.is_some() {
        let race = {
            let finished = pump
                .finished
                .as_mut()
                .expect("tool pump completion receiver exists");
            tokio::select! {
                biased;
                raw = run.wait() => ToolPumpRace::Run(raw),
                pump_result = finished => ToolPumpRace::Pump(pump_result),
            }
        };
        match race {
            ToolPumpRace::Run(raw) => {
                pump.stop().await;
                raw?
            }
            ToolPumpRace::Pump(pump_result) => {
                pump.finished.take();
                pump.stop().await;
                match pump_result {
                    Ok(Ok(())) => run.wait().await?,
                    Ok(Err(error)) => {
                        let _ = run.cancel(CancelOptions::default()).await;
                        let _ = run.wait().await;
                        return Err(error);
                    }
                    Err(receive_error) => {
                        let error = MuzenError::internal(format!(
                            "tool pump task stopped unexpectedly: {receive_error}"
                        ));
                        let _ = run.cancel(CancelOptions::default()).await;
                        let _ = run.wait().await;
                        return Err(error);
                    }
                }
            }
        }
    } else {
        run.wait().await?
    };
    result_from_run(raw, session.id(), has_output)
}

enum ToolPumpRace {
    Run(Result<RunResult, MuzenError>),
    Pump(Result<Result<(), MuzenError>, oneshot::error::RecvError>),
}

enum ToolPumpWorker {
    Thread(ThreadJoinHandle<()>),
    Task(JoinHandle<()>),
}

pub(super) struct ToolPump {
    cancel: Option<oneshot::Sender<()>>,
    pub(super) finished: Option<oneshot::Receiver<Result<(), MuzenError>>>,
    worker: Option<ToolPumpWorker>,
}

impl ToolPump {
    pub(super) fn spawn(client: Muzen, run: Run, tools: Arc<Vec<Tool>>) -> Self {
        if tools.is_empty() {
            return Self {
                cancel: None,
                finished: None,
                worker: None,
            };
        }
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (finished_tx, finished_rx) = oneshot::channel();
        let worker = match client.isolated_clone() {
            Some(isolated_client) => {
                let isolated_run = isolated_client.run_handle(run.id());
                let thread = thread::Builder::new()
                    .name("muzen-tool-pump".to_owned())
                    .spawn(move || {
                        let result = match tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            Ok(runtime) => runtime.block_on(run_tool_pump(
                                cancel_rx,
                                isolated_client,
                                isolated_run,
                                tools,
                            )),
                            Err(error) => Err(MuzenError::internal(format!(
                                "failed to build isolated tool pump runtime: {error}"
                            ))),
                        };
                        let _ = finished_tx.send(result);
                    })
                    .expect("failed to spawn isolated tool pump thread");
                ToolPumpWorker::Thread(thread)
            }
            None => {
                let task = tokio::spawn(async move {
                    let result = run_tool_pump(cancel_rx, client, run, tools).await;
                    let _ = finished_tx.send(result);
                });
                ToolPumpWorker::Task(task)
            }
        };
        Self {
            cancel: Some(cancel_tx),
            finished: Some(finished_rx),
            worker: Some(worker),
        }
    }

    pub(super) async fn stop(&mut self) {
        self.cancel();
        match self.worker.take() {
            Some(ToolPumpWorker::Thread(thread)) => join_pump_thread(thread),
            Some(ToolPumpWorker::Task(task)) => {
                task.abort();
                let _ = task.await;
            }
            None => {}
        }
    }

    #[cfg(test)]
    pub(super) fn is_thread_mode(&self) -> bool {
        matches!(self.worker, Some(ToolPumpWorker::Thread(_)))
    }

    #[cfg(test)]
    pub(super) fn worker_is_finished(&self) -> bool {
        match self.worker.as_ref() {
            Some(ToolPumpWorker::Thread(thread)) => thread.is_finished(),
            Some(ToolPumpWorker::Task(task)) => task.is_finished(),
            None => true,
        }
    }

    #[cfg(test)]
    pub(super) fn has_worker(&self) -> bool {
        self.worker.is_some()
    }

    fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

impl Drop for ToolPump {
    fn drop(&mut self) {
        self.cancel();
        match self.worker.take() {
            Some(ToolPumpWorker::Thread(thread)) => {
                // The private current-thread runtime observes cancellation on
                // its next poll, so joining here is bounded and avoids leaks.
                let _ = thread.join();
            }
            Some(ToolPumpWorker::Task(task)) => task.abort(),
            None => {}
        }
    }
}

async fn run_tool_pump(
    mut cancel: oneshot::Receiver<()>,
    client: Muzen,
    run: Run,
    tools: Arc<Vec<Tool>>,
) -> Result<(), MuzenError> {
    tokio::select! {
        biased;
        _ = &mut cancel => Ok(()),
        result = pump_run_tools(client, run, tools) => result,
    }
}

fn join_pump_thread(thread: ThreadJoinHandle<()>) {
    let on_multi_thread_runtime = tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::MultiThread);
    if on_multi_thread_runtime {
        tokio::task::block_in_place(|| {
            let _ = thread.join();
        });
    } else {
        let _ = thread.join();
    }
}

pub(super) async fn pump_run_tools(
    client: Muzen,
    run: Run,
    tools: Arc<Vec<Tool>>,
) -> Result<(), MuzenError> {
    let mut after = None;
    let mut resume_attempts = 0;
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
                resume_attempts = 0;
                continue;
            }
            match answer_requested_tool(&client, run.id(), &tools, &event.payload).await {
                Ok(()) => {
                    after = Some(event.sequence);
                    resume_attempts = 0;
                }
                Err(error) => break error,
            }
        };
        if !reconnect.retryable() || resume_attempts == MAX_EVENT_RESUME_ATTEMPTS {
            return Err(reconnect);
        }
        let delay = EVENT_RESUME_BACKOFF[resume_attempts];
        resume_attempts += 1;
        tokio::time::sleep(delay).await;
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
