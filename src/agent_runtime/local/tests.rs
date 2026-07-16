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
    AgentEvent, AgentInput, CancelOptions, ContentBlock, CreateOptions, EventOptions,
    ExistingSessionRoot, MessagePage, MessageRole, Muzen, Run, RunLimits, RunRoot, RunSpec,
    SessionId, SessionSpec, SingleRunOptions, TerminalAgentStatus, TerminalRunStatus, Usage,
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
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl ScriptedProvider {
    fn new(scripts: impl IntoIterator<Item = Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        })
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::Acquire)
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
    async fn complete(&self, _request: ModelRequest) -> Result<ModelTurn, ModelProviderError> {
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
