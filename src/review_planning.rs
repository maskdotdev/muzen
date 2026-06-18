#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use crate::review_sources::ReviewSource;
use crate::reviewer_kernel::kernel_types::SessionInstruction;
use crate::reviewer_kernel::review_contract::{AgentBudget, Role};
use crate::reviewer_kernel::snapshots::{
    ChangeKind, ChangeSpec, ChangedFileSpec, ChangedFileStatus, RenameDetection, SnapshotMode,
};
use crate::reviewer_kernel::spec::ReviewSessionSpec;

const LARGE_REVIEW_BATCH_THRESHOLD: usize = 8;
const LARGE_REVIEW_DEFAULT_MAX_ACTIVE_SESSIONS: usize = 8;

pub(crate) struct ReviewChangeDescriptor<'a> {
    pub(crate) kind: &'a str,
    pub(crate) base_revision: Option<&'a str>,
    pub(crate) start_revision: Option<&'a str>,
    pub(crate) head_revision: Option<&'a str>,
    pub(crate) changed_files: Vec<ReviewChangedFileDescriptor<'a>>,
    pub(crate) diff: Option<&'a str>,
    pub(crate) review_target: Option<&'a str>,
}

pub(crate) struct ReviewChangedFileDescriptor<'a> {
    pub(crate) path: &'a str,
    pub(crate) status: Option<&'a str>,
}

pub(crate) fn default_max_active_sessions(
    requested_session_count: usize,
    changed_file_count: usize,
    explicit: Option<usize>,
) -> usize {
    if let Some(explicit) = explicit {
        return explicit.max(1);
    }
    if changed_file_count > LARGE_REVIEW_BATCH_THRESHOLD {
        return LARGE_REVIEW_DEFAULT_MAX_ACTIVE_SESSIONS;
    }
    if requested_session_count == 0 {
        return 4;
    }
    requested_session_count.max(1)
}

pub(crate) fn default_review_orchestrator_session(
    instructions: Vec<SessionInstruction>,
) -> ReviewSessionSpec {
    let spec = ReviewSessionSpec::review_read_only(
        "review-orchestrator",
        Role::Generalist,
        "Autonomously review the changed code.",
        AgentBudget::planned_baseline(),
    );
    if instructions.is_empty() {
        spec
    } else {
        spec.with_instructions(instructions)
    }
}

pub(crate) fn session_instruction(
    kind: impl Into<String>,
    text: impl Into<String>,
    trusted: bool,
) -> SessionInstruction {
    SessionInstruction {
        text: text.into(),
        trusted,
        kind: kind.into(),
    }
}

pub(crate) fn changed_file_paths(change: Option<&ReviewChangeDescriptor<'_>>) -> Vec<String> {
    change
        .map(|change| {
            change
                .changed_files
                .iter()
                .map(|file| file.path.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn changed_file_specs(
    fallback_changed_files: &[String],
    change: Option<&ReviewChangeDescriptor<'_>>,
) -> Vec<ChangedFileSpec> {
    if let Some(change) = change.filter(|change| !change.changed_files.is_empty()) {
        return change
            .changed_files
            .iter()
            .map(|file| changed_file_spec(file.path, file.status))
            .collect();
    }

    fallback_changed_files
        .iter()
        .map(|path| changed_file_spec(path, None))
        .collect()
}

pub(crate) fn review_change_spec(
    source: Option<&ReviewSource>,
    change: Option<&ReviewChangeDescriptor<'_>>,
    changed_files: Vec<ChangedFileSpec>,
    materialized_inline_diff: Option<&str>,
    run_id: &str,
) -> ChangeSpec {
    let Some(change) = change else {
        let mut spec = ChangeSpec::local("sdk-run", "head", changed_files);
        spec.inline_diff = materialized_inline_diff.map(ToOwned::to_owned);
        return spec;
    };
    ChangeSpec {
        kind: review_change_kind(source),
        change_id: non_empty(change.review_target)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{}:{run_id}", change.kind)),
        source_ref: non_empty(change.head_revision)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "head".to_string()),
        target_ref: non_empty(change.base_revision)
            .or_else(|| non_empty(change.start_revision))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "base".to_string()),
        base_revision_id: non_empty(change.base_revision)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "base".to_string()),
        head_revision_id: non_empty(change.head_revision)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "head".to_string()),
        merge_base_revision_id: non_empty(change.start_revision).map(ToOwned::to_owned),
        inline_diff: non_empty(change.diff)
            .map(ToOwned::to_owned)
            .or_else(|| materialized_inline_diff.map(ToOwned::to_owned)),
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files,
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn review_change_kind(source: Option<&ReviewSource>) -> ChangeKind {
    match source {
        Some(ReviewSource::GithubPullRequest { .. }) => ChangeKind::PullRequest,
        Some(ReviewSource::GitlabMergeRequest { .. }) => ChangeKind::MergeRequest,
        _ => ChangeKind::LocalDiff,
    }
}

fn changed_file_spec(path: &str, status: Option<&str>) -> ChangedFileSpec {
    let status = review_changed_file_status(status);
    let path = PathBuf::from(path);
    ChangedFileSpec {
        old_path: if status == ChangedFileStatus::Added {
            None
        } else {
            Some(path.clone())
        },
        new_path: if status == ChangedFileStatus::Deleted {
            None
        } else {
            Some(path)
        },
        status,
        old_content_hash: None,
        new_content_hash: None,
        is_binary: false,
        is_generated: false,
    }
}

fn review_changed_file_status(status: Option<&str>) -> ChangedFileStatus {
    match status.map(|status| status.to_ascii_lowercase()) {
        Some(status) if matches!(status.as_str(), "added" | "add" | "a") => {
            ChangedFileStatus::Added
        }
        Some(status) if matches!(status.as_str(), "deleted" | "delete" | "removed" | "d") => {
            ChangedFileStatus::Deleted
        }
        Some(status) if matches!(status.as_str(), "renamed" | "rename" | "r") => {
            ChangedFileStatus::Renamed
        }
        Some(status) if matches!(status.as_str(), "copied" | "copy" | "c") => {
            ChangedFileStatus::Copied
        }
        Some(status) if matches!(status.as_str(), "type_changed" | "typechanged" | "t") => {
            ChangedFileStatus::TypeChanged
        }
        _ => ChangedFileStatus::Modified,
    }
}

#[cfg(test)]
pub(crate) fn select_target_path(
    repo_root: &Path,
    changed_files: &[String],
) -> anyhow::Result<String> {
    for path in changed_files {
        if repo_root.join(path).is_file() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!("run requires at least one changed file that exists in the materialized worktree")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_large_reviews_to_eight_active_sessions() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, None),
            8
        );
    }

    #[test]
    fn keeps_small_review_default_session_parallelism() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD, None),
            2
        );
        assert_eq!(default_max_active_sessions(0, 1, None), 4);
    }

    #[test]
    fn explicit_max_active_sessions_overrides_large_review_default() {
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, Some(3)),
            3
        );
        assert_eq!(
            default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, Some(0)),
            1
        );
    }

    #[test]
    fn change_changed_files_are_authoritative_and_preserve_deleted_paths() {
        let change = ReviewChangeDescriptor {
            kind: "revision_range",
            base_revision: Some("base"),
            start_revision: None,
            head_revision: Some("head"),
            changed_files: vec![ReviewChangedFileDescriptor {
                path: "src/removed.rs",
                status: Some("deleted"),
            }],
            diff: None,
            review_target: None,
        };

        let files = changed_file_specs(&["src/fallback.rs".to_string()], Some(&change));

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, ChangedFileStatus::Deleted);
        assert_eq!(
            files[0].old_path.as_deref(),
            Some(Path::new("src/removed.rs"))
        );
        assert_eq!(files[0].new_path, None);
    }

    #[test]
    fn fallback_changed_files_are_used_only_without_typed_change_files() {
        let change = ReviewChangeDescriptor {
            kind: "revision_range",
            base_revision: Some("base"),
            start_revision: None,
            head_revision: Some("head"),
            changed_files: Vec::new(),
            diff: None,
            review_target: None,
        };

        let files = changed_file_specs(&["src/fallback.rs".to_string()], Some(&change));

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, ChangedFileStatus::Modified);
        assert_eq!(
            files[0].new_path.as_deref(),
            Some(Path::new("src/fallback.rs"))
        );
    }
}
