use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::*;
use crate::reviewer_kernel::review_contract::{
    AgentBudget, ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, Role, SnapshotMode, ToolName,
};
use crate::workspace::RepoSnapshot;

#[derive(Default)]
struct RecordingRuntimeSink {
    records: Mutex<Vec<(RuntimeEventContext, RuntimeEvent)>>,
}

impl RuntimeEventSink for RecordingRuntimeSink {
    fn emit(&self, event: RuntimeEvent) {
        self.emit_with_context(RuntimeEventContext::from_event(&event), event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        self.records
            .lock()
            .expect("sink lock")
            .push((context, event));
    }
}

#[tokio::test]
async fn tool_batch_runner_applies_policy_denials_and_preserves_model_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("README.md"), "needle\n").expect("write readme");
    let change = test_change_with_file("README.md");
    let snapshot =
        RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64, 20), &change).expect("snapshot");
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let policy = ReviewerPolicy::new();
    let runtime_sink = Arc::new(RecordingRuntimeSink::default());
    let sink: Arc<dyn RuntimeEventSink> = runtime_sink.clone();
    let dispatcher = RuntimeEventDispatcher::new(Some(sink));
    let runner = ToolBatchRunner::new(&policy, &tools, &dispatcher);
    let scope = test_scope("tool-batch-session");

    let results = runner
        .execute(
            scope.clone(),
            TurnId(3),
            vec![
                model_call("diff", 0, ToolName::ReadDiff, "{}"),
                model_call("files", 1, ToolName::ListFiles, "{}"),
                model_call("changed", 2, ToolName::ListChangedFiles, "{}"),
            ],
            &SessionEvidence::default(),
            2,
            CancellationToken::new(),
        )
        .await;

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].tool_call_id, ToolCallId("diff".to_string()));
    assert!(results[0].ok);
    assert_eq!(results[1].tool_call_id, ToolCallId("files".to_string()));
    assert!(results[1].ok);
    assert_eq!(results[2].tool_call_id, ToolCallId("changed".to_string()));
    assert_eq!(
        results[2].error.as_ref().expect("denial").code,
        ToolErrorCode::BudgetExceeded
    );
    let records = runtime_sink.records.lock().expect("sink lock");
    assert!(records.iter().any(|(context, event)| {
        context.session_id.as_ref() == Some(&scope.id)
            && context.turn_id == Some(TurnId(3))
            && matches!(event, RuntimeEvent::ToolBatchStarted { count: 2, .. })
    }));
    let denied_rejection_trace = records
        .iter()
        .find_map(|(_, event)| match event {
            RuntimeEvent::AgentTrace {
                trace_kind,
                details,
                ..
            } if trace_kind == "tool_call_rejected" && details["callId"] == "changed" => {
                Some(details)
            }
            _ => None,
        })
        .expect("budget-denied rejection trace");
    assert_eq!(denied_rejection_trace["errorCode"], "budget_exceeded");
    assert_eq!(
        denied_rejection_trace["reason"],
        "session tool-call budget exhausted"
    );
}

#[tokio::test]
async fn tool_batch_runner_traces_rejected_invalid_tool_calls() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("README.md"), "needle\n").expect("write readme");
    let change = test_change_with_file("README.md");
    let snapshot =
        RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64, 20), &change).expect("snapshot");
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let policy = ReviewerPolicy::new();
    let runtime_sink = Arc::new(RecordingRuntimeSink::default());
    let sink: Arc<dyn RuntimeEventSink> = runtime_sink.clone();
    let dispatcher = RuntimeEventDispatcher::new(Some(sink));
    let runner = ToolBatchRunner::new(&policy, &tools, &dispatcher);
    let scope = test_scope("tool-rejection-trace");

    let results = runner
        .execute(
            scope.clone(),
            TurnId(7),
            vec![
                ModelToolCall {
                    call_id: ToolCallId("bad-json".to_string()),
                    index: 0,
                    name: ToolId::from(ToolName::ReadFile),
                    raw_arguments: "not-json".to_string(),
                },
                ModelToolCall {
                    call_id: ToolCallId("unknown-tool".to_string()),
                    index: 1,
                    name: ToolId::parse("missing_tool").unwrap(),
                    raw_arguments: "{}".to_string(),
                },
                ModelToolCall {
                    call_id: ToolCallId("bad-path-shape".to_string()),
                    index: 2,
                    name: ToolId::from(ToolName::ReadFile),
                    raw_arguments: r#"{"file":123}"#.to_string(),
                },
            ],
            &SessionEvidence::default(),
            4,
            CancellationToken::new(),
        )
        .await;

    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].error.as_ref().expect("bad json").code,
        ToolErrorCode::InvalidArgs
    );
    assert_eq!(
        results[1].error.as_ref().expect("unknown").code,
        ToolErrorCode::UnknownTool
    );
    assert_eq!(
        results[2].error.as_ref().expect("bad path shape").code,
        ToolErrorCode::InvalidArgs
    );
    let records = runtime_sink.records.lock().expect("sink lock");
    let rejection_traces = records
        .iter()
        .filter_map(|(_, event)| match event {
            RuntimeEvent::AgentTrace {
                trace_kind,
                details,
                ..
            } if trace_kind == "tool_call_rejected" => Some(details),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rejection_traces.len(), 3);
    assert!(rejection_traces.iter().any(|details| {
        details["callId"] == "bad-json"
            && details["errorCode"] == "invalid_args"
            && details["reason"] == "invalid tool invocation"
    }));
    assert!(rejection_traces.iter().any(|details| {
        details["callId"] == "unknown-tool"
            && details["errorCode"] == "unknown_tool"
            && details["reason"] == "tool is not registered"
    }));
    let rejected_path_trace = rejection_traces
        .iter()
        .find(|details| details["callId"] == "bad-path-shape")
        .expect("invalid args trace");
    assert_eq!(rejected_path_trace["errorCode"], "invalid_args");
    assert_eq!(rejected_path_trace["reason"], "invalid tool invocation");
}

#[tokio::test]
async fn tool_batch_runner_rejects_model_aliases_at_execution() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("README.md"), "first\nneedle\nthird\n").expect("write readme");
    let change = test_change_with_file("README.md");
    let snapshot =
        RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64, 20), &change).expect("snapshot");
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let policy = ReviewerPolicy::new();
    let runtime_sink = Arc::new(RecordingRuntimeSink::default());
    let sink: Arc<dyn RuntimeEventSink> = runtime_sink.clone();
    let dispatcher = RuntimeEventDispatcher::new(Some(sink));
    let runner = ToolBatchRunner::new(&policy, &tools, &dispatcher);
    let scope = test_scope("tool-alias-rejected");

    let results = runner
        .execute(
            scope,
            TurnId(9),
            vec![
                ModelToolCall {
                    call_id: ToolCallId("read-alias".to_string()),
                    index: 0,
                    name: ToolId::parse("read").unwrap(),
                    raw_arguments: r#"{"path":"README.md"}"#.to_string(),
                },
                ModelToolCall {
                    call_id: ToolCallId("grep-alias".to_string()),
                    index: 1,
                    name: ToolId::parse("grep").unwrap(),
                    raw_arguments: r#"{"query":"needle"}"#.to_string(),
                },
            ],
            &SessionEvidence::default(),
            6,
            CancellationToken::new(),
        )
        .await;

    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| {
        result
            .error
            .as_ref()
            .is_some_and(|error| error.code == ToolErrorCode::UnknownTool)
    }));
    assert_eq!(results[0].tool_name, ToolId::parse("read").unwrap());
    assert_eq!(results[1].tool_name, ToolId::parse("grep").unwrap());

    let records = runtime_sink.records.lock().expect("sink lock");
    let rejection_traces = records
        .iter()
        .filter_map(|(_, event)| match event {
            RuntimeEvent::AgentTrace {
                trace_kind,
                details,
                ..
            } if trace_kind == "tool_call_rejected" => Some(details),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rejection_traces.len(), 2);

    let read_trace = rejection_traces
        .iter()
        .find(|details| details["callId"] == "read-alias")
        .expect("read rejection trace");
    assert_eq!(read_trace["originalToolId"], "read");
    assert_eq!(read_trace["toolId"], "read");
    assert_eq!(read_trace["argumentSummary"]["parseable"], true);
    assert_eq!(read_trace["argumentSummary"]["keys"], json!(["path"]));

    let grep_trace = rejection_traces
        .iter()
        .find(|details| details["callId"] == "grep-alias")
        .expect("grep rejection trace");
    assert_eq!(grep_trace["originalToolId"], "grep");
    assert_eq!(grep_trace["toolId"], "grep");
    assert_eq!(grep_trace["argumentSummary"]["parseable"], true);
    assert_eq!(grep_trace["argumentSummary"]["keys"], json!(["query"]));
}

fn model_call(id: &str, index: usize, tool: ToolName, raw_arguments: &str) -> ModelToolCall {
    ModelToolCall {
        call_id: ToolCallId(id.to_string()),
        index,
        name: ToolId::from(tool),
        raw_arguments: raw_arguments.to_string(),
    }
}

fn test_scope(id: &str) -> SessionScope {
    SessionScope::review_read_only(
        SessionId(id.to_string()),
        Role::Generalist,
        "tool batch runner test",
        AgentBudget {
            max_turns: 4,
            max_tool_calls: 8,
            max_prompt_tokens: 32_000,
            max_output_tokens: 512,
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::PlannedDefault,
        },
    )
}

fn test_change_with_file(path: &str) -> ChangeScopeV1 {
    ChangeScopeV1 {
        kind: ChangeKind::LocalDiff,
        change_id: "test".to_string(),
        source_ref: "head".to_string(),
        target_ref: "base".to_string(),
        base_revision_id: "base".to_string(),
        head_revision_id: "head".to_string(),
        merge_base_revision_id: None,
        changed_files_manifest_ref: None,
        diff_manifest_ref: None,
        inline_diff: None,
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files: vec![ChangedFileEntryV1 {
            status: ChangedFileStatus::Modified,
            old_path: Some(PathBuf::from(path)),
            new_path: Some(PathBuf::from(path)),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        }],
    }
}
