//! Mid-loop prompt budget enforcement.
//!
//! Transcripts are append-only while they fit `AgentBudget::max_prompt_tokens`,
//! which keeps provider prompt caching effective. When a transcript outgrows
//! the budget, the oldest tool-result payloads are evicted first: the system
//! prompt, objective instructions, assistant turns, and the most recent tool
//! results stay intact so the session keeps its identity and its freshest
//! evidence. Eviction rewrites earlier messages and therefore invalidates any
//! cached prefix from that point — the tradeoff is deliberate, since the
//! alternative is a request the provider rejects outright.

use serde_json::{json, Value};

use crate::runtime::contracts::{ConversationItem, ToolResultEnvelope};

/// Crude chars-per-token ratio; good enough for budget enforcement, which
/// only needs to catch runaway transcripts, not bill-accurate counts.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
/// Fixed per-message overhead in tokens (role framing, ids).
const PER_MESSAGE_OVERHEAD_TOKENS: usize = 4;
/// Serialized envelope framing around a tool result's data payload.
const TOOL_RESULT_FRAMING_CHARS: usize = 160;
/// The newest tool results are never evicted; the model usually needs its
/// latest evidence to produce the next turn.
const PROTECTED_RECENT_TOOL_RESULTS: usize = 2;

pub(crate) fn estimate_prompt_tokens(transcript: &[ConversationItem]) -> usize {
    transcript.iter().map(estimate_item_tokens).sum()
}

fn estimate_item_tokens(item: &ConversationItem) -> usize {
    let chars = match item {
        ConversationItem::System { content }
        | ConversationItem::User { content }
        | ConversationItem::AssistantText { content } => content.len(),
        ConversationItem::AssistantToolCalls { calls } => calls
            .iter()
            .map(|call| call.raw_arguments.len() + call.name.as_str().len() + 24)
            .sum(),
        ConversationItem::ToolResult { content, .. } => tool_result_chars(content),
    };
    chars / CHARS_PER_TOKEN_ESTIMATE + PER_MESSAGE_OVERHEAD_TOKENS
}

fn tool_result_chars(envelope: &ToolResultEnvelope) -> usize {
    envelope
        .data
        .as_ref()
        .and_then(|data| serde_json::to_string(data).ok())
        .map(|serialized| serialized.len())
        .unwrap_or(0)
        + TOOL_RESULT_FRAMING_CHARS
}

fn is_evicted(envelope: &ToolResultEnvelope) -> bool {
    envelope
        .data
        .as_ref()
        .and_then(|data| data.get("evicted"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Evicts oldest tool-result payloads until the transcript fits the budget.
/// Returns how many results were evicted. A `max_prompt_tokens` of zero is
/// treated as unbounded.
pub(crate) fn enforce_prompt_budget(
    transcript: &mut [ConversationItem],
    max_prompt_tokens: u64,
) -> usize {
    let max_prompt_tokens = usize::try_from(max_prompt_tokens).unwrap_or(usize::MAX);
    if max_prompt_tokens == 0 || estimate_prompt_tokens(transcript) <= max_prompt_tokens {
        return 0;
    }
    let tool_result_indices: Vec<usize> = transcript
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, ConversationItem::ToolResult { .. }))
        .map(|(index, _)| index)
        .collect();
    let evictable = tool_result_indices
        .len()
        .saturating_sub(PROTECTED_RECENT_TOOL_RESULTS);
    let mut evicted = 0usize;
    for &index in tool_result_indices.iter().take(evictable) {
        if estimate_prompt_tokens(transcript) <= max_prompt_tokens {
            break;
        }
        let ConversationItem::ToolResult { content, .. } = &mut transcript[index] else {
            continue;
        };
        if is_evicted(content) {
            continue;
        }
        content.data = Some(json!({
            "evicted": true,
            "note": "Tool result evicted to fit the prompt budget. Re-run the tool or fetch the retained artifact if this evidence is still needed.",
            "artifactId": content.artifact_id.as_ref().map(|id| id.0.clone()),
        }));
        evicted += 1;
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::ToolName;
    use crate::runtime::contracts::{
        ArtifactId, CacheInfo, CacheStatus, LimitInfo, SnapshotId, ToolCallId, ToolId,
        ToolProviderId,
    };

    fn tool_result(call: &str, payload_chars: usize) -> ConversationItem {
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
                data: Some(json!({ "content": "x".repeat(payload_chars) })),
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
        assert_eq!(enforce_prompt_budget(&mut transcript, 1_000_000), 0);
        assert_eq!(enforce_prompt_budget(&mut transcript, 0), 0);
        let after = serde_json::to_string(&transcript).expect("serialize");
        assert_eq!(before, after, "no-op enforcement must keep bytes stable");
    }

    #[test]
    fn over_budget_evicts_oldest_tool_results_first() {
        let mut transcript = budget_transcript();
        // Fits roughly two payloads: forces eviction of the two oldest.
        let evicted = enforce_prompt_budget(&mut transcript, 2_300);
        assert_eq!(evicted, 2);
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
        let evicted = enforce_prompt_budget(&mut transcript, 1);
        assert_eq!(
            evicted, 2,
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
        assert!(first > 0);
        assert_eq!(second, 0);
        assert_eq!(
            snapshot,
            serde_json::to_string(&transcript).expect("serialize")
        );
    }
}
