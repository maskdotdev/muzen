use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

use super::{Inner, ModelProviderError, ModelRequest, ModelTurn};
use crate::agent_runtime::store::support::{new_message_id, timestamp};
use crate::agent_runtime::store::{ActivityEvent, FinishRun, RunActivity};
use crate::agent_runtime::{
    AgentMessage, AgentOutput, AgentSnapshot, ContentBlock, ExecutionError, ExecutionErrorCode,
    MessagePage, MessageRole, MuzenError, RunId, TerminalAgentStatus, TerminalRunStatus, Usage,
};

pub(super) async fn execute(inner: Arc<Inner>, run_id: RunId) {
    let execution = AssertUnwindSafe(execute_inner(Arc::clone(&inner), run_id.clone()))
        .catch_unwind()
        .await;
    match execution {
        Ok(Ok(())) => {}
        Ok(Err(error)) => recover(&inner, &run_id, error.message()).await,
        Err(_) => recover(&inner, &run_id, "local execution task panicked").await,
    }
    inner.cleanup_terminal(&run_id).await;
}

async fn execute_inner(inner: Arc<Inner>, run_id: RunId) -> Result<(), MuzenError> {
    let stored = inner.store.run(&run_id).await?;
    if stored.result.is_some() {
        return Ok(());
    }
    let deadline = stored
        .spec
        .limits
        .deadline_ms
        .map(|duration| Instant::now() + std::time::Duration::from_millis(duration.get()));
    inner.store.mark_run_running(&run_id).await?;
    inner.notify_snapshot(&run_id).await;

    let semaphore = Arc::new(Semaphore::new(
        stored.spec.limits.max_active_agents.get() as usize
    ));
    let budget = Arc::new(Mutex::new(BudgetState::default()));
    let roots = stored.snapshot.roots.clone();
    let mut agents = FuturesUnordered::new();
    for agent in stored.snapshot.agents {
        agents.push(run_agent(
            Arc::clone(&inner),
            run_id.clone(),
            agent,
            Arc::clone(&semaphore),
            Arc::clone(&budget),
            stored.spec.limits.max_total_tokens.map(|limit| limit.get()),
            deadline,
        ));
    }
    let mut outputs = Vec::new();
    while let Some(output) = agents.next().await {
        outputs.push(output);
    }
    outputs.sort_by(|left, right| left.path.cmp(&right.path));
    let usage = sum_usage(&outputs)?;
    let cancelled = inner.store.cancel_requested(&run_id).await?;
    let completed_agents = outputs
        .iter()
        .filter(|output| output.status == TerminalAgentStatus::Completed)
        .count();
    let completed_roots = outputs
        .iter()
        .filter(|output| {
            output.status == TerminalAgentStatus::Completed && roots.contains(&output.session_id)
        })
        .count();
    let status = if completed_agents == outputs.len() {
        TerminalRunStatus::Completed
    } else if completed_roots > 0 {
        TerminalRunStatus::Partial
    } else if cancelled {
        TerminalRunStatus::Cancelled
    } else {
        TerminalRunStatus::Failed
    };
    inner
        .store
        .finish_run(
            &run_id,
            FinishRun {
                status,
                outputs,
                usage,
                artifacts: Vec::new(),
                metadata: stored.spec.metadata,
            },
        )
        .await?;
    inner.notify_snapshot(&run_id).await;
    Ok(())
}

#[derive(Default)]
struct BudgetState {
    usage: Usage,
    exhausted: bool,
}

async fn run_agent(
    inner: Arc<Inner>,
    run_id: RunId,
    agent: AgentSnapshot,
    semaphore: Arc<Semaphore>,
    budget: Arc<Mutex<BudgetState>>,
    token_limit: Option<u64>,
    deadline: Option<Instant>,
) -> AgentOutput {
    if observe_cancel(&inner, &run_id, deadline).await {
        return cancelled_output(agent, Usage::default());
    }
    if budget.lock().await.exhausted {
        return budget_output(agent, Usage::default());
    }
    let permit = tokio::select! {
        permit = semaphore.acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => return failed_output(agent, "local agent concurrency limiter closed"),
        },
        _ = wait_cancel(&inner, &run_id, deadline) => {
            return cancelled_output(agent, Usage::default());
        }
    };
    if observe_cancel(&inner, &run_id, deadline).await {
        return cancelled_output(agent, Usage::default());
    }
    if budget.lock().await.exhausted {
        return budget_output(agent, Usage::default());
    }

    if let Err(error) = append_events(
        &inner,
        &run_id,
        vec![activity("model.started", &agent, BTreeMap::new())],
        Vec::new(),
    )
    .await
    {
        return failed_output(agent, error.message());
    }
    let request = match model_request(&inner, &agent).await {
        Ok(request) => request,
        Err(error) => return failed_output(agent, error.message()),
    };
    let response = tokio::select! {
        response = inner.provider.complete(request) => Some(response),
        _ = wait_cancel(&inner, &run_id, deadline) => None,
    };
    drop(permit);
    let Some(response) = response else {
        return cancelled_output(agent, Usage::default());
    };
    let turn = match response {
        Ok(turn) => turn,
        Err(error) => {
            let execution_error = provider_execution_error(&error);
            let mut payload = BTreeMap::new();
            payload.insert("error".to_owned(), execution_error_value(&execution_error));
            if let Err(store_error) = append_events(
                &inner,
                &run_id,
                vec![activity("model.failed", &agent, payload)],
                Vec::new(),
            )
            .await
            {
                return failed_output(agent, store_error.message());
            }
            return AgentOutput {
                session_id: agent.session_id,
                path: agent.path,
                status: TerminalAgentStatus::Failed,
                output: None,
                usage: Usage::default(),
                error: Some(execution_error),
            };
        }
    };
    let usage = turn.usage.clone();
    let exhausted = match record_usage(&budget, &usage, token_limit).await {
        Ok(exhausted) => exhausted,
        Err(error) => return failed_output_with_usage(agent, error.message(), usage),
    };
    if observe_cancel(&inner, &run_id, deadline).await {
        return cancelled_output(agent, usage);
    }
    let output = output_value(&turn);
    let message = match assistant_message(&agent, &turn) {
        Ok(message) => message,
        Err(error) => return failed_output_with_usage(agent, error.message(), usage),
    };
    if let Err(error) = append_events(
        &inner,
        &run_id,
        vec![
            activity("message.accepted", &agent, BTreeMap::new()),
            activity("model.completed", &agent, BTreeMap::new()),
        ],
        vec![message],
    )
    .await
    {
        return failed_output_with_usage(agent, error.message(), usage);
    }
    if exhausted {
        return AgentOutput {
            session_id: agent.session_id,
            path: agent.path,
            status: TerminalAgentStatus::BudgetExhausted,
            output: Some(output),
            usage,
            error: Some(execution_error(
                ExecutionErrorCode::BudgetExhausted,
                "run token budget exhausted",
            )),
        };
    }
    AgentOutput {
        session_id: agent.session_id,
        path: agent.path,
        status: TerminalAgentStatus::Completed,
        output: Some(output),
        usage,
        error: None,
    }
}

async fn model_request(inner: &Inner, agent: &AgentSnapshot) -> Result<ModelRequest, MuzenError> {
    let session = inner.store.session(&agent.session_id).await?;
    let model = session
        .spec
        .models
        .iter()
        .find(|model| model.id == session.spec.agent.model)
        .cloned()
        .ok_or_else(|| MuzenError::internal("agent model profile disappeared"))?;
    let mut transcript = Vec::new();
    let mut after = None;
    loop {
        let page = inner
            .store
            .messages(
                &agent.session_id,
                MessagePage {
                    after,
                    limit: NonZeroU32::new(100),
                },
            )
            .await?;
        transcript.extend(page.items);
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(ModelRequest {
        agent: session.spec.agent,
        model,
        transcript,
    })
}

async fn append_events(
    inner: &Inner,
    run_id: &RunId,
    events: Vec<ActivityEvent>,
    messages: Vec<AgentMessage>,
) -> Result<(), MuzenError> {
    inner
        .store
        .append_activity(run_id, RunActivity { events, messages })
        .await?;
    inner.notify_snapshot(run_id).await;
    Ok(())
}

fn activity(
    event_type: &str,
    agent: &AgentSnapshot,
    payload: BTreeMap<String, Value>,
) -> ActivityEvent {
    ActivityEvent {
        event_type: event_type.to_owned(),
        session_id: Some(agent.session_id.clone()),
        payload,
    }
}

async fn observe_cancel(inner: &Inner, run_id: &RunId, deadline: Option<Instant>) -> bool {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        request_deadline(inner, run_id).await;
    }
    inner.store.cancel_requested(run_id).await.unwrap_or(true)
}

async fn wait_cancel(inner: &Inner, run_id: &RunId, deadline: Option<Instant>) {
    let sequence = inner
        .store
        .run(run_id)
        .await
        .map(|run| run.snapshot.last_sequence)
        .unwrap_or(0);
    let mut receiver = inner.receiver_or_create(run_id, sequence);
    loop {
        if observe_cancel(inner, run_id, deadline).await {
            return;
        }
        match deadline {
            Some(deadline) => {
                tokio::select! {
                    _ = receiver.changed() => {}
                    _ = tokio::time::sleep_until(deadline) => request_deadline(inner, run_id).await,
                }
            }
            None => {
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn request_deadline(inner: &Inner, run_id: &RunId) {
    if inner
        .store
        .request_cancel(run_id, Some("deadline"))
        .await
        .is_ok()
    {
        inner.notify_snapshot(run_id).await;
    }
}

async fn record_usage(
    budget: &Mutex<BudgetState>,
    usage: &Usage,
    limit: Option<u64>,
) -> Result<bool, MuzenError> {
    let mut budget = budget.lock().await;
    budget.usage = add_usage(&budget.usage, usage)?;
    if limit.is_some_and(|limit| {
        budget
            .usage
            .input_tokens
            .saturating_add(budget.usage.output_tokens)
            >= limit
    }) {
        budget.exhausted = true;
    }
    Ok(budget.exhausted)
}

fn assistant_message(agent: &AgentSnapshot, turn: &ModelTurn) -> Result<AgentMessage, MuzenError> {
    Ok(AgentMessage {
        id: new_message_id(),
        session_id: agent.session_id.clone(),
        role: MessageRole::Assistant,
        content: turn.content.clone(),
        created_at: timestamp()?,
    })
}

fn output_value(turn: &ModelTurn) -> Value {
    match turn.content.as_slice() {
        [ContentBlock::Text { text }] => Value::String(text.clone()),
        content => serde_json::to_value(content).unwrap_or(Value::Null),
    }
}

fn provider_execution_error(error: &ModelProviderError) -> ExecutionError {
    ExecutionError {
        code: ExecutionErrorCode::ModelError,
        message: error.message().to_owned(),
        retryable: error.retryable(),
        details: error.details().cloned(),
    }
}

fn execution_error(code: ExecutionErrorCode, message: impl Into<String>) -> ExecutionError {
    ExecutionError {
        code,
        message: message.into(),
        retryable: false,
        details: None,
    }
}

fn execution_error_value(error: &ExecutionError) -> Value {
    serde_json::to_value(error).unwrap_or_else(|_| {
        json!({
            "code": "model_error",
            "message": "provider failed",
            "retryable": false
        })
    })
}

fn cancelled_output(agent: AgentSnapshot, usage: Usage) -> AgentOutput {
    AgentOutput {
        session_id: agent.session_id,
        path: agent.path,
        status: TerminalAgentStatus::Cancelled,
        output: None,
        usage,
        error: Some(execution_error(
            ExecutionErrorCode::Cancelled,
            "run was cancelled",
        )),
    }
}

fn budget_output(agent: AgentSnapshot, usage: Usage) -> AgentOutput {
    AgentOutput {
        session_id: agent.session_id,
        path: agent.path,
        status: TerminalAgentStatus::BudgetExhausted,
        output: None,
        usage,
        error: Some(execution_error(
            ExecutionErrorCode::BudgetExhausted,
            "run token budget exhausted",
        )),
    }
}

fn failed_output(agent: AgentSnapshot, message: impl Into<String>) -> AgentOutput {
    failed_output_with_usage(agent, message, Usage::default())
}

fn failed_output_with_usage(
    agent: AgentSnapshot,
    message: impl Into<String>,
    usage: Usage,
) -> AgentOutput {
    AgentOutput {
        session_id: agent.session_id,
        path: agent.path,
        status: TerminalAgentStatus::Failed,
        output: None,
        usage,
        error: Some(execution_error(ExecutionErrorCode::ModelError, message)),
    }
}

fn sum_usage(outputs: &[AgentOutput]) -> Result<Usage, MuzenError> {
    outputs.iter().try_fold(Usage::default(), |usage, output| {
        add_usage(&usage, &output.usage)
    })
}

fn add_usage(left: &Usage, right: &Usage) -> Result<Usage, MuzenError> {
    Ok(Usage {
        input_tokens: left
            .input_tokens
            .checked_add(right.input_tokens)
            .ok_or_else(|| MuzenError::internal("run input token usage overflow"))?,
        output_tokens: left
            .output_tokens
            .checked_add(right.output_tokens)
            .ok_or_else(|| MuzenError::internal("run output token usage overflow"))?,
        tool_calls: left
            .tool_calls
            .checked_add(right.tool_calls)
            .ok_or_else(|| MuzenError::internal("run tool call usage overflow"))?,
    })
}

async fn recover(inner: &Inner, run_id: &RunId, message: &str) {
    let Ok(stored) = inner.store.run(run_id).await else {
        return;
    };
    if stored.result.is_some() {
        return;
    }
    let cancelled = inner.store.cancel_requested(run_id).await.unwrap_or(false);
    let outputs = stored
        .snapshot
        .agents
        .into_iter()
        .map(|agent| {
            if cancelled {
                cancelled_output(agent, Usage::default())
            } else {
                failed_output(agent, message)
            }
        })
        .collect::<Vec<_>>();
    let status = if cancelled {
        TerminalRunStatus::Cancelled
    } else {
        TerminalRunStatus::Failed
    };
    let _ = inner
        .store
        .finish_run(
            run_id,
            FinishRun {
                status,
                outputs,
                usage: Usage::default(),
                artifacts: Vec::new(),
                metadata: stored.spec.metadata,
            },
        )
        .await;
    inner.notify_snapshot(run_id).await;
}
