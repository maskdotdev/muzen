use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::*;
use crate::contracts::ToolName;
use crate::runtime::contracts::{
    ArtifactId, CacheInfo, CacheStatus, LimitInfo, SnapshotId, ToolCallId, ToolId, ToolProviderId,
    ToolResultEnvelope,
};

fn user(content: &str) -> ConversationItem {
    ConversationItem::User {
        content: content.to_string(),
    }
}

fn tool_result(call: &str) -> ConversationItem {
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
            data: Some(json!({ "content": "payload" })),
            error: None,
        }),
    }
}

fn counting_render(
    counter: &AtomicUsize,
) -> impl FnMut(&ConversationItem) -> RuntimeResult<Option<Value>> + '_ {
    move |item| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(match item {
            ConversationItem::System { .. } => None,
            _ => Some(json!({ "rendered": true })),
        })
    }
}

#[test]
fn appended_items_render_only_the_suffix() {
    let cache = MessageAssemblyCache::new();
    let capabilities = CapabilitySet::review_read_only();
    let renders = AtomicUsize::new(0);
    let mut transcript = vec![user("a"), tool_result("call-1")];
    cache
        .assemble(
            "session",
            &capabilities,
            &transcript,
            counting_render(&renders),
        )
        .expect("assemble");
    assert_eq!(renders.load(Ordering::SeqCst), 2);
    transcript.push(user("b"));
    let messages = cache
        .assemble(
            "session",
            &capabilities,
            &transcript,
            counting_render(&renders),
        )
        .expect("assemble");
    assert_eq!(
        renders.load(Ordering::SeqCst),
        3,
        "only the new item renders"
    );
    assert_eq!(messages.len(), 3);
}

#[test]
fn system_items_are_excluded_but_cached() {
    let cache = MessageAssemblyCache::new();
    let capabilities = CapabilitySet::review_read_only();
    let renders = AtomicUsize::new(0);
    let transcript = vec![
        ConversationItem::System {
            content: "rules".to_string(),
        },
        user("a"),
    ];
    let messages = cache
        .assemble(
            "session",
            &capabilities,
            &transcript,
            counting_render(&renders),
        )
        .expect("assemble");
    assert_eq!(messages.len(), 1);
    cache
        .assemble(
            "session",
            &capabilities,
            &transcript,
            counting_render(&renders),
        )
        .expect("assemble");
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "second pass is fully cached"
    );
}

#[test]
fn eviction_invalidates_the_mutated_item_and_its_suffix() {
    let cache = MessageAssemblyCache::new();
    let capabilities = CapabilitySet::review_read_only();
    let renders = AtomicUsize::new(0);
    let mut transcript = vec![tool_result("call-1"), tool_result("call-2"), user("next")];
    cache
        .assemble(
            "session",
            &capabilities,
            &transcript,
            counting_render(&renders),
        )
        .expect("assemble");
    assert_eq!(renders.load(Ordering::SeqCst), 3);
    if let ConversationItem::ToolResult { content, .. } = &mut transcript[0] {
        content.data = Some(json!({ "evicted": true }));
    }
    cache
        .assemble(
            "session",
            &capabilities,
            &transcript,
            counting_render(&renders),
        )
        .expect("assemble");
    assert_eq!(
        renders.load(Ordering::SeqCst),
        6,
        "mutation at index 0 re-renders everything after it"
    );
}

#[test]
fn capability_change_resets_the_session_entry() {
    let cache = MessageAssemblyCache::new();
    let renders = AtomicUsize::new(0);
    let transcript = vec![user("a"), tool_result("call-1")];
    cache
        .assemble(
            "session",
            &CapabilitySet::review_read_only(),
            &transcript,
            counting_render(&renders),
        )
        .expect("assemble");
    let mut stripped = CapabilitySet::review_read_only();
    stripped.tool_grants.clear();
    cache
        .assemble("session", &stripped, &transcript, counting_render(&renders))
        .expect("assemble");
    assert_eq!(
        renders.load(Ordering::SeqCst),
        4,
        "capability change must re-render the full transcript"
    );
}

#[test]
fn sessions_are_isolated() {
    let cache = MessageAssemblyCache::new();
    let capabilities = CapabilitySet::review_read_only();
    let renders = AtomicUsize::new(0);
    let transcript = vec![user("a")];
    cache
        .assemble("a", &capabilities, &transcript, counting_render(&renders))
        .expect("assemble");
    cache
        .assemble("b", &capabilities, &transcript, counting_render(&renders))
        .expect("assemble");
    assert_eq!(renders.load(Ordering::SeqCst), 2);
}
