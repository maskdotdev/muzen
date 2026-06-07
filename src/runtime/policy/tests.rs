use std::collections::BTreeSet;

use serde_json::json;

use crate::contracts::{AgentBudget, Role, ToolName};
use crate::runtime::contracts::{
    ArtifactId, CacheInfo, CacheStatus, CapabilitySet, LimitInfo, ModelOutputPolicy, ModelToolCall,
    SessionId, SessionScope, SnapshotId, ToolCallId, ToolErrorCode, ToolErrorInfo, ToolId,
    ToolProviderId, ToolResultEnvelope,
};

use super::*;
#[test]
fn transcript_policy_compacts_model_visible_tool_output() {
    let result = ToolResultEnvelope {
        ok: true,
        tool_call_id: ToolCallId("read-file".to_string()),
        tool_name: ToolId::from(ToolName::ReadFile),
        provider_id: ToolProviderId::builtin_review(),
        snapshot_id: SnapshotId("snapshot".to_string()),
        artifact_id: None,
        cache: CacheInfo {
            status: CacheStatus::NotCacheable,
            key_hash: None,
        },
        limits: LimitInfo::default(),
        data: Some(json!({
            "path": "README.md",
            "content": "a".repeat(1_400),
            "evidenceId": "evidence-1",
        })),
        error: None,
    };

    let compact =
        ReviewerPolicy::new().compact_tool_result(&result, &CapabilitySet::review_read_only());
    let snippet = compact["data"]["contentSnippet"].as_str().unwrap();

    assert!(snippet.ends_with("[truncated]"));
    assert!(snippet.len() < 1_230);
}

#[test]
fn transcript_policy_hides_data_and_artifacts_when_output_policy_denies_them() {
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.model_output = ModelOutputPolicy::metadata_only();
    let result = successful_result(ToolName::ReadFile);

    let compact = ReviewerPolicy::new().compact_tool_result(&result, &capabilities);

    assert!(compact["artifactId"].is_null());
    assert!(compact["data"].is_null());
    assert_eq!(compact["limits"]["outputBytes"].as_u64(), Some(0));
}

#[test]
fn transcript_policy_truncates_data_to_model_output_capability() {
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.model_output.max_tool_data_bytes = 80;
    let result = ToolResultEnvelope {
        ok: true,
        tool_call_id: ToolCallId("read-file".to_string()),
        tool_name: ToolId::from(ToolName::ReadFile),
        provider_id: ToolProviderId::builtin_review(),
        snapshot_id: SnapshotId("snapshot".to_string()),
        artifact_id: Some(ArtifactId("artifact-read".to_string())),
        cache: CacheInfo {
            status: CacheStatus::NotCacheable,
            key_hash: None,
        },
        limits: LimitInfo::default(),
        data: Some(json!({
            "path": "README.md",
            "content": "a".repeat(1_400),
        })),
        error: None,
    };

    let compact = ReviewerPolicy::new().compact_tool_result(&result, &capabilities);

    assert_eq!(
        compact["data"]["reason"].as_str(),
        Some("model-visible tool data exceeded capability policy")
    );
    assert_eq!(compact["data"]["truncated"].as_bool(), Some(true));
}

#[test]
fn evidence_policy_blocks_terminal_tools_until_evidence_ready() {
    let policy = ReviewerPolicy::new();
    let missing_evidence = SessionEvidence::default();
    let denial = policy
        .terminal_denial_before_evidence(&ToolId::from(ToolName::RecordFinding), &missing_evidence)
        .expect("terminal denial");
    assert_eq!(denial.code, ToolErrorCode::ToolNotAllowed);
    assert_eq!(
            denial.message,
            "terminal tool requires successful read_diff, read_file/read_file_range/read_head_file, and search_text evidence first"
        );
    assert!(!denial.retryable);
    let ready_evidence = ready_evidence();
    assert!(policy
        .terminal_denial_before_evidence(&ToolId::from(ToolName::RecordFinding), &ready_evidence)
        .is_none());
    assert!(policy
        .terminal_denial_before_evidence(&ToolId::from(ToolName::ReadDiff), &missing_evidence)
        .is_none());
}

#[test]
fn evidence_policy_blocks_finish_until_small_changed_file_scope_is_read() {
    let policy = ReviewerPolicy::new();
    let mut evidence = ready_evidence();
    evidence.changed_files.insert("src/a.ts".to_string());
    evidence.changed_files.insert("src/b.ts".to_string());
    evidence.read_files.insert("src/a.ts".to_string());

    let denial = policy
        .terminal_denial_before_evidence(&ToolId::from(ToolName::Finish), &evidence)
        .expect("finish denial");

    assert_eq!(denial.code, ToolErrorCode::ToolNotAllowed);
    assert!(denial.message.contains("every listed changed file"));
    assert!(denial.retryable);
    evidence.read_files.insert("src/b.ts".to_string());
    evidence.reviewed_files.insert("src/a.ts".to_string());
    assert!(policy
        .terminal_denial_before_evidence(&ToolId::from(ToolName::Finish), &evidence)
        .is_some());
    evidence.reviewed_files.insert("src/b.ts".to_string());
    assert!(policy
        .terminal_denial_before_evidence(&ToolId::from(ToolName::Finish), &evidence)
        .is_none());
}

#[test]
fn failed_uninspectable_read_counts_for_assigned_file_coverage_but_not_review() {
    let scope = test_scope_with_changed_file_batch("src/generated.bin");
    let mut evidence = SessionEvidence::for_scope(&scope);
    evidence.observe(&successful_result(ToolName::ReadDiff));
    evidence.observe(&successful_result(ToolName::SearchText));

    let mut failed_read = failed_result(ToolName::ReadHeadFile, ToolErrorCode::NotText);
    failed_read.data = Some(json!({
        "path": "src/generated.bin",
        "available": false,
    }));
    evidence.observe(&failed_read);

    assert!(evidence.ready());
    assert!(!evidence.ready_to_finish());
    assert!(evidence.missing_read_files(8).is_empty());
    assert_eq!(
        evidence.missing_review_files(8),
        vec!["src/generated.bin".to_string()]
    );

    let mut review = successful_result(ToolName::RecordFileReview);
    review.data = Some(json!({
        "path": "src/generated.bin",
        "verdict": "skipped",
        "summary": "Could not inspect src/generated.bin because the read tool reported it is not text-readable.",
        "findingId": null,
        "relatedPaths": [],
    }));
    evidence.observe(&review);

    assert!(evidence.ready_to_finish());
}

#[test]
fn failed_read_for_related_file_does_not_satisfy_fixed_batch_coverage() {
    let scope = test_scope_with_changed_file_batch("src/assigned.rs");
    let mut evidence = SessionEvidence::for_scope(&scope);
    evidence.observe(&successful_result(ToolName::ReadDiff));
    evidence.observe(&successful_result(ToolName::SearchText));

    let mut failed_read = failed_result(ToolName::ReadHeadFile, ToolErrorCode::NotText);
    failed_read.data = Some(json!({
        "path": "src/related.bin",
        "available": false,
    }));
    evidence.observe(&failed_read);

    assert!(!evidence.ready());
    assert_eq!(
        evidence.missing_read_files(8),
        vec!["src/assigned.rs".to_string()]
    );
}

#[test]
fn evidence_policy_blocks_file_review_outside_fixed_batch_scope() {
    let policy = ReviewerPolicy::new();
    let mut evidence = ready_evidence();
    evidence.fixed_changed_file_scope = true;
    evidence.changed_files.insert("src/assigned.ts".to_string());

    let mut call = model_call("review-related", 0, ToolName::RecordFileReview);
    call.raw_arguments = json!({
        "path": "src/related.ts",
        "verdict": "clean",
        "summary": "inspected related file",
        "finding_id": "",
        "related_paths": []
    })
    .to_string();

    let plan = policy.plan_tool_batch(vec![call], &evidence, 4);

    assert!(plan.allowed_calls.is_empty());
    assert_eq!(plan.denied_calls.len(), 1);
    assert_eq!(
        plan.denied_calls[0].denial.code,
        ToolErrorCode::ToolNotAllowed
    );
    assert!(plan.denied_calls[0]
        .denial
        .message
        .contains("src/assigned.ts"));
    assert!(plan.denied_calls[0].denial.retryable);
}

#[test]
fn evidence_policy_blocks_finding_outside_fixed_batch_scope() {
    let policy = ReviewerPolicy::new();
    let mut evidence = ready_evidence();
    evidence.fixed_changed_file_scope = true;
    evidence.changed_files.insert("src/assigned.ts".to_string());

    let mut call = model_call("finding-related", 0, ToolName::RecordFinding);
    call.raw_arguments = json!({
        "title": "Related file bug",
        "claim": "The related file has a concrete bug.",
        "path": "src/related.ts",
        "start_line": 10,
        "end_line": 12
    })
    .to_string();

    let plan = policy.plan_tool_batch(vec![call], &evidence, 4);

    assert!(plan.allowed_calls.is_empty());
    assert_eq!(plan.denied_calls.len(), 1);
    assert_eq!(
        plan.denied_calls[0].denial.code,
        ToolErrorCode::ToolNotAllowed
    );
    assert!(plan.denied_calls[0]
        .denial
        .message
        .contains("src/assigned.ts"));
    assert!(plan.denied_calls[0].denial.retryable);
}

#[test]
fn evidence_policy_uses_trusted_batch_scope_for_finish_coverage() {
    let policy = ReviewerPolicy::new();
    let mut evidence = ready_evidence();
    evidence.fixed_changed_file_scope = true;
    evidence.changed_files.insert("src/a.ts".to_string());
    evidence.changed_files.insert("src/b.ts".to_string());
    evidence.saw_diff = true;
    evidence.saw_file = true;
    evidence.saw_search = true;
    evidence.read_files.insert("src/a.ts".to_string());
    evidence.read_files.insert("src/b.ts".to_string());
    evidence.reviewed_files.insert("src/a.ts".to_string());
    evidence.reviewed_files.insert("src/b.ts".to_string());
    let mut listed = successful_result(ToolName::ListChangedFiles);
    listed.data = Some(json!({
        "changedFiles": ["Modified src/a.ts", "Modified src/b.ts", "Modified src/c.ts"]
    }));

    policy.observe_evidence_result(&mut evidence, &listed);

    assert!(policy
        .terminal_denial_before_evidence(&ToolId::from(ToolName::Finish), &evidence)
        .is_none());
}

#[test]
fn evidence_policy_plans_tool_batch_denials_before_terminal_evidence() {
    let policy = ReviewerPolicy::new();
    let missing_evidence = SessionEvidence::default();
    let plan = policy.plan_tool_batch(
        vec![
            model_call("read", 0, ToolName::ReadFile),
            model_call("finding", 1, ToolName::RecordFinding),
            model_call("finish", 2, ToolName::Finish),
        ],
        &missing_evidence,
        usize::MAX,
    );

    assert_eq!(plan.scheduled_count, 3);
    assert_eq!(plan.allowed_calls.len(), 1);
    assert_eq!(plan.allowed_calls[0].name, ToolId::from(ToolName::ReadFile));
    assert_eq!(plan.denied_calls.len(), 2);
    assert_eq!(plan.denied_calls[0].index, 1);
    assert_eq!(
        plan.denied_calls[0].tool_id,
        ToolId::from(ToolName::RecordFinding)
    );
    assert_eq!(
        plan.denied_calls[0].denial.code,
        ToolErrorCode::ToolNotAllowed
    );
    assert_eq!(plan.denied_calls[1].index, 2);
    assert_eq!(plan.denied_calls[1].tool_id, ToolId::from(ToolName::Finish));

    let ready_evidence = ready_evidence();
    let ready_plan = policy.plan_tool_batch(
        vec![
            model_call("finding", 0, ToolName::RecordFinding),
            model_call("finish", 1, ToolName::Finish),
        ],
        &ready_evidence,
        usize::MAX,
    );
    assert_eq!(ready_plan.scheduled_count, 2);
    assert_eq!(ready_plan.allowed_calls.len(), 2);
    assert!(ready_plan.denied_calls.is_empty());
}

#[test]
fn batch_policy_applies_budget_before_evidence_gate() {
    let policy = ReviewerPolicy::new();
    let missing_evidence = SessionEvidence::default();
    let plan = policy.plan_tool_batch(
        vec![
            model_call("finding", 0, ToolName::RecordFinding),
            model_call("read", 1, ToolName::ReadFile),
            model_call("finish", 2, ToolName::Finish),
        ],
        &missing_evidence,
        2,
    );

    assert_eq!(plan.scheduled_count, 2);
    assert_eq!(plan.allowed_calls.len(), 1);
    assert_eq!(plan.allowed_calls[0].index, 1);
    assert_eq!(plan.denied_calls.len(), 2);
    assert_eq!(plan.denied_calls[0].index, 0);
    assert_eq!(
        plan.denied_calls[0].denial.code,
        ToolErrorCode::ToolNotAllowed
    );
    assert_eq!(plan.denied_calls[1].index, 2);
    assert_eq!(
        plan.denied_calls[1].denial.code,
        ToolErrorCode::BudgetExceeded
    );
    assert_eq!(
        plan.denied_calls[1].denial.message,
        "session tool-call budget exhausted"
    );
    assert!(!plan.denied_calls[1].denial.retryable);
}

#[test]
fn session_policy_fails_after_repeated_terminal_denials() {
    let policy = ReviewerPolicy::new();
    let mut terminal = SessionTerminal::default();
    let denied = denied_result(ToolName::RecordFinding);

    policy.observe_terminal_error(&mut terminal, &denied);
    assert!(!policy.should_fail_after_terminal_errors(&terminal));
    policy.observe_terminal_error(&mut terminal, &denied);

    assert!(policy.should_fail_after_terminal_errors(&terminal));
    assert_eq!(
        policy.session_state(false, terminal.seen(), false, true),
        "failed"
    );

    let mut retryable_terminal = SessionTerminal::default();
    let mut retryable = denied_result(ToolName::Finish);
    retryable.error.as_mut().expect("error").retryable = true;
    policy.observe_terminal_error(&mut retryable_terminal, &retryable);
    policy.observe_terminal_error(&mut retryable_terminal, &retryable);
    assert!(!policy.should_fail_after_terminal_errors(&retryable_terminal));
}

fn model_call(id: &str, index: usize, tool: ToolName) -> ModelToolCall {
    ModelToolCall {
        call_id: ToolCallId(id.to_string()),
        index,
        name: ToolId::from(tool),
        raw_arguments: "{}".to_string(),
    }
}

fn ready_evidence() -> SessionEvidence {
    SessionEvidence {
        saw_diff: true,
        saw_file: true,
        saw_search: true,
        changed_files: BTreeSet::new(),
        read_files: BTreeSet::new(),
        reviewed_files: BTreeSet::new(),
        fixed_changed_file_scope: false,
        results: Vec::new(),
    }
}

fn successful_result(tool: ToolName) -> ToolResultEnvelope {
    let tool_id = ToolId::from(tool);
    ToolResultEnvelope {
        ok: true,
        tool_call_id: ToolCallId(format!("call-{}", tool.as_str())),
        tool_name: tool_id,
        provider_id: ToolProviderId::builtin_review(),
        snapshot_id: SnapshotId("snapshot".to_string()),
        artifact_id: Some(ArtifactId(format!("artifact-{}", tool.as_str()))),
        cache: CacheInfo {
            status: CacheStatus::NotCacheable,
            key_hash: None,
        },
        limits: LimitInfo::default(),
        data: None,
        error: None,
    }
}

fn denied_result(tool: ToolName) -> ToolResultEnvelope {
    let tool_id = ToolId::from(tool);
    ToolResultEnvelope {
        ok: false,
        tool_call_id: ToolCallId(format!("denied-{}", tool.as_str())),
        tool_name: tool_id,
        provider_id: ToolProviderId::builtin_review(),
        snapshot_id: SnapshotId("snapshot".to_string()),
        artifact_id: None,
        cache: CacheInfo {
            status: CacheStatus::NotCacheable,
            key_hash: None,
        },
        limits: LimitInfo::default(),
        data: None,
        error: Some(ToolErrorInfo {
            code: ToolErrorCode::ToolNotAllowed,
            message: "denied".to_string(),
            retryable: false,
            partial: false,
        }),
    }
}

fn failed_result(tool: ToolName, code: ToolErrorCode) -> ToolResultEnvelope {
    let tool_id = ToolId::from(tool);
    ToolResultEnvelope {
        ok: false,
        tool_call_id: ToolCallId(format!("failed-{}", tool.as_str())),
        tool_name: tool_id,
        provider_id: ToolProviderId::builtin_review(),
        snapshot_id: SnapshotId("snapshot".to_string()),
        artifact_id: None,
        cache: CacheInfo {
            status: CacheStatus::NotCacheable,
            key_hash: None,
        },
        limits: LimitInfo::default(),
        data: None,
        error: Some(ToolErrorInfo {
            code,
            message: "failed".to_string(),
            retryable: false,
            partial: false,
        }),
    }
}

fn test_scope_with_changed_file_batch(path: &str) -> SessionScope {
    let mut scope = SessionScope::review_read_only(
        SessionId("batch-scope".to_string()),
        Role::Generalist,
        "policy diagnostic test",
        AgentBudget {
            max_turns: 4,
            max_tool_calls: 8,
            max_prompt_tokens: 32_000,
            max_output_tokens: 512,
        },
    );
    scope
        .instructions
        .push(crate::runtime::contracts::SessionInstruction {
            kind: "changed_file_batch".to_string(),
            text: format!("Batch 1/1 changed files:\n1. {path}"),
            trusted: true,
        });
    scope
}
