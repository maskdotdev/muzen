use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

use serde_json::{json, Value};

use super::{AgentStore, FinishRun};
use crate::agent_runtime::{
    AgentInput, AgentOutput, AgentStatus, ErrorCode, ExistingSessionRoot, IdempotencyKey,
    MessagePage, NewSessionRoot, RunId, RunRoot, RunSpec, SessionId, SessionSpec,
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
    assert_eq!(tail.items.len(), 1);
    assert_eq!(tail.next, None);

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
