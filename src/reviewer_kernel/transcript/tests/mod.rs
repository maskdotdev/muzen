use super::*;
use crate::reviewer_kernel::kernel_types::{
    ArtifactId, CacheInfo, CacheStatus, LimitInfo, SnapshotId, ToolCallId, ToolId, ToolProviderId,
};
use crate::reviewer_kernel::review_contract::ToolName;

fn tool_result(call: &str, payload_chars: usize) -> ConversationItem {
    tool_result_with_data(call, json!({ "content": "x".repeat(payload_chars) }))
}

fn tool_result_with_data(call: &str, data: serde_json::Value) -> ConversationItem {
    ConversationItem::ToolResult {
        call_id: ToolCallId(call.to_string()),
        name: ToolId::from(ToolName::ReadFile),
        content: Box::new(ToolResultEnvelope {
            ok: true,
            tool_call_id: ToolCallId(call.to_string()),
            tool_name: ToolId::from(ToolName::ReadFile),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: SnapshotId("snap".to_string()),
            artifact_id: Some(ArtifactId(format!("artifact-{call}"))),
            cache: CacheInfo {
                status: CacheStatus::Miss,
                key_hash: None,
            },
            limits: LimitInfo::default(),
            data: Some(data),
            error: None,
        }),
    }
}

fn payload_is_evicted(item: &ConversationItem) -> bool {
    match item {
        ConversationItem::ToolResult { content, .. } => is_evicted(content),
        _ => false,
    }
}

fn budget_transcript() -> Vec<ConversationItem> {
    vec![
        ConversationItem::System {
            content: "system prompt".to_string(),
        },
        ConversationItem::User {
            content: "objective".to_string(),
        },
        tool_result("call-0", 4_000),
        tool_result("call-1", 4_000),
        tool_result("call-2", 4_000),
        tool_result("call-3", 4_000),
    ]
}

#[test]
fn under_budget_transcripts_stay_untouched() {
    let mut transcript = budget_transcript();
    let before = serde_json::to_string(&transcript).expect("serialize");
    assert_eq!(enforce_prompt_budget(&mut transcript, 1_000_000), None);
    assert_eq!(enforce_prompt_budget(&mut transcript, 0), None);
    let after = serde_json::to_string(&transcript).expect("serialize");
    assert_eq!(before, after, "no-op enforcement must keep bytes stable");
}

#[test]
fn over_budget_evicts_oldest_tool_results_first() {
    let mut transcript = budget_transcript();
    // Fits roughly two payloads: forces eviction of the two oldest.
    let compaction = enforce_prompt_budget(&mut transcript, 2_300).expect("compaction");
    assert_eq!(compaction.evicted.tool_results, 2);
    assert_eq!(compaction.evicted.model_text, 0);
    assert_eq!(compaction.evicted.artifact_refs, 2);
    assert_eq!(compaction.transcript_items_before, 6);
    assert_eq!(compaction.transcript_items_after, 6);
    assert!(compaction.token_estimate_before > compaction.token_estimate_after);
    assert!(payload_is_evicted(&transcript[2]));
    assert!(payload_is_evicted(&transcript[3]));
    assert!(!payload_is_evicted(&transcript[4]));
    assert!(!payload_is_evicted(&transcript[5]));
    assert!(estimate_prompt_tokens(&transcript) <= 2_300);
    // System and objective survive verbatim.
    assert!(matches!(
        &transcript[0],
        ConversationItem::System { content } if content == "system prompt"
    ));
    assert!(matches!(
        &transcript[1],
        ConversationItem::User { content } if content == "objective"
    ));
}

#[test]
fn recent_tool_results_survive_even_when_budget_is_tiny() {
    let mut transcript = budget_transcript();
    let evicted = enforce_prompt_budget(&mut transcript, 1).expect("compaction");
    assert_eq!(
        evicted.evicted.tool_results, 2,
        "only the unprotected prefix of tool results may be evicted"
    );
    assert!(!payload_is_evicted(&transcript[4]));
    assert!(!payload_is_evicted(&transcript[5]));
}

#[test]
fn eviction_keeps_artifact_reference_for_recovery() {
    let mut transcript = budget_transcript();
    enforce_prompt_budget(&mut transcript, 2_300);
    let ConversationItem::ToolResult { content, .. } = &transcript[2] else {
        panic!("expected tool result");
    };
    let data = content.data.as_ref().expect("data");
    assert_eq!(data["artifactId"], "artifact-call-0");
    assert_eq!(
        content.artifact_id.as_ref().expect("artifact").0,
        "artifact-call-0"
    );
}

#[test]
fn repeated_enforcement_is_idempotent() {
    let mut transcript = budget_transcript();
    let first = enforce_prompt_budget(&mut transcript, 2_300);
    let snapshot = serde_json::to_string(&transcript).expect("serialize");
    let second = enforce_prompt_budget(&mut transcript, 2_300);
    assert!(first.is_some());
    assert_eq!(second, None);
    assert_eq!(
        snapshot,
        serde_json::to_string(&transcript).expect("serialize")
    );
}

#[test]
fn compaction_counts_candidate_evidence_payloads() {
    let mut transcript = vec![
        ConversationItem::System {
            content: "system prompt".to_string(),
        },
        tool_result_with_data(
            "call-0",
            json!({ "candidateCount": 1, "content": "x".repeat(4_000) }),
        ),
        tool_result("call-1", 4_000),
        tool_result("call-2", 4_000),
        tool_result("call-3", 4_000),
    ];

    let compaction = enforce_prompt_budget(&mut transcript, 2_300).expect("compaction");

    assert_eq!(compaction.evicted.tool_results, 2);
    assert_eq!(compaction.evicted.candidate_evidence, 1);
    assert_eq!(compaction.evicted.artifact_refs, 2);
}
