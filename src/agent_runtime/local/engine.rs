use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::time::Instant;

use super::{Inner, ModelProviderError, ModelRequest, ModelStop, ModelToolCall, ModelTurn};
use crate::agent_runtime::output_schema::validate_instance;
use crate::agent_runtime::store::support::{new_message_id, timestamp};
use crate::agent_runtime::store::{ActivityEvent, AgentAdvance, FinishRun, RunActivity};
use crate::agent_runtime::{
    AgentMessage, AgentOutput, AgentSnapshot, AgentStatus, ContentBlock, ExecutionError,
    ExecutionErrorCode, MessageDelivery, MessagePage, MessageRole, MuzenError, OutputContract,
    RunId, SendCommand, SpawnCommand, TerminalAgentStatus, TerminalRunStatus, ToolEffect,
    ToolProvider, ToolProviderId, Usage,
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
    let initial = inner.store.run(&run_id).await?;
    if initial.result.is_some() {
        return Ok(());
    }
    let deadline = initial
        .spec
        .limits
        .deadline_ms
        .map(|duration| Instant::now() + std::time::Duration::from_millis(duration.get()));
    if initial.snapshot.status == crate::agent_runtime::RunStatus::Queued {
        inner.store.mark_run_running(&run_id).await?;
        inner.notify_snapshot(&run_id).await;
    }

    let semaphore = Arc::new(Semaphore::new(
        initial.spec.limits.max_active_agents.get() as usize
    ));
    let budget = Arc::new(RunBudget::default());
    let token_limit = initial
        .spec
        .limits
        .max_total_tokens
        .map(|limit| limit.get());
    let tool_limit = initial.spec.limits.max_total_tool_calls;
    let mut receiver = inner.receiver_or_create(&run_id, initial.snapshot.last_sequence);
    let mut scheduled = BTreeSet::new();
    let mut agents = FuturesUnordered::new();

    loop {
        let stored = inner.store.run(&run_id).await?;
        if stored.result.is_some() {
            return Ok(());
        }
        for agent in &stored.snapshot.agents {
            if !terminal_agent(agent.status) && scheduled.insert(agent.session_id.clone()) {
                let inner = Arc::clone(&inner);
                let run_id = run_id.clone();
                let agent = agent.clone();
                let semaphore = Arc::clone(&semaphore);
                let budget = Arc::clone(&budget);
                agents.push(
                    async move {
                        let session_id = agent.session_id.clone();
                        run_agent(
                            inner,
                            run_id,
                            agent,
                            semaphore,
                            budget,
                            token_limit,
                            tool_limit,
                            deadline,
                        )
                        .await;
                        session_id
                    }
                    .boxed(),
                );
            }
        }

        if stored
            .snapshot
            .agents
            .iter()
            .all(|agent| terminal_agent(agent.status))
        {
            match finish(&inner, &run_id).await {
                Ok(()) => return Ok(()),
                Err(error) if error.code() == crate::agent_runtime::ErrorCode::Conflict => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }

        if agents.is_empty() {
            receiver
                .changed()
                .await
                .map_err(|_| MuzenError::internal("run notification closed"))?;
            continue;
        }
        tokio::select! {
            completed = agents.next() => {
                if let Some(session_id) = completed {
                    scheduled.remove(&session_id);
                }
            }
            changed = receiver.changed() => {
                changed.map_err(|_| MuzenError::internal("run notification closed"))?;
            }
        }
    }
}

#[derive(Default)]
struct BudgetState {
    usage: Usage,
    exhaustion: Option<BudgetExhaustion>,
}

#[derive(Default)]
struct RunBudget {
    state: Mutex<BudgetState>,
    exhausted: Notify,
}

#[derive(Clone, Copy)]
enum BudgetExhaustion {
    RunTokens,
    AgentToolCalls,
    RunToolCalls,
    AgentTurns,
}

impl BudgetExhaustion {
    fn message(self) -> &'static str {
        match self {
            Self::RunTokens => "run token budget exhausted",
            Self::AgentToolCalls => "agent maxToolCalls exhausted",
            Self::RunToolCalls => "run maxTotalToolCalls exhausted",
            Self::AgentTurns => "agent maxTurns exhausted",
        }
    }
}

async fn run_agent(
    inner: Arc<Inner>,
    run_id: RunId,
    agent: AgentSnapshot,
    semaphore: Arc<Semaphore>,
    budget: Arc<RunBudget>,
    token_limit: Option<u64>,
    tool_limit: Option<u64>,
    deadline: Option<Instant>,
) {
    let mut usage = Usage::default();
    let mut turns = 0_u32;
    let mut grant_calls = BTreeMap::new();
    loop {
        if observe_cancel(&inner, &run_id, deadline).await {
            finish_agent(
                &inner,
                &run_id,
                cancelled_output(agent.clone(), usage),
                false,
            )
            .await;
            return;
        }
        if let Some(exhaustion) = budget.state.lock().await.exhaustion {
            finish_agent(
                &inner,
                &run_id,
                budget_output(agent.clone(), usage, exhaustion),
                false,
            )
            .await;
            return;
        }
        let permit = tokio::select! {
            permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    finish_agent(&inner, &run_id, failed_output(agent.clone(), "local agent concurrency limiter closed", usage), false).await;
                    return;
                }
            },
            _ = wait_cancel(&inner, &run_id, deadline) => {
                finish_agent(&inner, &run_id, cancelled_output(agent.clone(), usage), false).await;
                return;
            }
        };
        if let Err(error) = inner
            .store
            .set_agent_status(&run_id, &agent.session_id, AgentStatus::Running)
            .await
        {
            finish_agent(
                &inner,
                &run_id,
                failed_output(agent.clone(), error.message(), usage),
                false,
            )
            .await;
            return;
        }
        inner.notify_snapshot(&run_id).await;
        if let Err(error) = append_events(
            &inner,
            &run_id,
            vec![activity("model.started", &agent, BTreeMap::new())],
            Vec::new(),
        )
        .await
        {
            finish_agent(
                &inner,
                &run_id,
                failed_output(agent.clone(), error.message(), usage),
                false,
            )
            .await;
            return;
        }
        let request = match model_request(&inner, &agent).await {
            Ok(request) => request,
            Err(error) => {
                finish_agent(
                    &inner,
                    &run_id,
                    failed_output(agent.clone(), error.message(), usage),
                    false,
                )
                .await;
                return;
            }
        };
        let output_contract = request.agent.output.clone();
        let response = tokio::select! {
            response = inner.provider.complete(request) => Some(response),
            _ = wait_cancel(&inner, &run_id, deadline) => None,
            _ = budget.exhausted.notified() => {
                if let Some(exhaustion) = budget.state.lock().await.exhaustion {
                    drop(permit);
                    finish_agent(
                        &inner,
                        &run_id,
                        budget_output(agent.clone(), usage.clone(), exhaustion),
                        false,
                    )
                    .await;
                    return;
                }
                continue
            },
        };
        drop(permit);
        let Some(response) = response else {
            finish_agent(
                &inner,
                &run_id,
                cancelled_output(agent.clone(), usage),
                false,
            )
            .await;
            return;
        };
        let turn = match response {
            Ok(turn) => turn,
            Err(error) => {
                let execution_error = provider_execution_error(&error);
                let mut payload = BTreeMap::new();
                payload.insert("error".to_owned(), execution_error_value(&execution_error));
                let _ = append_events(
                    &inner,
                    &run_id,
                    vec![activity("model.failed", &agent, payload)],
                    Vec::new(),
                )
                .await;
                let output = AgentOutput {
                    session_id: agent.session_id.clone(),
                    path: agent.path.clone(),
                    status: TerminalAgentStatus::Failed,
                    output: None,
                    usage,
                    error: Some(execution_error),
                };
                finish_agent(&inner, &run_id, output, false).await;
                return;
            }
        };
        turns = turns.saturating_add(1);
        let model_usage = Usage {
            input_tokens: turn.usage.input_tokens,
            output_tokens: turn.usage.output_tokens,
            tool_calls: 0,
        };
        let exhaustion = match record_usage(&budget, &model_usage, token_limit).await {
            Ok(exhausted) => exhausted,
            Err(error) => {
                finish_agent(
                    &inner,
                    &run_id,
                    failed_output(agent.clone(), error.message(), usage),
                    false,
                )
                .await;
                return;
            }
        };
        usage = match add_usage(&usage, &model_usage) {
            Ok(usage) => usage,
            Err(error) => {
                finish_agent(
                    &inner,
                    &run_id,
                    failed_output(agent.clone(), error.message(), usage),
                    false,
                )
                .await;
                return;
            }
        };
        let message = match assistant_message(&agent, &turn) {
            Ok(message) => message,
            Err(error) => {
                finish_agent(
                    &inner,
                    &run_id,
                    failed_output(agent.clone(), error.message(), usage),
                    false,
                )
                .await;
                return;
            }
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
            finish_agent(
                &inner,
                &run_id,
                failed_output(agent.clone(), error.message(), usage),
                false,
            )
            .await;
            return;
        }
        let latest_output = if turn.stop == ModelStop::EndTurn {
            match output_value(&turn, output_contract.as_ref()) {
                Ok(output) => Some(output),
                Err(message) => {
                    finish_agent(
                        &inner,
                        &run_id,
                        failed_output(agent.clone(), message, usage),
                        false,
                    )
                    .await;
                    return;
                }
            }
        } else {
            None
        };
        if let Some(exhaustion) = exhaustion {
            let mut output = budget_output(agent.clone(), usage, exhaustion);
            output.output = latest_output;
            finish_agent(&inner, &run_id, output, false).await;
            return;
        }
        if turn.stop == ModelStop::ToolUse {
            match execute_tool_batch(
                &inner,
                &run_id,
                &agent,
                &turn.tool_calls,
                &mut usage,
                &mut grant_calls,
                &budget,
                tool_limit,
                deadline,
            )
            .await
            {
                ToolBatchOutcome::Continue => {
                    if deliver_pending_steers(&inner, &run_id, &agent.session_id)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                ToolBatchOutcome::Cancelled => {
                    finish_agent(
                        &inner,
                        &run_id,
                        cancelled_output(agent.clone(), usage),
                        false,
                    )
                    .await;
                    return;
                }
                ToolBatchOutcome::BudgetExhausted(exhaustion) => {
                    finish_agent(
                        &inner,
                        &run_id,
                        budget_output(agent.clone(), usage, exhaustion),
                        false,
                    )
                    .await;
                    return;
                }
                ToolBatchOutcome::Failed(message) => {
                    finish_agent(
                        &inner,
                        &run_id,
                        failed_output(agent.clone(), message, usage),
                        false,
                    )
                    .await;
                    return;
                }
            }
        }
        let turn_limit_reached = inner
            .store
            .session(&agent.session_id)
            .await
            .ok()
            .and_then(|session| {
                session
                    .spec
                    .agent
                    .budget
                    .map(|budget| budget.max_turns.get())
            })
            .is_some_and(|limit| turns >= limit);
        let output = AgentOutput {
            session_id: agent.session_id.clone(),
            path: agent.path.clone(),
            status: TerminalAgentStatus::Completed,
            output: latest_output.clone(),
            usage: usage.clone(),
            error: None,
        };
        match inner.store.advance_agent(&run_id, output, true).await {
            Ok(AgentAdvance::Finished) => {
                inner.notify_snapshot(&run_id).await;
                return;
            }
            Ok(AgentAdvance::Pending(delivery)) => {
                if turn_limit_reached {
                    let mut output =
                        budget_output(agent.clone(), usage, BudgetExhaustion::AgentTurns);
                    output.output = latest_output;
                    finish_agent(&inner, &run_id, output, false).await;
                    return;
                }
                if delivery == MessageDelivery::FollowUp {
                    if inner
                        .store
                        .set_agent_status(&run_id, &agent.session_id, AgentStatus::Waiting)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    inner.notify_snapshot(&run_id).await;
                    tokio::task::yield_now().await;
                    let permit = tokio::select! {
                        permit = Arc::clone(&semaphore).acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return },
                        _ = wait_cancel(&inner, &run_id, deadline) => {
                            finish_agent(&inner, &run_id, cancelled_output(agent.clone(), usage), false).await;
                            return;
                        }
                    };
                    drop(permit);
                    if inner
                        .store
                        .set_agent_status(&run_id, &agent.session_id, AgentStatus::Running)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                match inner
                    .store
                    .deliver_send(&run_id, &agent.session_id, delivery)
                    .await
                {
                    Ok(true) => inner.notify_snapshot(&run_id).await,
                    Ok(false) => {}
                    Err(_) => return,
                }
                if delivery == MessageDelivery::Steer {
                    loop {
                        match inner.store.pending_send(&run_id, &agent.session_id).await {
                            Ok(Some(pending)) if pending.delivery == MessageDelivery::Steer => {
                                match inner
                                    .store
                                    .deliver_send(
                                        &run_id,
                                        &agent.session_id,
                                        MessageDelivery::Steer,
                                    )
                                    .await
                                {
                                    Ok(true) => {}
                                    _ => return,
                                }
                            }
                            Ok(_) => break,
                            Err(_) => return,
                        }
                    }
                }
            }
            Err(_) => return,
        }
    }
}

enum ToolBatchOutcome {
    Continue,
    Cancelled,
    BudgetExhausted(BudgetExhaustion),
    Failed(String),
}

enum ToolReservation {
    Execute,
    GrantRejected(MuzenError),
    AgentExhausted(BudgetExhaustion, MuzenError),
    RunExhausted(BudgetExhaustion, MuzenError),
}

async fn execute_tool_batch(
    inner: &Inner,
    run_id: &RunId,
    agent: &AgentSnapshot,
    calls: &[ModelToolCall],
    usage: &mut Usage,
    grant_calls: &mut BTreeMap<(ToolProviderId, String), u64>,
    budget: &RunBudget,
    tool_limit: Option<u64>,
    deadline: Option<Instant>,
) -> ToolBatchOutcome {
    if calls.is_empty() {
        return ToolBatchOutcome::Failed("tool-use turn contained no tool calls".to_owned());
    }
    let session = match inner.store.session(&agent.session_id).await {
        Ok(session) => session,
        Err(error) => return ToolBatchOutcome::Failed(error.message().to_owned()),
    };
    for call in calls {
        if observe_cancel(inner, run_id, deadline).await {
            return ToolBatchOutcome::Cancelled;
        }
        if let Some(exhaustion) = budget.state.lock().await.exhaustion {
            return ToolBatchOutcome::BudgetExhausted(exhaustion);
        }
        let Some(grant) = session
            .spec
            .agent
            .tools
            .iter()
            .find(|grant| grant.provider == call.provider && grant.tool == call.name)
        else {
            let error = MuzenError::permission_denied(format!(
                "tool {} from provider {} is outside agent authority",
                call.name, call.provider
            ));
            if let Err(error) = append_tool_result(inner, run_id, agent, call, Err(&error)).await {
                return ToolBatchOutcome::Failed(error.message().to_owned());
            }
            continue;
        };
        let provider = session
            .spec
            .tool_providers
            .iter()
            .find(|provider| provider.id() == &call.provider);
        if let Some(ToolProvider::Builtin { .. }) = provider {
            let required = match call.name.as_str() {
                "agent.spawn" => Some(ToolEffect::AgentSpawn),
                "agent.message" => Some(ToolEffect::AgentMessage),
                _ => None,
            };
            if required.is_some_and(|effect| !grant.effects.contains(&effect)) {
                let error = MuzenError::permission_denied(format!(
                    "tool {} requires the matching agent effect",
                    call.name
                ));
                if let Err(error) =
                    append_tool_result(inner, run_id, agent, call, Err(&error)).await
                {
                    return ToolBatchOutcome::Failed(error.message().to_owned());
                }
                continue;
            }
        }
        match reserve_tool_call(
            budget,
            usage,
            grant_calls,
            grant,
            session
                .spec
                .agent
                .budget
                .as_ref()
                .map(|value| value.max_tool_calls),
            tool_limit,
        )
        .await
        {
            Ok(ToolReservation::Execute) => {}
            Ok(ToolReservation::GrantRejected(error)) => {
                if let Err(error) =
                    append_tool_result(inner, run_id, agent, call, Err(&error)).await
                {
                    return ToolBatchOutcome::Failed(error.message().to_owned());
                }
                continue;
            }
            Ok(ToolReservation::AgentExhausted(exhaustion, error)) => {
                if let Err(error) =
                    append_tool_result(inner, run_id, agent, call, Err(&error)).await
                {
                    return ToolBatchOutcome::Failed(error.message().to_owned());
                }
                return ToolBatchOutcome::BudgetExhausted(exhaustion);
            }
            Ok(ToolReservation::RunExhausted(exhaustion, error)) => {
                if let Err(error) =
                    append_tool_result(inner, run_id, agent, call, Err(&error)).await
                {
                    return ToolBatchOutcome::Failed(error.message().to_owned());
                }
                return ToolBatchOutcome::BudgetExhausted(exhaustion);
            }
            Err(error) => return ToolBatchOutcome::Failed(error.message().to_owned()),
        }
        if let Err(error) = append_events(
            inner,
            run_id,
            vec![activity("tool.started", agent, tool_payload(call))],
            Vec::new(),
        )
        .await
        {
            return ToolBatchOutcome::Failed(error.message().to_owned());
        }
        let result = execute_tool(inner, run_id, agent, provider, call).await;
        if observe_cancel(inner, run_id, deadline).await {
            return ToolBatchOutcome::Cancelled;
        }
        if let Err(error) = append_tool_result(inner, run_id, agent, call, result.as_ref()).await {
            return ToolBatchOutcome::Failed(error.message().to_owned());
        }
        tokio::task::yield_now().await;
    }
    ToolBatchOutcome::Continue
}

async fn reserve_tool_call(
    budget: &RunBudget,
    usage: &mut Usage,
    grant_calls: &mut BTreeMap<(ToolProviderId, String), u64>,
    grant: &crate::agent_runtime::ToolGrant,
    agent_limit: Option<u32>,
    run_limit: Option<u64>,
) -> Result<ToolReservation, MuzenError> {
    let key = (grant.provider.clone(), grant.tool.clone());
    let grant_used = grant_calls.get(&key).copied().unwrap_or(0);
    if grant
        .max_calls
        .is_some_and(|limit| grant_used >= u64::from(limit.get()))
    {
        return Ok(ToolReservation::GrantRejected(
            MuzenError::resource_exhausted("tool grant maxCalls exhausted"),
        ));
    }
    if agent_limit.is_some_and(|limit| usage.tool_calls >= u64::from(limit)) {
        return Ok(ToolReservation::AgentExhausted(
            BudgetExhaustion::AgentToolCalls,
            MuzenError::resource_exhausted(BudgetExhaustion::AgentToolCalls.message()),
        ));
    }
    let mut state = budget.state.lock().await;
    if run_limit.is_some_and(|limit| state.usage.tool_calls >= limit) {
        state.exhaustion = Some(BudgetExhaustion::RunToolCalls);
        drop(state);
        budget.exhausted.notify_waiters();
        return Ok(ToolReservation::RunExhausted(
            BudgetExhaustion::RunToolCalls,
            MuzenError::resource_exhausted(BudgetExhaustion::RunToolCalls.message()),
        ));
    }
    state.usage.tool_calls = state
        .usage
        .tool_calls
        .checked_add(1)
        .ok_or_else(|| MuzenError::internal("run tool call usage overflow"))?;
    usage.tool_calls = usage
        .tool_calls
        .checked_add(1)
        .ok_or_else(|| MuzenError::internal("agent tool call usage overflow"))?;
    grant_calls.insert(key, grant_used + 1);
    Ok(ToolReservation::Execute)
}

async fn execute_tool(
    inner: &Inner,
    run_id: &RunId,
    agent: &AgentSnapshot,
    provider: Option<&ToolProvider>,
    call: &ModelToolCall,
) -> Result<Value, MuzenError> {
    match provider {
        Some(ToolProvider::McpHttp { .. }) => Err(MuzenError::unsupported(
            "MCP HTTP tool execution is not supported by the local runtime",
        )),
        Some(ToolProvider::Builtin { .. }) => match call.name.as_str() {
            "agent.spawn" => {
                let mut arguments = normalize_builtin_arguments(&call.name, call.arguments.clone());
                let object = arguments.as_object_mut().ok_or_else(|| {
                    MuzenError::invalid_input("agent.spawn arguments must be an object")
                })?;
                object.insert(
                    "parentSessionId".to_owned(),
                    Value::String(agent.session_id.as_str().to_owned()),
                );
                let command: SpawnCommand = serde_json::from_value(arguments).map_err(|error| {
                    MuzenError::invalid_input(format!("invalid agent.spawn arguments: {error}"))
                })?;
                let child = inner.store.spawn_agent(run_id, command).await?;
                inner.notify_snapshot(run_id).await;
                Ok(json!(child))
            }
            "agent.message" => {
                let arguments = normalize_builtin_arguments(&call.name, call.arguments.clone());
                let command: SendCommand = serde_json::from_value(arguments).map_err(|error| {
                    MuzenError::invalid_input(format!("invalid agent.message arguments: {error}"))
                })?;
                let receipt = inner.store.accept_send(run_id, command).await?;
                inner.notify_snapshot(run_id).await;
                Ok(json!(receipt.sequence.get()))
            }
            _ => Err(MuzenError::unsupported(format!(
                "built-in tool {} is not supported",
                call.name
            ))),
        },
        None => Err(MuzenError::permission_denied(format!(
            "tool provider {} is not available",
            call.provider
        ))),
    }
}

fn normalize_builtin_arguments(tool: &str, mut arguments: Value) -> Value {
    let Some(object) = arguments.as_object_mut() else {
        return arguments;
    };
    match tool {
        "agent.spawn" => {
            if let Some(agent) = object.get_mut("agent").and_then(Value::as_object_mut) {
                if let Some(instructions) = agent.get_mut("instructions") {
                    normalize_content_blocks(instructions);
                }
            }
            if let Some(input) = object.get_mut("input") {
                normalize_agent_input(input);
            }
        }
        "agent.message" => {
            if let Some(input) = object.get_mut("input") {
                normalize_agent_input(input);
            }
        }
        _ => {}
    }
    arguments
}

fn normalize_agent_input(input: &mut Value) {
    if input.is_string() {
        let content = std::mem::take(input);
        *input = json!({ "content": content });
    }
    if let Some(content) = input
        .as_object_mut()
        .and_then(|object| object.get_mut("content"))
    {
        normalize_content_blocks(content);
    }
}

fn normalize_content_blocks(content: &mut Value) {
    if content.is_string() {
        let text = std::mem::take(content);
        *content = Value::Array(vec![text]);
    }
    if let Some(blocks) = content.as_array_mut() {
        for block in blocks {
            if let Some(text) = block.as_str().map(str::to_owned) {
                *block = json!({ "type": "text", "text": text });
            }
        }
    }
}

async fn append_tool_result(
    inner: &Inner,
    run_id: &RunId,
    agent: &AgentSnapshot,
    call: &ModelToolCall,
    result: Result<&Value, &MuzenError>,
) -> Result<(), MuzenError> {
    let (event_type, envelope, error) = match result {
        Ok(value) => (
            "tool.completed",
            json!({
                "callId": call.id,
                "provider": call.provider,
                "tool": call.name,
                "arguments": call.arguments,
                "result": value,
            }),
            None,
        ),
        Err(error) => {
            let value = muzen_error_value(error);
            (
                "tool.failed",
                json!({
                    "callId": call.id,
                    "provider": call.provider,
                    "tool": call.name,
                    "arguments": call.arguments,
                    "error": value,
                }),
                Some(value),
            )
        }
    };
    let mut payload = tool_payload(call);
    if let Some(error) = error {
        payload.insert("error".to_owned(), error);
    }
    let text = serde_json::to_string(&envelope)
        .map_err(|error| MuzenError::internal(format!("failed to encode tool result: {error}")))?;
    append_events(
        inner,
        run_id,
        vec![activity(event_type, agent, payload)],
        vec![AgentMessage {
            id: new_message_id(),
            session_id: agent.session_id.clone(),
            role: MessageRole::Tool,
            content: vec![ContentBlock::Text { text }],
            created_at: timestamp()?,
        }],
    )
    .await
}

fn tool_payload(call: &ModelToolCall) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("provider".to_owned(), json!(call.provider)),
        ("tool".to_owned(), json!(call.name)),
    ])
}

fn muzen_error_value(error: &MuzenError) -> Value {
    serde_json::to_value(error).unwrap_or_else(|_| {
        json!({
            "code": "internal",
            "message": "failed to encode tool error",
            "retryable": false,
        })
    })
}

async fn deliver_pending_steers(
    inner: &Inner,
    run_id: &RunId,
    session_id: &crate::agent_runtime::SessionId,
) -> Result<(), MuzenError> {
    loop {
        match inner.store.pending_send(run_id, session_id).await? {
            Some(pending) if pending.delivery == MessageDelivery::Steer => {
                if inner
                    .store
                    .deliver_send(run_id, session_id, MessageDelivery::Steer)
                    .await?
                {
                    inner.notify_snapshot(run_id).await;
                }
            }
            _ => return Ok(()),
        }
    }
}

async fn finish_agent(inner: &Inner, run_id: &RunId, output: AgentOutput, allow_pending: bool) {
    let _ = inner
        .store
        .advance_agent(run_id, output, allow_pending)
        .await;
    inner.notify_snapshot(run_id).await;
}

async fn finish(inner: &Inner, run_id: &RunId) -> Result<(), MuzenError> {
    let stored = inner.store.run(run_id).await?;
    let outputs = stored.outputs.clone();
    if outputs.len() != stored.snapshot.agents.len() {
        return Err(MuzenError::conflict(
            "not every tracked agent has a durable output",
        ));
    }
    let usage = sum_usage(&outputs)?;
    let cancelled = inner.store.cancel_requested(run_id).await?;
    let completed = outputs
        .iter()
        .filter(|output| output.status == TerminalAgentStatus::Completed)
        .count();
    let completed_roots = outputs
        .iter()
        .filter(|output| {
            output.status == TerminalAgentStatus::Completed
                && stored.snapshot.roots.contains(&output.session_id)
        })
        .count();
    let status = if completed == outputs.len() {
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
            run_id,
            FinishRun {
                status,
                outputs,
                usage,
                artifacts: Vec::new(),
                metadata: stored.spec.metadata,
            },
        )
        .await?;
    inner.notify_snapshot(run_id).await;
    Ok(())
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
        tool_providers: session.spec.tool_providers,
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
                tokio::select! { _ = receiver.changed() => {}, _ = tokio::time::sleep_until(deadline) => request_deadline(inner, run_id).await }
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
    budget: &RunBudget,
    usage: &Usage,
    limit: Option<u64>,
) -> Result<Option<BudgetExhaustion>, MuzenError> {
    let mut state = budget.state.lock().await;
    state.usage = add_usage(&state.usage, usage)?;
    if state.exhaustion.is_none()
        && limit.is_some_and(|limit| {
            state
                .usage
                .input_tokens
                .saturating_add(state.usage.output_tokens)
                >= limit
        })
    {
        state.exhaustion = Some(BudgetExhaustion::RunTokens);
        budget.exhausted.notify_waiters();
    }
    Ok(state.exhaustion)
}

fn assistant_message(agent: &AgentSnapshot, turn: &ModelTurn) -> Result<AgentMessage, MuzenError> {
    let mut content = turn.content.clone();
    if !turn.tool_calls.is_empty() {
        let calls = turn
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "provider": call.provider,
                    "name": call.name,
                    "arguments": call.arguments,
                })
            })
            .collect::<Vec<_>>();
        let text = serde_json::to_string(&json!({
            "_muzen": super::provider::ASSISTANT_TOOL_ENVELOPE,
            "calls": calls,
        }))
        .map_err(|error| {
            MuzenError::internal(format!("failed to encode assistant tool calls: {error}"))
        })?;
        content.push(ContentBlock::Text { text });
    }
    Ok(AgentMessage {
        id: new_message_id(),
        session_id: agent.session_id.clone(),
        role: MessageRole::Assistant,
        content,
        created_at: timestamp()?,
    })
}

fn output_value(turn: &ModelTurn, contract: Option<&OutputContract>) -> Result<Value, String> {
    let Some(contract) = contract else {
        return Ok(raw_output_value(turn));
    };
    let mut text = String::new();
    for block in &turn.content {
        match block {
            ContentBlock::Text { text: block } => text.push_str(block),
            _ => {
                return Err(
                    "output schema violation at $: final assistant output must contain only text"
                        .to_owned(),
                )
            }
        }
    }
    let value = serde_json::from_str(&text).map_err(|error| {
        format!("output schema violation at $: assistant output is not valid JSON: {error}")
    })?;
    validate_instance(&contract.schema, &value).map_err(|error| {
        format!(
            "output schema violation at {}: {}",
            error.path, error.message
        )
    })?;
    Ok(value)
}

fn raw_output_value(turn: &ModelTurn) -> Value {
    match turn.content.as_slice() {
        [ContentBlock::Text { text }] => Value::String(text.clone()),
        content => serde_json::to_value(content).unwrap_or(Value::Null),
    }
}

fn provider_execution_error(error: &ModelProviderError) -> ExecutionError {
    ExecutionError {
        code: error.code(),
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
    serde_json::to_value(error).unwrap_or_else(
        |_| json!({ "code": "model_error", "message": "provider failed", "retryable": false }),
    )
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

fn budget_output(agent: AgentSnapshot, usage: Usage, exhaustion: BudgetExhaustion) -> AgentOutput {
    AgentOutput {
        session_id: agent.session_id,
        path: agent.path,
        status: TerminalAgentStatus::BudgetExhausted,
        output: None,
        usage,
        error: Some(execution_error(
            ExecutionErrorCode::BudgetExhausted,
            exhaustion.message(),
        )),
    }
}

fn failed_output(agent: AgentSnapshot, message: impl Into<String>, usage: Usage) -> AgentOutput {
    AgentOutput {
        session_id: agent.session_id,
        path: agent.path,
        status: TerminalAgentStatus::Failed,
        output: None,
        usage,
        error: Some(execution_error(ExecutionErrorCode::ModelError, message)),
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
    for agent in stored
        .snapshot
        .agents
        .into_iter()
        .filter(|agent| !terminal_agent(agent.status))
    {
        let output = if cancelled {
            cancelled_output(agent, Usage::default())
        } else {
            failed_output(agent, message, Usage::default())
        };
        let _ = inner.store.advance_agent(run_id, output, false).await;
    }
    let _ = finish(inner, run_id).await;
}
