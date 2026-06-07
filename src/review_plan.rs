use std::path::Path;

use crate::contracts::ChangedFileStatus;
use crate::runtime::contracts::{RepoPath, SnapshotCaptureStatus, SnapshotId};
use crate::runtime::repo::{ChangedFileMeta, FileMeta, RepoSnapshot};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct ReviewPlan {
    pub(crate) snapshot_id: SnapshotId,
    pub(crate) counts: ReviewPlanCounts,
    pub(crate) files: Vec<PlannedReviewFile>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct ReviewPlanCounts {
    pub(crate) total_files: usize,
    pub(crate) excluded_files: usize,
    pub(crate) full_files: usize,
    pub(crate) execution_eligible_files: usize,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct PlannedReviewFile {
    pub(crate) file_id: String,
    pub(crate) path: RepoPath,
    pub(crate) status: ChangedFileStatus,
    pub(crate) content_state: PlannedFileContentState,
    pub(crate) estimated_bytes: Option<u64>,
    pub(crate) mode: ReviewPlanFileMode,
    pub(crate) score: u8,
    pub(crate) reasons: Vec<ReviewPlanReason>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum PlannedFileContentState {
    Available,
    Deleted,
    Binary,
    Unavailable,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum ReviewPlanFileMode {
    Full,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewPlanReason {
    pub(crate) code: &'static str,
    pub(crate) detail: String,
}

pub(crate) fn build_review_plan(snapshot: &RepoSnapshot) -> ReviewPlan {
    let mut files = snapshot
        .manifest
        .changed_file_entries
        .iter()
        .enumerate()
        .map(|(index, entry)| planned_file(snapshot, index, entry))
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.display().cmp(&right.path.display()));
    let counts = ReviewPlanCounts {
        total_files: files.len(),
        excluded_files: files
            .iter()
            .filter(|file| file.mode == ReviewPlanFileMode::Excluded)
            .count(),
        full_files: files
            .iter()
            .filter(|file| file.mode == ReviewPlanFileMode::Full)
            .count(),
        execution_eligible_files: files
            .iter()
            .filter(|file| file.mode != ReviewPlanFileMode::Excluded)
            .count(),
    };
    ReviewPlan {
        snapshot_id: snapshot.snapshot_id.clone(),
        counts,
        files,
    }
}

fn planned_file(
    snapshot: &RepoSnapshot,
    index: usize,
    entry: &ChangedFileMeta,
) -> PlannedReviewFile {
    let meta = snapshot
        .manifest
        .by_path
        .get(&entry.rel_path)
        .and_then(|file_id| snapshot.manifest.files.get(file_id.0 as usize));
    let content_state = content_state(entry, meta);
    let mut reasons = priority_reasons(&entry.rel_path);
    let mut score = 35u8;
    for reason in &reasons {
        score = score.saturating_add(match reason.code {
            "security_sensitive_path" => 35,
            "billing_sensitive_path" => 25,
            "database_migration" => 20,
            "application_command_path" => 18,
            "domain_model_path" => 15,
            "api_surface_path" => 12,
            "async_or_workflow_path" => 12,
            "rendering_sensitive_path" => 12,
            _ => 0,
        });
    }
    if reasons.is_empty() {
        reasons.push(ReviewPlanReason {
            code: "default_full",
            detail:
                "Default deterministic classification routes reviewable files through full review."
                    .to_string(),
        });
    }
    let exclusion = exclusion_reason(&entry.rel_path, content_state);
    let mode = if let Some(reason) = exclusion {
        reasons = vec![reason];
        score = 0;
        ReviewPlanFileMode::Excluded
    } else {
        ReviewPlanFileMode::Full
    };
    PlannedReviewFile {
        file_id: format!("changed-{index:04}"),
        path: entry.rel_path.clone(),
        status: status_from_summary(&entry.summary),
        content_state,
        estimated_bytes: meta.map(|file| file.size),
        mode,
        score: score.min(100),
        reasons,
    }
}

fn content_state(entry: &ChangedFileMeta, meta: Option<&FileMeta>) -> PlannedFileContentState {
    if entry.summary.contains("deleted") {
        return PlannedFileContentState::Deleted;
    }
    let Some(meta) = meta else {
        return PlannedFileContentState::Unavailable;
    };
    if !meta.is_text_candidate {
        return PlannedFileContentState::Binary;
    }
    match meta.capture_status {
        SnapshotCaptureStatus::Captured => PlannedFileContentState::Available,
        SnapshotCaptureStatus::NotTextCandidate => PlannedFileContentState::Binary,
        SnapshotCaptureStatus::SkippedMemoryLimit | SnapshotCaptureStatus::SkippedUnreadable => {
            PlannedFileContentState::Unavailable
        }
    }
}

fn exclusion_reason(
    path: &RepoPath,
    content_state: PlannedFileContentState,
) -> Option<ReviewPlanReason> {
    match content_state {
        PlannedFileContentState::Available => {}
        PlannedFileContentState::Deleted => {
            return Some(ReviewPlanReason {
                code: "deleted_file",
                detail: "File was deleted and has no head content to review directly.".to_string(),
            });
        }
        PlannedFileContentState::Binary => {
            return Some(ReviewPlanReason {
                code: "binary_or_non_text",
                detail: "File is not reviewable as text.".to_string(),
            });
        }
        PlannedFileContentState::Unavailable => {
            return Some(ReviewPlanReason {
                code: "unavailable_content",
                detail: "Changed file content was unavailable in the captured snapshot."
                    .to_string(),
            });
        }
    }
    let display = path.display();
    let lower = display.to_ascii_lowercase();
    if lower.ends_with("package-lock.json")
        || lower.ends_with("pnpm-lock.yaml")
        || lower.ends_with("yarn.lock")
        || lower.ends_with("cargo.lock")
        || lower.ends_with("go.sum")
    {
        return Some(ReviewPlanReason {
            code: "lockfile",
            detail: "Lockfiles are excluded from model review by default.".to_string(),
        });
    }
    if lower.contains("/vendor/")
        || lower.contains("/dist/")
        || lower.contains("/build/")
        || lower.contains("/target/")
        || lower.contains("/node_modules/")
    {
        return Some(ReviewPlanReason {
            code: "generated_or_vendor_path",
            detail: "Generated, vendored, or build-output paths are excluded.".to_string(),
        });
    }
    None
}

fn priority_reasons(path: &RepoPath) -> Vec<ReviewPlanReason> {
    let display = path.display();
    let lower = display.to_ascii_lowercase();
    let mut reasons = Vec::new();
    push_if(
        &mut reasons,
        lower.contains("auth")
            || lower.contains("permission")
            || lower.contains("security")
            || lower.contains("token")
            || lower.contains("credential"),
        "security_sensitive_path",
        "Path appears to touch authentication, authorization, or credentials.",
    );
    push_if(
        &mut reasons,
        lower.contains("billing")
            || lower.contains("payment")
            || lower.contains("invoice")
            || lower.contains("quota")
            || lower.contains("refund"),
        "billing_sensitive_path",
        "Path appears to touch billing, quota, payment, or refund behavior.",
    );
    push_if(
        &mut reasons,
        lower.contains("migration") || lower.contains("schema"),
        "database_migration",
        "Path appears to touch persistence schema or migration behavior.",
    );
    push_if(
        &mut reasons,
        lower.contains("/api/") || lower.contains("route.") || lower.contains("controller"),
        "api_surface_path",
        "Path appears to touch an API or request boundary.",
    );
    push_if(
        &mut reasons,
        lower.contains("model") || lower.contains("domain") || lower.contains("service"),
        "domain_model_path",
        "Path appears to touch domain or service behavior.",
    );
    push_if(
        &mut reasons,
        lower.contains("command")
            || lower.contains("worker")
            || lower.contains("workflow")
            || lower.contains("job"),
        "application_command_path",
        "Path appears to touch command, worker, job, or workflow behavior.",
    );
    push_if(
        &mut reasons,
        lower.contains("async") || lower.contains("queue") || lower.contains("event"),
        "async_or_workflow_path",
        "Path appears to touch async, queue, or event behavior.",
    );
    push_if(
        &mut reasons,
        is_template_or_render_path(Path::new(&display)),
        "rendering_sensitive_path",
        "Path appears to touch rendering or template behavior.",
    );
    reasons
}

fn push_if(
    reasons: &mut Vec<ReviewPlanReason>,
    condition: bool,
    code: &'static str,
    detail: &'static str,
) {
    if condition {
        reasons.push(ReviewPlanReason {
            code,
            detail: detail.to_string(),
        });
    }
}

fn is_template_or_render_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("tsx" | "jsx" | "erb" | "html" | "hbs" | "liquid" | "vue" | "svelte")
    )
}

fn status_from_summary(summary: &str) -> ChangedFileStatus {
    if summary.contains("added") {
        ChangedFileStatus::Added
    } else if summary.contains("deleted") {
        ChangedFileStatus::Deleted
    } else if summary.contains("renamed") {
        ChangedFileStatus::Renamed
    } else {
        ChangedFileStatus::Modified
    }
}
