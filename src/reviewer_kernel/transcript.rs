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

use crate::reviewer_kernel::kernel_types::{ConversationItem, ToolResultEnvelope};

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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptCompaction {
    pub(crate) token_estimate_before: usize,
    pub(crate) token_estimate_after: usize,
    pub(crate) transcript_items_before: usize,
    pub(crate) transcript_items_after: usize,
    pub(crate) evicted: EvictedItemCounts,
}

impl TranscriptCompaction {
    pub(crate) fn evicted_total(&self) -> usize {
        self.evicted.tool_results
            + self.evicted.model_text
            + self.evicted.artifact_refs
            + self.evicted.candidate_evidence
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct EvictedItemCounts {
    pub(crate) tool_results: usize,
    pub(crate) model_text: usize,
    pub(crate) artifact_refs: usize,
    pub(crate) candidate_evidence: usize,
}

pub(crate) fn estimate_prompt_tokens(transcript: &[ConversationItem]) -> usize {
    transcript.iter().map(estimate_item_tokens).sum()
}

pub(crate) fn estimate_transcript_bytes(transcript: &[ConversationItem]) -> usize {
    serde_json::to_vec(transcript)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
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
/// Returns compaction diagnostics when anything was evicted. A
/// `max_prompt_tokens` of zero is treated as unbounded.
pub(crate) fn enforce_prompt_budget(
    transcript: &mut [ConversationItem],
    max_prompt_tokens: u64,
) -> Option<TranscriptCompaction> {
    let max_prompt_tokens = usize::try_from(max_prompt_tokens).unwrap_or(usize::MAX);
    let token_estimate_before = estimate_prompt_tokens(transcript);
    if max_prompt_tokens == 0 || token_estimate_before <= max_prompt_tokens {
        return None;
    }
    let transcript_items_before = transcript.len();
    let tool_result_indices: Vec<usize> = transcript
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, ConversationItem::ToolResult { .. }))
        .map(|(index, _)| index)
        .collect();
    let evictable = tool_result_indices
        .len()
        .saturating_sub(PROTECTED_RECENT_TOOL_RESULTS);
    let mut evicted = EvictedItemCounts::default();
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
        evicted.tool_results += 1;
        if content.artifact_id.is_some() {
            evicted.artifact_refs += 1;
        }
        if carries_candidate_evidence(content) {
            evicted.candidate_evidence += 1;
        }
        content.data = Some(json!({
            "evicted": true,
            "note": "Tool result evicted to fit the prompt budget. Re-run the tool or fetch the retained artifact if this evidence is still needed.",
            "artifactId": content.artifact_id.as_ref().map(|id| id.0.clone()),
        }));
    }
    if evicted.tool_results == 0 {
        return None;
    }
    Some(TranscriptCompaction {
        token_estimate_before,
        token_estimate_after: estimate_prompt_tokens(transcript),
        transcript_items_before,
        transcript_items_after: transcript.len(),
        evicted,
    })
}

fn carries_candidate_evidence(envelope: &ToolResultEnvelope) -> bool {
    let Some(data) = envelope.data.as_ref() else {
        return false;
    };
    data.get("candidateCount")
        .and_then(Value::as_u64)
        .is_some_and(|count| count > 0)
        || data
            .get("candidateFindings")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
}

#[cfg(test)]
mod tests;
