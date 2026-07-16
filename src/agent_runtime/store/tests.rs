use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

use serde_json::{json, Value};

use super::memory::MemoryAgentStore;
use super::sqlite::SqliteAgentStore;
use super::{AgentStore, FinishRun};
use crate::agent_runtime::{
    AgentInput, AgentOutput, AgentStatus, ErrorCode, ExistingSessionRoot, IdempotencyKey,
    MessagePage, NewSessionRoot, RunId, RunRoot, RunSpec, SessionSpec, TerminalAgentStatus,
    TerminalRunStatus, Usage,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../fixtures/agent-interface-v1.json"))
        .expect("agent contract fixture should be valid JSON")
}

fn session_spec() -> SessionSpec {
    serde_json::from_value(fixture()["sessionSpec"].clone()).expect("valid session fixture")
}

fn input(label: &str) -> AgentInput {
    serde_json::from_value(json!({
        "content": [{ "type": "text", "text": label }]
    }))
    .expect("valid input")
}

fn run_spec(session_ids: &[crate::agent_runtime::SessionId], key: Option<&str>) -> RunSpec {
    let mut spec: RunSpec =
        serde_json::from_value(fixture()["runSpec"].clone()).expect("valid run fixture");
    spec.roots = session_ids
        .iter()
        .map(|session_id| {
            RunRoot::Existing(ExistingSessionRoot {
                session_id: session_id.clone(),
                input: input(session_id.as_str()),
            })
        })
        .collect();
    spec.idempotency_key = key.map(idempotency_key);
    spec
}

fn idempotency_key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).expect("non-empty idempotency key")
}

async fn finish(store: &MemoryAgentStore, run_id: &RunId, status: TerminalRunStatus) -> FinishRun {
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

#[tokio::test]
async fn session_creation_is_body_sensitive_and_idempotent() {
    let store = MemoryAgentStore::new();
    let key = idempotency_key("session-request");
    let first = store
        .create_session(session_spec(), Some(&key))
        .await
        .expect("first create");
    let replay = store
        .create_session(session_spec(), Some(&key))
        .await
        .expect("same body should replay");
    assert_eq!(first, replay);

    let mut changed = session_spec();
    changed.metadata.insert("changed".to_owned(), json!(true));
    let error = store
        .create_session(changed, Some(&key))
        .await
        .expect_err("different body must conflict");
    assert_eq!(error.code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn run_claims_are_atomic_and_exclusive() {
    let store = MemoryAgentStore::new();
    let first = store
        .create_session(session_spec(), None)
        .await
        .expect("first session");
    let second = store
        .create_session(session_spec(), None)
        .await
        .expect("second session");
    store
        .create_run(run_spec(std::slice::from_ref(&first), None))
        .await
        .expect("claim first session");

    let error = store
        .create_run(run_spec(&[second.clone(), first], None))
        .await
        .expect_err("mixed claim must fail atomically");
    assert_eq!(error.code(), ErrorCode::Conflict);

    store
        .create_run(run_spec(std::slice::from_ref(&second), None))
        .await
        .expect("second session was not partially claimed");
}

#[tokio::test]
async fn lifecycle_persists_gap_free_events_result_and_release() {
    let store = MemoryAgentStore::new();
    let session_id = store
        .create_session(session_spec(), None)
        .await
        .expect("session");
    let run_id = store
        .create_run(run_spec(
            std::slice::from_ref(&session_id),
            Some("run-request"),
        ))
        .await
        .expect("run");
    let replay = store
        .create_run(run_spec(
            std::slice::from_ref(&session_id),
            Some("run-request"),
        ))
        .await
        .expect("run replay does not re-claim");
    assert_eq!(run_id, replay);

    store.mark_run_running(&run_id).await.expect("start run");
    let result = store
        .finish_run(
            &run_id,
            finish(&store, &run_id, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish run");
    assert_eq!(result.status, TerminalRunStatus::Completed);

    let events = store
        .events_after(&run_id, None, NonZeroU64::new(10).expect("non-zero limit"))
        .await
        .expect("events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(events[0].event_type, "run.queued");
    assert_eq!(events[1].event_type, "agent.created");
    assert_eq!(events[2].event_type, "run.started");
    assert_eq!(events[3].event_type, "agent.started");
    assert_eq!(events[4].event_type, "agent.completed");
    assert_eq!(events[5].event_type, "run.completed");
    assert!(events.iter().all(|event| {
        event.timestamp.ends_with('Z')
            && event.timestamp.contains('T')
            && event
                .timestamp
                .rsplit_once('.')
                .is_some_and(|(_, tail)| tail.len() == 4)
    }));

    let tail = store
        .events_after(
            &run_id,
            Some(1),
            NonZeroU64::new(1).expect("non-zero limit"),
        )
        .await
        .expect("event tail");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].sequence, 2);

    let session = store.session(&session_id).await.expect("session record");
    assert_eq!(session.snapshot.active_run_id, None);
    let messages = store
        .messages(
            &session_id,
            MessagePage {
                after: None,
                limit: NonZeroU32::new(1),
            },
        )
        .await
        .expect("message page");
    assert_eq!(messages.items.len(), 1);
    store
        .create_run(run_spec(std::slice::from_ref(&session_id), None))
        .await
        .expect("released session can run again");
}

#[tokio::test]
async fn cancellation_request_is_recorded_exactly_once() {
    let store = MemoryAgentStore::new();
    let session_id = store
        .create_session(session_spec(), None)
        .await
        .expect("session");
    let run_id = store
        .create_run(run_spec(std::slice::from_ref(&session_id), None))
        .await
        .expect("run");

    let first = store
        .request_cancel(&run_id, Some("operator request"))
        .await
        .expect("request cancel");
    let replay = store
        .request_cancel(&run_id, Some("different retry body"))
        .await
        .expect("cancel is exactly once");
    assert_eq!(first, replay);

    store
        .finish_run(
            &run_id,
            finish(&store, &run_id, TerminalRunStatus::Cancelled).await,
        )
        .await
        .expect("acknowledge cancellation");
    assert_eq!(
        store
            .request_cancel(&run_id, None)
            .await
            .expect("recorded cancellation remains replayable"),
        first
    );

    let other = store
        .create_run(run_spec(std::slice::from_ref(&session_id), None))
        .await
        .expect("second run");
    store
        .finish_run(
            &other,
            finish(&store, &other, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish second run");
    let error = store
        .request_cancel(&other, None)
        .await
        .expect_err("never-cancelled terminal run must conflict");
    assert_eq!(error.code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn archive_rejects_active_sessions_and_is_idempotent_after_completion() {
    let store = MemoryAgentStore::new();
    let session_id = store
        .create_session(session_spec(), None)
        .await
        .expect("session");
    let run_id = store
        .create_run(run_spec(std::slice::from_ref(&session_id), None))
        .await
        .expect("run");
    let error = store
        .archive_session(&session_id)
        .await
        .expect_err("active session must not archive");
    assert_eq!(error.code(), ErrorCode::Conflict);

    store
        .finish_run(
            &run_id,
            finish(&store, &run_id, TerminalRunStatus::Failed).await,
        )
        .await
        .expect("finish");
    store.archive_session(&session_id).await.expect("archive");
    store
        .archive_session(&session_id)
        .await
        .expect("archive replay");
}

#[tokio::test]
async fn new_root_idempotency_reuses_only_the_same_session_body() {
    let store = MemoryAgentStore::new();
    let mut spec = run_spec(&[], None);
    spec.roots = vec![RunRoot::New(NewSessionRoot {
        session: session_spec(),
        input: input("first run"),
        idempotency_key: Some(idempotency_key("new-root")),
    })];
    let first_run = store.create_run(spec.clone()).await.expect("first run");
    let first_root = store
        .run(&first_run)
        .await
        .expect("first run record")
        .snapshot
        .roots[0]
        .clone();
    store
        .finish_run(
            &first_run,
            finish(&store, &first_run, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish first run");

    let second_run = store.create_run(spec.clone()).await.expect("second run");
    let second_root = store
        .run(&second_run)
        .await
        .expect("second run record")
        .snapshot
        .roots[0]
        .clone();
    assert_eq!(first_root, second_root);
    store
        .finish_run(
            &second_run,
            finish(&store, &second_run, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish second run");

    let RunRoot::New(root) = &mut spec.roots[0] else {
        panic!("expected new root")
    };
    root.session
        .metadata
        .insert("different".to_owned(), json!(true));
    let error = store
        .create_run(spec)
        .await
        .expect_err("same key with changed session body must conflict");
    assert_eq!(error.code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn session_run_budget_is_enforced_before_claiming() {
    let store = MemoryAgentStore::new();
    let mut spec = session_spec();
    spec.session_budget
        .as_mut()
        .expect("fixture budget")
        .max_runs = Some(NonZeroU64::new(1).expect("non-zero budget"));
    let session_id = store.create_session(spec, None).await.expect("session");
    let run_id = store
        .create_run(run_spec(std::slice::from_ref(&session_id), None))
        .await
        .expect("first run");
    store
        .finish_run(
            &run_id,
            finish(&store, &run_id, TerminalRunStatus::Completed).await,
        )
        .await
        .expect("finish first run");
    let error = store
        .create_run(run_spec(std::slice::from_ref(&session_id), None))
        .await
        .expect_err("run budget must be exhausted");
    assert_eq!(error.code(), ErrorCode::ResourceExhausted);
    assert_eq!(
        store.session(&session_id).await.expect("session").run_count,
        1
    );
}

#[tokio::test]
async fn duplicate_new_root_idempotency_keys_are_rejected_without_mutation() {
    let store = MemoryAgentStore::new();
    let root = NewSessionRoot {
        session: session_spec(),
        input: input("duplicate"),
        idempotency_key: Some(idempotency_key("duplicate-new-root")),
    };
    let mut spec = run_spec(&[], None);
    spec.roots = vec![RunRoot::New(root.clone()), RunRoot::New(root.clone())];
    let error = store
        .create_run(spec)
        .await
        .expect_err("duplicate nested keys must be invalid");
    assert_eq!(error.code(), ErrorCode::InvalidInput);

    let mut valid = run_spec(&[], None);
    valid.roots = vec![RunRoot::New(root)];
    store
        .create_run(valid)
        .await
        .expect("rejected request must leave no idempotency record");
}

#[tokio::test]
async fn partial_finish_preserves_each_agent_terminal_status_and_event() {
    let store = MemoryAgentStore::new();
    let first = store
        .create_session(session_spec(), None)
        .await
        .expect("first session");
    let second = store
        .create_session(session_spec(), None)
        .await
        .expect("second session");
    let run_id = store
        .create_run(run_spec(&[first, second], None))
        .await
        .expect("run");
    let run = store.run(&run_id).await.expect("run record");
    let outputs = run
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
            &run_id,
            FinishRun {
                status: TerminalRunStatus::Partial,
                outputs,
                usage: Usage::default(),
                artifacts: Vec::new(),
                metadata: BTreeMap::new(),
            },
        )
        .await
        .expect("partial finish");

    let snapshot = store.run(&run_id).await.expect("finished run").snapshot;
    assert_eq!(snapshot.agents[0].status, AgentStatus::Completed);
    assert_eq!(snapshot.agents[1].status, AgentStatus::BudgetExhausted);
    let events = store
        .events_after(&run_id, None, NonZeroU64::new(20).expect("limit"))
        .await
        .expect("events");
    assert_eq!(events[3].event_type, "agent.completed");
    assert_eq!(events[4].event_type, "agent.budget_exhausted");
    assert_eq!(events[5].event_type, "run.partial");
}

#[tokio::test]
async fn messages_are_read_in_bounded_cursor_pages() {
    let store = MemoryAgentStore::new();
    let session_id = store
        .create_session(session_spec(), None)
        .await
        .expect("session");
    for _ in 0..3 {
        let run_id = store
            .create_run(run_spec(std::slice::from_ref(&session_id), None))
            .await
            .expect("run");
        store
            .finish_run(
                &run_id,
                finish(&store, &run_id, TerminalRunStatus::Completed).await,
            )
            .await
            .expect("finish run");
    }

    let first = store
        .messages(
            &session_id,
            MessagePage {
                after: None,
                limit: NonZeroU32::new(2),
            },
        )
        .await
        .expect("first page");
    assert_eq!(first.items.len(), 2);
    let cursor = first.next.expect("more messages remain");
    let second = store
        .messages(
            &session_id,
            MessagePage {
                after: Some(cursor),
                limit: NonZeroU32::new(2),
            },
        )
        .await
        .expect("second page");
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.next, None);
}

#[tokio::test]
async fn shared_conformance_runs_against_memory() {
    super::conformance::assert_store_conformance(&MemoryAgentStore::new()).await;
}

#[tokio::test]
async fn shared_conformance_runs_against_sqlite_and_survives_reopen() {
    let directory = tempfile::tempdir().expect("temporary agent store directory");
    let path = directory.path().join("agents.db");
    let store = SqliteAgentStore::connect(&path)
        .await
        .expect("connect SQLite agent store");
    let outcome = super::conformance::assert_store_conformance(&store).await;
    drop(store);

    let reopened = SqliteAgentStore::connect(&path)
        .await
        .expect("reopen SQLite agent store");
    let session = reopened
        .session(&outcome.session_id)
        .await
        .expect("persisted session");
    assert_eq!(
        session.snapshot.status,
        crate::agent_runtime::SessionStatus::Archived
    );
    assert_eq!(
        reopened
            .create_session(
                session_spec(),
                Some(&idempotency_key("conformance-session")),
            )
            .await
            .expect("idempotency survives reopen"),
        outcome.session_id
    );
    let run = reopened.run(&outcome.run_id).await.expect("persisted run");
    assert_eq!(
        run.snapshot.status,
        crate::agent_runtime::RunStatus::Completed
    );
    assert!(run.result.is_some());
    assert_eq!(
        reopened
            .events_after(
                &outcome.run_id,
                None,
                NonZeroU64::new(20).expect("event limit"),
            )
            .await
            .expect("persisted events")
            .len(),
        6
    );
    assert!(!reopened
        .messages(
            &outcome.session_id,
            MessagePage {
                after: None,
                limit: NonZeroU32::new(1),
            },
        )
        .await
        .expect("persisted messages")
        .items
        .is_empty());
}

#[tokio::test]
async fn sqlite_serializes_concurrent_idempotency_and_run_claims() {
    let directory = tempfile::tempdir().expect("temporary agent store directory");
    let store = std::sync::Arc::new(
        SqliteAgentStore::connect(directory.path().join("concurrent.db"))
            .await
            .expect("connect SQLite agent store"),
    );
    let key = idempotency_key("concurrent-session");
    let (left, right) = tokio::join!(
        store.create_session(session_spec(), Some(&key)),
        store.create_session(session_spec(), Some(&key))
    );
    let session_id = left.expect("left idempotent create");
    assert_eq!(right.expect("right idempotent create"), session_id);

    let (left, right) = tokio::join!(
        store.create_run(run_spec(std::slice::from_ref(&session_id), None)),
        store.create_run(run_spec(std::slice::from_ref(&session_id), None))
    );
    let results = [left, right];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let error = results
        .into_iter()
        .find_map(Result::err)
        .expect("one competing claim must fail");
    assert_eq!(error.code(), ErrorCode::Conflict);
}
