use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};

use super::{
    LocalRuntime, LocalRuntimeConfig, ModelProvider, ModelProviderError, ModelRequest, ModelStop,
    ModelTurn,
};
use crate::agent_runtime::client::RuntimeTransport;
use crate::agent_runtime::{
    AgentEvent, AgentInput, AgentStatus, CancelOptions, ContentBlock, CreateOptions, ErrorCode,
    EventOptions, ExistingSessionRoot, IdempotencyKey, MessageDelivery, MessagePage, MessageRole,
    ModelProfileId, Muzen, Run, RunLimits, RunRoot, RunSpec, SendCommand, SessionId, SessionSpec,
    SingleRunOptions, SpawnCommand, TerminalAgentStatus, TerminalRunStatus, Usage,
};

enum Script {
    Turn {
        delay: Duration,
        turn: ModelTurn,
    },
    Error {
        delay: Duration,
        error: ModelProviderError,
    },
}

struct ScriptedProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ModelRequest>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl ScriptedProvider {
    fn new(scripts: impl IntoIterator<Item = Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        })
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::Acquire)
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().clone()
    }
}

struct ActiveCall<'a>(&'a ScriptedProvider);

impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelTurn, ModelProviderError> {
        self.requests.lock().push(request);
        let script = self
            .scripts
            .lock()
            .pop_front()
            .expect("scripted provider turn");
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
        let _active = ActiveCall(self);
        match script {
            Script::Turn { delay, turn } => {
                tokio::time::sleep(delay).await;
                Ok(turn)
            }
            Script::Error { delay, error } => {
                tokio::time::sleep(delay).await;
                Err(error)
            }
        }
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../fixtures/agent-interface-v1.json"))
        .expect("agent fixture")
}

fn session_spec() -> SessionSpec {
    serde_json::from_value(fixture()["sessionSpec"].clone()).expect("session fixture")
}

fn input(text: &str) -> AgentInput {
    AgentInput {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    }
}

fn limits() -> RunLimits {
    RunLimits {
        max_active_agents: NonZeroU32::new(4).expect("limit"),
        max_agents: NonZeroU32::new(4).expect("limit"),
        max_depth: 0,
        max_input_bytes: NonZeroU64::new(1024).expect("limit"),
        max_total_tokens: None,
        max_total_tool_calls: Some(0),
        deadline_ms: None,
    }
}

fn turn(text: &str, input_tokens: u64, output_tokens: u64) -> Script {
    Script::Turn {
        delay: Duration::ZERO,
        turn: ModelTurn {
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            usage: Usage {
                input_tokens,
                output_tokens,
                tool_calls: 0,
            },
            stop: ModelStop::EndTurn,
        },
    }
}

fn delayed_turn(delay: Duration, text: &str) -> Script {
    Script::Turn {
        delay,
        turn: ModelTurn {
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
                tool_calls: 0,
            },
            stop: ModelStop::EndTurn,
        },
    }
}

async fn memory_runtime(provider: Arc<ScriptedProvider>) -> Muzen {
    Muzen::local(LocalRuntimeConfig::memory(provider))
        .await
        .expect("local runtime")
}

async fn create_sessions(muzen: &Muzen, count: usize) -> Vec<SessionId> {
    let mut ids = Vec::new();
    for _ in 0..count {
        ids.push(
            muzen
                .create_session(session_spec(), CreateOptions::default())
                .await
                .expect("create session")
                .id()
                .clone(),
        );
    }
    ids
}

async fn start_roots(muzen: &Muzen, ids: &[SessionId], limits: RunLimits) -> Run {
    muzen
        .start_run(RunSpec {
            roots: ids
                .iter()
                .map(|session_id| {
                    RunRoot::Existing(ExistingSessionRoot {
                        session_id: session_id.clone(),
                        input: input(session_id.as_str()),
                    })
                })
                .collect(),
            limits,
            idempotency_key: None,
            metadata: Default::default(),
        })
        .await
        .expect("start run")
}

async fn all_events(run: &Run) -> Vec<AgentEvent> {
    run.events(EventOptions::default())
        .try_collect()
        .await
        .expect("run events")
}

async fn wait_for_event(run: &Run, event_type: &str) -> AgentEvent {
    let mut events = run.events(EventOptions::default());
    while let Some(event) = events.next().await {
        let event = event.expect("run event");
        if event.event_type == event_type {
            return event;
        }
    }
    panic!("event stream ended before {event_type}")
}

#[tokio::test]
async fn single_root_completes_through_public_api() {
    let provider = ScriptedProvider::new([turn("done", 3, 2)]);
    let muzen = memory_runtime(provider).await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run = session
        .run(
            input("hello"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let result = run.wait().await.expect("durable result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert_eq!(result.usage.input_tokens, 3);
    assert_eq!(result.usage.output_tokens, 2);
    assert_eq!(run.result().await.expect("result read"), Some(result));
    let events = all_events(&run).await;
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "run.queued",
            "agent.created",
            "run.started",
            "agent.started",
            "model.started",
            "message.accepted",
            "model.completed",
            "agent.completed",
            "run.completed",
        ]
    );
    assert!(events
        .iter()
        .enumerate()
        .all(|(index, event)| event.sequence == index as u64 + 1));
    let messages = session
        .messages(MessagePage::default())
        .await
        .expect("messages");
    assert_eq!(messages.items.len(), 2);
    assert_eq!(messages.items[0].role, MessageRole::User);
    assert_eq!(messages.items[1].role, MessageRole::Assistant);
}

#[tokio::test]
async fn max_active_agents_one_serializes_multiple_roots() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(20), "one"),
        delayed_turn(Duration::from_millis(20), "two"),
    ]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let result = start_roots(&muzen, &ids, run_limits)
        .await
        .wait()
        .await
        .expect("result");
    assert_eq!(result.status, TerminalRunStatus::Completed);
    assert!(result
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::Completed));
    assert_eq!(provider.max_active(), 1);
}

#[tokio::test]
async fn one_provider_error_makes_multi_root_run_partial() {
    let provider = ScriptedProvider::new([
        Script::Error {
            delay: Duration::ZERO,
            error: ModelProviderError::new("provider unavailable").with_retryable(true),
        },
        turn("recovered", 2, 1),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    let result = run.wait().await.expect("result");
    assert_eq!(result.status, TerminalRunStatus::Partial);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Failed);
    assert_eq!(result.outputs[1].status, TerminalAgentStatus::Completed);
    assert!(all_events(&run)
        .await
        .iter()
        .any(|event| event.event_type == "model.failed"));
}

#[tokio::test]
async fn cancellation_interrupts_model_and_is_durable_and_idempotent() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_secs(5), "late")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    let first = run
        .cancel(CancelOptions {
            reason: Some("operator".to_owned()),
            idempotency_key: None,
        })
        .await
        .expect("cancel");
    assert_eq!(
        run.cancel(CancelOptions::default())
            .await
            .expect("cancel replay"),
        first
    );
    let result = run.wait().await.expect("cancelled result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Cancelled);
    assert!(all_events(&run)
        .await
        .iter()
        .any(|event| event.event_type == "run.cancel_requested"));
}

#[tokio::test]
async fn close_cancels_and_joins_in_flight_runs() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_secs(5), "late")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    muzen.close().await.expect("close runtime");
    let result = run.result().await.expect("result read").expect("result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Cancelled);
}

#[tokio::test]
async fn total_token_limit_marks_agent_budget_exhausted() {
    let provider = ScriptedProvider::new([turn("too expensive", 3, 2)]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_total_tokens = NonZeroU64::new(5);
    let result = start_roots(&muzen, &ids, run_limits)
        .await
        .wait()
        .await
        .expect("result");
    assert_eq!(result.status, TerminalRunStatus::Failed);
    assert_eq!(
        result.outputs[0].status,
        TerminalAgentStatus::BudgetExhausted
    );
    assert_eq!(result.usage.input_tokens + result.usage.output_tokens, 5);
}

#[tokio::test]
async fn deadline_records_cancellation_intent_and_cancelled_result() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_secs(5), "late")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.deadline_ms = NonZeroU64::new(20);
    let run = start_roots(&muzen, &ids, run_limits).await;
    let result = run.wait().await.expect("deadline result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    let events = all_events(&run).await;
    let cancel = events
        .iter()
        .find(|event| event.event_type == "run.cancel_requested")
        .expect("deadline cancellation event");
    assert_eq!(cancel.payload.get("reason"), Some(&json!("deadline")));
}

#[tokio::test]
async fn event_tail_after_mid_run_sequence_has_no_gap_or_duplicate() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(50), "done")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    let started = wait_for_event(&run, "model.started").await;
    assert_eq!(started.sequence, 5);
    let tail = run
        .events(EventOptions { after: Some(3) })
        .try_collect::<Vec<_>>()
        .await
        .expect("event tail");
    assert_eq!(
        tail.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        (4..=9).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn finished_run_replay_after_middle_delivers_terminal_event() {
    let provider = ScriptedProvider::new([turn("done", 2, 1)]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    run.wait().await.expect("result");
    let tail = run
        .events(EventOptions { after: Some(5) })
        .try_collect::<Vec<_>>()
        .await
        .expect("finished event tail");
    assert_eq!(
        tail.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        (6..=9).collect::<Vec<_>>()
    );
    assert_eq!(
        tail.last().expect("terminal event").event_type,
        "run.completed"
    );
}

#[tokio::test]
async fn parked_subscriber_drains_tail_after_terminal_state_cleanup() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(50), "done")]);
    let runtime = LocalRuntime::connect(LocalRuntimeConfig::memory(provider))
        .await
        .expect("local runtime");
    let session_id = runtime
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let run_id = runtime
        .start_run(RunSpec {
            roots: vec![RunRoot::Existing(ExistingSessionRoot {
                session_id,
                input: input("park"),
            })],
            limits: limits(),
            idempotency_key: None,
            metadata: Default::default(),
        })
        .await
        .expect("run");
    let mut prefix = runtime.events(&run_id, EventOptions::default());
    while let Some(event) = prefix.next().await {
        if event.expect("prefix event").event_type == "model.started" {
            break;
        }
    }
    drop(prefix);
    let tail = runtime
        .events(&run_id, EventOptions { after: Some(5) })
        .try_collect::<Vec<_>>()
        .await
        .expect("parked event tail");
    assert_eq!(
        tail.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        (6..=9).collect::<Vec<_>>()
    );
    assert_eq!(
        tail.last().expect("terminal event").event_type,
        "run.completed"
    );
    assert!(runtime.inner.notifications.lock().is_empty());
    assert!(runtime.inner.scheduled.lock().is_empty());
    assert!(runtime.inner.tasks.lock().is_empty());
}

#[tokio::test]
async fn follow_up_extends_run_and_accumulates_usage() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "first"),
        turn("second", 4, 2),
    ]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    run.send(SendCommand {
        session_id: ids[0].clone(),
        input: input("continue"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: None,
    })
    .await
    .expect("accept follow-up");
    let result = run.wait().await.expect("extended result");
    assert_eq!(result.usage.input_tokens, 6);
    assert_eq!(result.usage.output_tokens, 3);
    assert_eq!(provider.requests().len(), 2);
    let second = &provider.requests()[1].transcript;
    assert_eq!(second.len(), 3);
    assert_eq!(second[0].role, MessageRole::User);
    assert_eq!(second[1].role, MessageRole::Assistant);
    assert_eq!(second[2].role, MessageRole::User);
    assert_eq!(second[2].content, input("continue").content);
    let events = all_events(&run).await;
    assert!(events
        .iter()
        .any(|event| event.event_type == "agent.waiting"));
    assert!(events.iter().any(|event| event.event_type == "run.waiting"));
    assert!(events
        .iter()
        .enumerate()
        .all(|(index, event)| event.sequence == index as u64 + 1));
}

#[tokio::test]
async fn steer_mid_model_call_is_delivered_after_assistant_before_next_request() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "first"),
        turn("steered", 1, 1),
    ]);
    let muzen = memory_runtime(Arc::clone(&provider)).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    run.send(SendCommand {
        session_id: ids[0].clone(),
        input: input("steer now"),
        delivery: MessageDelivery::Steer,
        idempotency_key: None,
    })
    .await
    .expect("accept steer");
    run.wait().await.expect("steered result");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].transcript.len(), 3);
    assert_eq!(requests[1].transcript[1].role, MessageRole::Assistant);
    assert_eq!(
        requests[1].transcript[2].content,
        input("steer now").content
    );
}

#[tokio::test]
async fn send_errors_and_idempotency_follow_command_contract() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "first"),
        turn("second", 1, 1),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let run = start_roots(&muzen, &ids[..1], limits()).await;
    wait_for_event(&run, "model.started").await;
    let key = IdempotencyKey::new("send-key").expect("key");
    let command = SendCommand {
        session_id: ids[0].clone(),
        input: input("again"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: Some(key.clone()),
    };
    let receipt = run.send(command.clone()).await.expect("first send");
    assert_eq!(run.send(command).await.expect("replay send"), receipt);
    let mut changed = SendCommand {
        session_id: ids[0].clone(),
        input: input("different"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: Some(key),
    };
    assert_eq!(
        run.send(changed.clone())
            .await
            .expect_err("changed replay")
            .code(),
        ErrorCode::Conflict
    );
    changed.idempotency_key = None;
    changed.session_id = ids[1].clone();
    assert_eq!(
        run.send(changed)
            .await
            .expect_err("untracked target")
            .code(),
        ErrorCode::NotFound
    );
    run.wait().await.expect("result");
    assert_eq!(
        run.send(SendCommand {
            session_id: ids[0].clone(),
            input: input("late"),
            delivery: MessageDelivery::FollowUp,
            idempotency_key: None
        })
        .await
        .expect_err("terminal send")
        .code(),
        ErrorCode::Conflict
    );
}

#[tokio::test]
async fn spawn_creates_ordered_child_and_run_waits_for_it() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "parent"),
        turn("child", 2, 1),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    run_limits.max_depth = 1;
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    let child = run
        .spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("child input"),
            idempotency_key: Some(IdempotencyKey::new("spawn-key").expect("key")),
        })
        .await
        .expect("spawn child");
    let replay = run
        .spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("child input"),
            idempotency_key: Some(IdempotencyKey::new("spawn-key").expect("key")),
        })
        .await
        .expect("spawn replay");
    assert_eq!(child.id(), replay.id());
    assert_eq!(
        run.spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("different child input"),
            idempotency_key: Some(IdempotencyKey::new("spawn-key").expect("key")),
        })
        .await
        .err()
        .expect("changed spawn replay")
        .code(),
        ErrorCode::Conflict
    );
    let snapshot = run.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.agents.len(), 2);
    assert_eq!(snapshot.agents[1].parent_session_id.as_ref(), Some(&ids[0]));
    assert_eq!(snapshot.agents[1].path, vec![0, 0]);
    let result = run.wait().await.expect("spawned result");
    assert_eq!(result.outputs.len(), 2);
    assert_eq!(result.outputs[1].session_id, *child.id());
    assert_eq!(
        child
            .messages(MessagePage::default())
            .await
            .expect("child messages")
            .items[0]
            .content,
        input("child input").content
    );
    assert_eq!(
        child
            .snapshot()
            .await
            .expect("child snapshot")
            .active_run_id,
        None
    );
}

#[tokio::test]
async fn spawn_limits_and_authority_fail_without_creating_child() {
    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(80), "parent")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_agents = NonZeroU32::new(1).expect("limit");
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    let command = SpawnCommand {
        parent_session_id: ids[0].clone(),
        agent: session_spec().agent,
        input: input("child"),
        idempotency_key: None,
    };
    assert_eq!(
        run.spawn(command).await.err().expect("max agents").code(),
        ErrorCode::ResourceExhausted
    );
    assert_eq!(run.snapshot().await.expect("snapshot").agents.len(), 1);
    run.cancel(CancelOptions::default()).await.expect("cancel");
    run.wait().await.expect("cancelled");

    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(80), "parent")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let run = start_roots(&muzen, &ids, limits()).await;
    wait_for_event(&run, "model.started").await;
    assert_eq!(
        run.spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent: session_spec().agent,
            input: input("child"),
            idempotency_key: None
        })
        .await
        .err()
        .expect("max depth")
        .code(),
        ErrorCode::ResourceExhausted
    );
    assert_eq!(run.snapshot().await.expect("snapshot").agents.len(), 1);
    run.cancel(CancelOptions::default()).await.expect("cancel");
    run.wait().await.expect("cancelled");

    let provider = ScriptedProvider::new([delayed_turn(Duration::from_millis(80), "parent")]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut authority_limits = limits();
    authority_limits.max_depth = 1;
    let run = start_roots(&muzen, &ids, authority_limits).await;
    wait_for_event(&run, "model.started").await;
    let mut agent = session_spec().agent;
    agent.model = ModelProfileId::new("outside").expect("model id");
    assert_eq!(
        run.spawn(SpawnCommand {
            parent_session_id: ids[0].clone(),
            agent,
            input: input("child"),
            idempotency_key: None
        })
        .await
        .err()
        .expect("model authority")
        .code(),
        ErrorCode::PermissionDenied
    );
    assert_eq!(run.snapshot().await.expect("snapshot").agents.len(), 1);
    run.cancel(CancelOptions::default()).await.expect("cancel");
    run.wait().await.expect("cancelled");
}

#[tokio::test]
async fn failed_child_makes_completed_root_run_partial() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(40), "parent"),
        Script::Error {
            delay: Duration::ZERO,
            error: ModelProviderError::new("child failed"),
        },
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 1).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    run_limits.max_depth = 1;
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    run.spawn(SpawnCommand {
        parent_session_id: ids[0].clone(),
        agent: session_spec().agent,
        input: input("child"),
        idempotency_key: None,
    })
    .await
    .expect("spawn child");
    let result = run.wait().await.expect("partial result");
    assert_eq!(result.status, TerminalRunStatus::Partial);
    assert_eq!(result.outputs[0].status, TerminalAgentStatus::Completed);
    assert_eq!(result.outputs[1].status, TerminalAgentStatus::Failed);
}

#[tokio::test]
async fn cancellation_interrupts_agent_waiting_on_follow_up() {
    let provider = ScriptedProvider::new([
        delayed_turn(Duration::from_millis(30), "first"),
        delayed_turn(Duration::from_secs(5), "second root"),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "model.started").await;
    run.send(SendCommand {
        session_id: ids[0].clone(),
        input: input("follow"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: None,
    })
    .await
    .expect("follow-up");
    wait_for_event(&run, "agent.waiting").await;
    assert_eq!(
        run.snapshot().await.expect("waiting snapshot").agents[0].status,
        AgentStatus::Waiting
    );
    run.cancel(CancelOptions::default()).await.expect("cancel");
    let result = run.wait().await.expect("cancelled result");
    assert_eq!(result.status, TerminalRunStatus::Cancelled);
    assert!(result
        .outputs
        .iter()
        .all(|output| output.status == TerminalAgentStatus::Cancelled));
}

#[tokio::test]
async fn send_to_terminal_agent_in_live_run_conflicts() {
    let provider = ScriptedProvider::new([
        turn("first done", 1, 1),
        delayed_turn(Duration::from_secs(5), "second late"),
    ]);
    let muzen = memory_runtime(provider).await;
    let ids = create_sessions(&muzen, 2).await;
    let mut run_limits = limits();
    run_limits.max_active_agents = NonZeroU32::new(1).expect("limit");
    let run = start_roots(&muzen, &ids, run_limits).await;
    wait_for_event(&run, "agent.completed").await;
    assert_eq!(
        run.send(SendCommand {
            session_id: ids[0].clone(),
            input: input("too late"),
            delivery: MessageDelivery::FollowUp,
            idempotency_key: None,
        })
        .await
        .expect_err("terminal agent send")
        .code(),
        ErrorCode::Conflict
    );
    run.cancel(CancelOptions::default()).await.expect("cancel");
    let result = run.wait().await.expect("partial result");
    assert_eq!(result.status, TerminalRunStatus::Partial);
}

#[tokio::test]
async fn sqlite_reopen_preserves_result_events_and_messages() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("local-runtime.db");
    let provider = ScriptedProvider::new([turn("durable", 2, 1)]);
    let muzen = sqlite_runtime(&path, provider).await;
    let session = muzen
        .create_session(session_spec(), CreateOptions::default())
        .await
        .expect("session");
    let session_id = session.id().clone();
    let run = session
        .run(
            input("persist"),
            SingleRunOptions {
                limits: limits(),
                idempotency_key: None,
                metadata: Default::default(),
            },
        )
        .await
        .expect("run");
    let run_id = run.id().clone();
    run.wait().await.expect("result");
    muzen.close().await.expect("close runtime");

    let reopened = sqlite_runtime(&path, ScriptedProvider::new([])).await;
    let run = reopened.get_run(&run_id).await.expect("reopened run");
    assert_eq!(
        run.result()
            .await
            .expect("persisted result")
            .expect("result")
            .status,
        TerminalRunStatus::Completed
    );
    assert_eq!(all_events(&run).await.len(), 9);
    let session = reopened
        .get_session(&session_id)
        .await
        .expect("reopened session");
    assert_eq!(
        session
            .messages(MessagePage::default())
            .await
            .expect("persisted messages")
            .items
            .len(),
        2
    );
}

async fn sqlite_runtime(path: &Path, provider: Arc<ScriptedProvider>) -> Muzen {
    Muzen::local(LocalRuntimeConfig::sqlite(provider, path))
        .await
        .expect("SQLite local runtime")
}
