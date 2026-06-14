//! Incremental provider-message assembly.
//!
//! Transcripts are append-only within a session (the one exception is prompt
//! budget eviction, which rewrites old tool-result payloads), but the model
//! clients used to re-render every conversation item into provider JSON on
//! every turn — re-compacting and re-serializing each tool result transcript
//! repeatedly. This cache keeps the rendered message per item and re-renders
//! only the suffix that changed since the previous turn.
//!
//! Correctness is guarded two ways:
//! - a per-item fingerprint covers the fields that influence rendering, so
//!   budget eviction (which replaces a tool result's data payload) invalidates
//!   that item and everything after it;
//! - a per-session scope fingerprint covers the capability set, so the
//!   text-only final turn (capabilities stripped) re-renders from scratch
//!   instead of reusing messages compacted under the old capabilities.
//!
//! Clients are constructed per run, so entries live at most for one run and
//! the map is bounded by that run's session count.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use serde_json::Value;

use crate::runtime::contracts::{CapabilitySet, ConversationItem, RuntimeResult};

#[derive(Default)]
pub(crate) struct MessageAssemblyCache {
    sessions: Mutex<HashMap<String, SessionAssembly>>,
}

impl std::fmt::Debug for MessageAssemblyCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MessageAssemblyCache").finish()
    }
}

struct SessionAssembly {
    scope_fingerprint: u64,
    fingerprints: Vec<u64>,
    /// One slot per transcript item; `None` when the protocol renders the
    /// item outside the message list (e.g. Anthropic system blocks).
    rendered: Vec<Option<Value>>,
}

impl MessageAssemblyCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Renders the transcript into provider messages, reusing the rendered
    /// values of the longest unchanged prefix. `render` returns `None` for
    /// items the protocol excludes from the message list.
    pub(crate) fn assemble(
        &self,
        session_id: &str,
        capabilities: &CapabilitySet,
        transcript: &[ConversationItem],
        mut render: impl FnMut(&ConversationItem) -> RuntimeResult<Option<Value>>,
    ) -> RuntimeResult<Vec<Value>> {
        let scope_fingerprint = capabilities_fingerprint(capabilities);
        let fingerprints: Vec<u64> = transcript.iter().map(item_fingerprint).collect();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionAssembly {
                scope_fingerprint,
                fingerprints: Vec::new(),
                rendered: Vec::new(),
            });
        if entry.scope_fingerprint != scope_fingerprint {
            entry.scope_fingerprint = scope_fingerprint;
            entry.fingerprints.clear();
            entry.rendered.clear();
        }
        let mut prefix = 0usize;
        let reusable = entry.fingerprints.len().min(fingerprints.len());
        while prefix < reusable && entry.fingerprints[prefix] == fingerprints[prefix] {
            prefix += 1;
        }
        entry.fingerprints.truncate(prefix);
        entry.rendered.truncate(prefix);
        for (item, fingerprint) in transcript[prefix..].iter().zip(&fingerprints[prefix..]) {
            let value = render(item)?;
            entry.fingerprints.push(*fingerprint);
            entry.rendered.push(value);
        }
        Ok(entry.rendered.iter().flatten().cloned().collect())
    }
}

fn capabilities_fingerprint(capabilities: &CapabilitySet) -> u64 {
    let mut hasher = DefaultHasher::new();
    for tool_id in capabilities.tool_grants.keys() {
        tool_id.as_str().hash(&mut hasher);
    }
    capabilities.tool_grants.len().hash(&mut hasher);
    hasher.finish()
}

/// Hashes every field of an item that can influence its rendered message.
/// Tool-result data payloads are deliberately not serialized here; the
/// envelope fields below (ok, ids, cache status, limits, eviction flag)
/// change whenever the payload is rewritten by the only in-place mutation in
/// the runtime, prompt-budget eviction.
fn item_fingerprint(item: &ConversationItem) -> u64 {
    let mut hasher = DefaultHasher::new();
    match item {
        ConversationItem::System { content } => {
            0u8.hash(&mut hasher);
            content.hash(&mut hasher);
        }
        ConversationItem::User { content } => {
            1u8.hash(&mut hasher);
            content.hash(&mut hasher);
        }
        ConversationItem::AssistantText { content } => {
            2u8.hash(&mut hasher);
            content.hash(&mut hasher);
        }
        ConversationItem::AssistantToolCalls { calls } => {
            3u8.hash(&mut hasher);
            for call in calls {
                call.call_id.0.hash(&mut hasher);
                call.name.as_str().hash(&mut hasher);
                call.raw_arguments.hash(&mut hasher);
            }
        }
        ConversationItem::ToolResult {
            call_id,
            name,
            content,
        } => {
            4u8.hash(&mut hasher);
            call_id.0.hash(&mut hasher);
            name.as_str().hash(&mut hasher);
            content.ok.hash(&mut hasher);
            std::mem::discriminant(&content.cache.status).hash(&mut hasher);
            content.limits.truncated.hash(&mut hasher);
            if let Some(artifact_id) = &content.artifact_id {
                artifact_id.0.hash(&mut hasher);
            }
            content.error.is_some().hash(&mut hasher);
            let evicted = content
                .data
                .as_ref()
                .and_then(|data| data.get("evicted"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            evicted.hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests;
