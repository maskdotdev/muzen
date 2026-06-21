use serde_json::Value;

use crate::reviewer_kernel::kernel_types::SessionInstruction;
use crate::workspace::RepoSnapshot;

use super::diff_risk::{format_diff_risk_inventory, truncate_chars};
use super::tasks::{DelegateTaskKind, DelegateTaskRequest};

const MAX_DIFF_RISK_ENTRIES: usize = 40;

pub(super) fn neutral_starter_context(
    snapshot: &RepoSnapshot,
    instructions: &[SessionInstruction],
) -> String {
    let changed_files = snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| format!("- {}", file.summary))
        .collect::<Vec<_>>()
        .join("\n");
    let diff = truncate_chars(&snapshot.diff.content, 24_000);
    let instruction_text = instructions
        .iter()
        .map(|instruction| format!("- [{}] {}", instruction.kind, instruction.text))
        .collect::<Vec<_>>()
        .join("\n");
    let risk_inventory = format_diff_risk_inventory(&snapshot.diff.content, MAX_DIFF_RISK_ENTRIES);
    format!(
        "Neutral review starter context.\n\nChanged files:\n{}\n\nDiff risk inventory:\n{}\n\nRisk inventory instructions:\n- Treat entries as review obligations, not findings.\n- For each listed risk id, inspect enough raw code or diff evidence to support a bug, refute it, or list the unresolved question.\n- Changed async, promise, lazy-loading, callback, and side-effect aggregation code needs caller adaptation, awaited ordering, value shape, and error propagation checked before returning clean.\n- Changed localized resources need locale, placeholder, markup, and copied-language parity checked before returning clean.\n- Changed nullability, slicing/indexing, type-cast/deserialization, feature-gate, identifier lookup, process-exit, and exception-boundary code needs caller/consumer impact checked before returning clean.\n- Use search_code or explore_code when independent risk entries can be investigated in parallel.\n\nRaw diff{}:\n{}\n\nProject/review instructions:\n{}",
        if changed_files.is_empty() {
            "(none)"
        } else {
            &changed_files
        },
        risk_inventory,
        if snapshot.diff.content.chars().count() > 24_000 {
            " (truncated; use diff/read tools for omitted hunks)"
        } else {
            ""
        },
        diff,
        if instruction_text.is_empty() {
            "(none)"
        } else {
            &instruction_text
        }
    )
}

pub(super) fn child_task_packet(
    kind: DelegateTaskKind,
    request: &DelegateTaskRequest,
    snapshot: &RepoSnapshot,
) -> String {
    let changed_files = snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| format!("- {}", file.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Task type: {}\nObjective: {}\nPrompt: {}\nCandidate: {}\n\nChanged files:\n{}",
        kind.tool_name(),
        request.objective,
        request.prompt,
        request
            .candidate
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_else(|| "none".to_string()),
        if changed_files.is_empty() {
            "(none)"
        } else {
            &changed_files
        }
    )
}
