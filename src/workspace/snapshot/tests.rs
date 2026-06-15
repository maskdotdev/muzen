use super::*;
use crate::reviewer_kernel::review_contract::{
    ChangeKind, ChangedFileEntryV1, RenameDetection, SnapshotMode,
};

#[test]
fn build_diff_prefers_inline_change_diff() {
    let change = ChangeScopeV1 {
        kind: ChangeKind::LocalDiff,
        change_id: "inline-diff-change".to_string(),
        source_ref: "head".to_string(),
        target_ref: "base".to_string(),
        base_revision_id: "base".to_string(),
        head_revision_id: "head".to_string(),
        merge_base_revision_id: None,
        changed_files_manifest_ref: None,
        diff_manifest_ref: None,
        inline_diff: Some("diff --git a/src/lib.rs b/src/lib.rs\n".to_string()),
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files: vec![ChangedFileEntryV1 {
            status: ChangedFileStatus::Modified,
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        }],
    };

    let diff = build_diff(&change);

    assert_eq!(diff.content, "diff --git a/src/lib.rs b/src/lib.rs\n");
    assert!(!diff.content.contains("inline-diff-change base..head"));
}
