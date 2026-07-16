use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

use serde_json::{json, Value};

use super::support::{new_message_id, timestamp};
use super::{ActivityEvent, AgentStore, FinishRun, RunActivity};
use crate::agent_runtime::{
    AgentInput, AgentMessage, AgentOutput, AgentStatus, ContentBlock, ErrorCode,
    ExistingSessionRoot, IdempotencyKey, MessageDelivery, MessagePage, MessageRole, NewSessionRoot,
    RunId, RunRoot, RunSpec, RunStatus, SendCommand, SessionId, SessionSpec, SpawnCommand,
    TerminalAgentStatus, TerminalRunStatus, Usage,
};

pub(super) struct ConformanceOutcome {
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
}

pub(super) async fn assert_store_conformance<S: AgentStore>(store: &S) -> ConformanceOutcome {
    let session_key = key("conformance-session");
    let first = store
        .create_session(session_spec(), Some(&session_key))
        .await
        .expect("create first session");
    assert_eq!(
        store
            .create_session(session_spec(), Some(&session_key))
            .await
            .expect("replay session create"),
        first
    );
    let mut changed = session_spec();
    changed.metadata.insert("changed".to_owned(), json!(true));
    assert_eq!(
        store
            .create_session(changed, Some(&session_key))
            .await
            .expect_err("changed idempotent body")
            .code(),
        ErrorCode::Conflict
    );
    let second = store
        .create_session(session_spec(), None)
        .await
        .expect("create second session");

    let run_key = "conformance-run";
    let first_run = store
        .create_run(run_spec(std::slice::from_ref(&first), Some(run_key)))
        .await
        .expect("create run");
    assert_eq!(
        store
            .create_run(run_spec(std::slice::from_ref(&first), Some(run_key)))
            .await
            .expect("replay run create"),
        first_run
    );
    let atomic_error = store
        .create_run(run_spec(&[second.clone(), first.clone()], None))
        .await
        .expect_err("mixed active claim");
    assert_eq!(atomic_error.code(), ErrorCode::Conflict);
    let second_run = store
        .create_run(run_spec(std::slice::from_ref(&second), None))
        .await
        .expect("failed mixed claim must not claim second session");

    store
        .mark_run_running(&first_run)
        .await
        .expect("start first run");
    assert!(!store
        .cancel_requested(&first_run)
        .await
        .expect("read cancellation state"));
    store
        .append_activity(
            &first_run,
            RunActivity {
                events: vec![
                    ActivityEvent {
                        event_type: "model.started".to_owned(),
                        session_id: Some(first.clone()),
                        payload: BTreeMap::new(),
                    },
                    ActivityEvent {
                        event_type: "message.accepted".to_owned(),
                        session_id: Some(first.clone()),
                        payload: BTreeMap::new(),
                    },
                    ActivityEvent {
                        event_type: "model.completed".to_owned(),
                        session_id: Some(first.clone()),
                        payload: BTreeMap::new(),
                    },
                ],
                messages: vec![AgentMessage {
                    id: new_message_id(),
                    session_id: first.clone(),
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "assistant".to_owned(),
                    }],
                    created_at: timestamp().expect("message timestamp"),
                }],
            },
        )
        .await
        .expect("append run activity");
    store
        .finish_run(
            &first_run,
            finish_for(store, &first_run, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish first run");
    store
        .finish_run(
            &second_run,
            finish_for(store, &second_run, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish second run");
    let events = store
        .events_after(&first_run, None, NonZeroU64::new(20).expect("event limit"))
        .await
        .expect("read events");
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
            "run.completed"
        ]
    );
    assert!(events
        .iter()
        .enumerate()
        .all(|(index, event)| event.sequence == index as u64 + 1));

    let cancel_run = store
        .create_run(run_spec(std::slice::from_ref(&first), None))
        .await
        .expect("cancel run");
    let cancel = store
        .request_cancel(&cancel_run, Some("conformance"))
        .await
        .expect("request cancellation");
    assert!(store
        .cancel_requested(&cancel_run)
        .await
        .expect("observe cancellation"));
    assert_eq!(
        store
            .request_cancel(&cancel_run, Some("retry"))
            .await
            .expect("replay cancellation"),
        cancel
    );
    store
        .finish_run(
            &cancel_run,
            finish_for(store, &cancel_run, TerminalRunStatus::Cancelled).await,
        )
        .await
        .expect("finish cancelled run");
    assert_eq!(
        store
            .append_activity(
                &cancel_run,
                RunActivity {
                    events: vec![ActivityEvent {
                        event_type: "trace".to_owned(),
                        session_id: None,
                        payload: BTreeMap::new(),
                    }],
                    messages: Vec::new(),
                },
            )
            .await
            .expect_err("terminal runs reject activity")
            .code(),
        ErrorCode::Conflict
    );

    let partial_run = store
        .create_run(run_spec(&[first.clone(), second.clone()], None))
        .await
        .expect("partial run");
    let snapshot = store.run(&partial_run).await.expect("partial snapshot");
    let outputs = snapshot
        .snapshot
        .agents
        .iter()
        .enumerate()
        .map(|(index, agent)| AgentOutput {
            session_id: agent.session_id.clone(),
            path: agent.path.clone(),
            status: if index == 0 {
                TerminalAgentStatus::Completed
            } else {
                TerminalAgentStatus::BudgetExhausted
            },
            output: None,
            usage: Usage::default(),
            error: None,
        })
        .collect();
    store
        .finish_run(
            &partial_run,
            FinishRun {
                status: TerminalRunStatus::Partial,
                outputs,
                usage: Usage::default(),
                artifacts: Vec::new(),
                metadata: BTreeMap::new(),
            },
        )
        .await
        .expect("finish partial run");
    let partial = store.run(&partial_run).await.expect("partial result");
    assert_eq!(partial.snapshot.agents[0].status, AgentStatus::Completed);
    assert_eq!(
        partial.snapshot.agents[1].status,
        AgentStatus::BudgetExhausted
    );

    let messages = store
        .messages(
            &first,
            MessagePage {
                after: None,
                limit: NonZeroU32::new(2),
            },
        )
        .await
        .expect("message page");
    assert_eq!(messages.items.len(), 2);
    let next = messages.next.expect("more messages remain");
    let tail = store
        .messages(
            &first,
            MessagePage {
                after: Some(next),
                limit: NonZeroU32::new(2),
            },
        )
        .await
        .expect("message tail");
    assert_eq!(tail.items.len(), 2);
    assert_eq!(tail.next, None);
    let first_messages = store
        .messages(
            &first,
            MessagePage {
                after: None,
                limit: NonZeroU32::new(10),
            },
        )
        .await
        .expect("ordered messages");
    assert_eq!(first_messages.items[0].role, MessageRole::User);
    assert_eq!(first_messages.items[1].role, MessageRole::Assistant);

    let root = NewSessionRoot {
        session: session_spec(),
        input: input("duplicate"),
        idempotency_key: Some(key("conformance-new-root")),
    };
    let mut duplicate = run_spec(&[], None);
    duplicate.roots = vec![RunRoot::New(root.clone()), RunRoot::New(root.clone())];
    assert_eq!(
        store
            .create_run(duplicate)
            .await
            .expect_err("duplicate nested key")
            .code(),
        ErrorCode::InvalidInput
    );
    let mut valid = run_spec(&[], None);
    valid.roots = vec![RunRoot::New(root)];
    store
        .create_run(valid)
        .await
        .expect("duplicate rejection left no mutation");

    let mut budgeted = session_spec();
    budgeted
        .session_budget
        .as_mut()
        .expect("fixture budget")
        .max_runs = NonZeroU64::new(1);
    let budgeted = store
        .create_session(budgeted, None)
        .await
        .expect("budgeted session");
    let budget_run = store
        .create_run(run_spec(std::slice::from_ref(&budgeted), None))
        .await
        .expect("budget run");
    store
        .finish_run(
            &budget_run,
            finish_for(store, &budget_run, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish budget run");
    assert_eq!(
        store
            .create_run(run_spec(std::slice::from_ref(&budgeted), None))
            .await
            .expect_err("budget exhausted")
            .code(),
        ErrorCode::ResourceExhausted
    );

    let mut command_spec = run_spec(std::slice::from_ref(&first), None);
    command_spec.limits.max_depth = 1;
    let command_run = store.create_run(command_spec).await.expect("command run");
    store
        .mark_run_running(&command_run)
        .await
        .expect("start command run");
    let send_key = key("conformance-send");
    let send = SendCommand {
        session_id: first.clone(),
        input: input("follow-up"),
        delivery: MessageDelivery::FollowUp,
        idempotency_key: Some(send_key.clone()),
    };
    let accepted = store
        .accept_send(&command_run, send.clone())
        .await
        .expect("accept send");
    assert_eq!(
        store
            .accept_send(&command_run, send)
            .await
            .expect("replay send"),
        accepted
    );
    assert_eq!(
        store
            .pending_send(&command_run, &first)
            .await
            .expect("pending send")
            .expect("send")
            .delivery,
        MessageDelivery::FollowUp
    );
    store
        .set_agent_status(&command_run, &first, AgentStatus::Waiting)
        .await
        .expect("agent waiting");
    let waiting = store.run(&command_run).await.expect("waiting snapshot");
    assert_eq!(waiting.snapshot.status, RunStatus::Waiting);
    assert_eq!(waiting.snapshot.agents[0].status, AgentStatus::Waiting);
    assert!(store
        .deliver_send(&command_run, &first, MessageDelivery::FollowUp)
        .await
        .expect("deliver send"));
    store
        .set_agent_status(&command_run, &first, AgentStatus::Running)
        .await
        .expect("agent resumed");

    let command = SpawnCommand {
        parent_session_id: first.clone(),
        agent: session_spec().agent,
        input: input("child"),
        idempotency_key: Some(key("conformance-spawn")),
    };
    let child = store
        .spawn_agent(&command_run, command.clone())
        .await
        .expect("spawn child");
    assert_eq!(
        store
            .spawn_agent(&command_run, command)
            .await
            .expect("replay spawn"),
        child
    );
    let spawned = store.run(&command_run).await.expect("spawn snapshot");
    assert_eq!(spawned.snapshot.agents[1].path, vec![0, 0]);
    assert_eq!(
        spawned.snapshot.agents[1].parent_session_id.as_ref(),
        Some(&first)
    );
    store
        .finish_run(
            &command_run,
            finish_for(store, &command_run, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish command run");

    store
        .archive_session(&first)
        .await
        .expect("archive session");
    store
        .archive_session(&first)
        .await
        .expect("archive is idempotent");
    ConformanceOutcome {
        session_id: first,
        run_id: first_run,
    }
}

async fn finish_for<S: AgentStore>(
    store: &S,
    run_id: &RunId,
    status: TerminalRunStatus,
) -> FinishRun {
    let agent_status = match status {
        TerminalRunStatus::Completed => TerminalAgentStatus::Completed,
        TerminalRunStatus::Partial | TerminalRunStatus::Failed => TerminalAgentStatus::Failed,
        TerminalRunStatus::Cancelled => TerminalAgentStatus::Cancelled,
    };
    let outputs = store
        .run(run_id)
        .await
        .expect("run record")
        .snapshot
        .agents
        .into_iter()
        .map(|agent| AgentOutput {
            session_id: agent.session_id,
            path: agent.path,
            status: agent_status,
            output: None,
            usage: Usage::default(),
            error: None,
        })
        .collect();
    FinishRun {
        status,
        outputs,
        usage: Usage::default(),
        artifacts: Vec::new(),
        metadata: BTreeMap::new(),
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
    serde_json::from_value(json!({
        "content": [{ "type": "text", "text": text }]
    }))
    .expect("agent input")
}

fn run_spec(ids: &[SessionId], idempotency_key: Option<&str>) -> RunSpec {
    let mut spec: RunSpec =
        serde_json::from_value(fixture()["runSpec"].clone()).expect("run fixture");
    spec.roots = ids
        .iter()
        .map(|session_id| {
            RunRoot::Existing(ExistingSessionRoot {
                session_id: session_id.clone(),
                input: input(session_id.as_str()),
            })
        })
        .collect();
    spec.idempotency_key = idempotency_key.map(key);
    spec
}

fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).expect("idempotency key")
}
