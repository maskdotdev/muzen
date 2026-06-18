use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::*;
use crate::reviewer_kernel::kernel_types::{
    CacheInfo, CacheStatus, CapabilitySet, LimitInfo, RuntimeEvent, RuntimeEventContext,
    RuntimeEventSink, RuntimeLimits, SessionId, SnapshotId, ToolCallId, ToolErrorCode, ToolId,
    ToolProviderId,
};
use crate::reviewer_kernel::review_contract::ToolName;
use crate::reviewer_kernel::review_contract::{
    AgentBudget, ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, Role, SnapshotMode,
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

#[test]
fn processor_applies_tool_result_side_effects_in_batch_order() {
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
    let processor =
        ToolResultEffectProcessor::new(&policy, &tools, &dispatcher, &change.head_revision_id);
    let scope = test_scope("effects-session");
    let mut evidence = SessionEvidence::default();
    let mut tool_counts = ToolCounts::default();
    let mut transcript = Vec::new();

    let outcome = processor.apply_batch(
        &scope,
        TurnId(2),
        vec![successful_search_result(&snapshot.snapshot_id)],
        ToolResultBatchState {
            evidence: &mut evidence,
            tool_counts: &mut tool_counts,
            transcript: &mut transcript,
        },
    );

    assert!(!outcome.terminal_seen);
    assert!(evidence.saw_search());
    assert_eq!(tool_counts.search_text, 1);
    assert_eq!(transcript.len(), 1);
    assert!(matches!(
        transcript[0],
        ConversationItem::ToolResult { ref call_id, .. } if call_id.0 == "search"
    ));

    let records = runtime_sink.records.lock().expect("sink lock");
    assert_eq!(records.len(), 2);
    assert!(matches!(
        records[0].1,
        RuntimeEvent::ToolCallCompleted { ref call_id, ok: true, .. }
            if call_id.0 == "search"
    ));
    assert!(matches!(
        records[1].1,
        RuntimeEvent::SearchBatchCompleted {
            searched_files: 3,
            ..
        }
    ));
}

#[test]
fn processor_emits_host_visible_denied_tool_events() {
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
    let processor =
        ToolResultEffectProcessor::new(&policy, &tools, &dispatcher, &change.head_revision_id);
    let scope = test_scope("effects-denial-session");
    let mut evidence = SessionEvidence::default();
    let mut tool_counts = ToolCounts::default();
    let mut transcript = Vec::new();

    let denied = tools.error_result(
        ToolCallId("denied-read".to_string()),
        ToolId::from(ToolName::ReadFile),
        ToolErrorCode::PathDenied,
        "path is outside the review snapshot",
        false,
    );

    let outcome = processor.apply_batch(
        &scope,
        TurnId(4),
        vec![denied],
        ToolResultBatchState {
            evidence: &mut evidence,
            tool_counts: &mut tool_counts,
            transcript: &mut transcript,
        },
    );

    assert!(!outcome.terminal_seen);
    assert_eq!(transcript.len(), 1);
    let records = runtime_sink.records.lock().expect("sink lock");
    assert!(records.iter().any(|(context, event)| {
        context.session_id.as_ref() == Some(&scope.id)
            && context.turn_id == Some(TurnId(4))
            && matches!(
                event,
                RuntimeEvent::ToolCallDenied {
                    call_id,
                    error_code: ToolErrorCode::PathDenied,
                    ..
                } if call_id.0 == "denied-read"
            )
    }));
    assert!(records.iter().any(|(_, event)| {
        matches!(
            event,
            RuntimeEvent::ToolCallCompleted {
                call_id,
                ok: false,
                error_code: Some(ToolErrorCode::PathDenied),
                ..
            } if call_id.0 == "denied-read"
        )
    }));
}

fn successful_search_result(snapshot_id: &SnapshotId) -> ToolResultEnvelope {
    ToolResultEnvelope {
        ok: true,
        tool_call_id: ToolCallId("search".to_string()),
        tool_name: ToolId::from(ToolName::SearchText),
        provider_id: ToolProviderId::builtin_review(),
        snapshot_id: snapshot_id.clone(),
        artifact_id: None,
        cache: CacheInfo {
            status: CacheStatus::NotCacheable,
            key_hash: None,
        },
        limits: LimitInfo {
            searched_files: 3,
            skipped_files: 1,
            bytes_scanned: 128,
            ..LimitInfo::default()
        },
        data: None,
        error: None,
    }
}

fn test_scope(id: &str) -> SessionScope {
    SessionScope {
        id: SessionId(id.to_string()),
        role: Role::Generalist,
        objective: "effects test".to_string(),
        instructions: Vec::new(),
        snapshot_id: None,
        model_profile_id: Some("test-model".to_string()),
        response_format: None,
        capabilities: CapabilitySet::review_read_only(),
        budget: AgentBudget {
            max_turns: 4,
            max_tool_calls: 8,
            max_prompt_tokens: 32_000,
            max_output_tokens: 512,
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::PlannedDefault,
        },
    }
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
