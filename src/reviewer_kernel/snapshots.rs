use std::path::PathBuf;
use std::sync::Arc;

use crate::reviewer_kernel::kernel_types::{
    RepoPath, RuntimeError, RuntimeResult, SnapshotCapturePolicy, SnapshotCaptureStatus, SnapshotId,
};

use crate::reviewer_kernel::review_contract::{
    ChangeKind as ContractChangeKind, ChangeScopeV1, ChangedFileEntryV1,
    ChangedFileStatus as ContractChangedFileStatus, PathPolicyV1,
    RenameDetection as ContractRenameDetection, SnapshotMode as ContractSnapshotMode,
};
use crate::workspace::RepoSnapshot;

pub struct SnapshotSpec {
    pub snapshot_id: Option<SnapshotId>,
    pub repo_root: PathBuf,
    pub change: ChangeSpec,
    pub path_policy: SnapshotPathPolicy,
    pub capture_policy: SnapshotCapturePolicy,
}

impl SnapshotSpec {
    pub fn new(repo_root: impl Into<PathBuf>, change: ChangeSpec) -> Self {
        Self {
            snapshot_id: None,
            repo_root: repo_root.into(),
            change,
            path_policy: SnapshotPathPolicy::default(),
            capture_policy: SnapshotCapturePolicy::default(),
        }
    }

    pub fn with_snapshot_id(mut self, snapshot_id: SnapshotId) -> Self {
        self.snapshot_id = Some(snapshot_id);
        self
    }

    pub fn with_path_policy(mut self, path_policy: SnapshotPathPolicy) -> Self {
        self.path_policy = path_policy;
        self
    }

    pub fn with_capture_limit(mut self, max_captured_text_bytes: usize) -> Self {
        self.capture_policy = SnapshotCapturePolicy::new(max_captured_text_bytes);
        self
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotPathPolicy {
    pub allowed_roots: Vec<PathBuf>,
    pub denied_globs: Vec<String>,
    pub allowed_globs: Option<Vec<String>>,
    pub allow_dot_git: bool,
    pub follow_symlinks: bool,
    pub max_file_bytes: usize,
    pub max_diff_bytes: usize,
    pub max_search_results: usize,
    pub max_directory_entries: usize,
}

impl SnapshotPathPolicy {
    pub fn standard(max_file_bytes: usize, max_search_results: usize) -> Self {
        Self {
            max_file_bytes,
            max_diff_bytes: max_file_bytes,
            max_search_results,
            ..Self::default()
        }
    }
}

impl Default for SnapshotPathPolicy {
    fn default() -> Self {
        Self {
            allowed_roots: vec![PathBuf::from(".")],
            denied_globs: vec![
                ".git".to_string(),
                "node_modules".to_string(),
                "target".to_string(),
                ".venv".to_string(),
                "dist".to_string(),
                "build".to_string(),
                ".next".to_string(),
            ],
            allowed_globs: None,
            allow_dot_git: false,
            follow_symlinks: false,
            max_file_bytes: 200 * 1024,
            max_diff_bytes: 200 * 1024,
            max_search_results: 120,
            max_directory_entries: 20_000,
        }
    }
}

impl From<SnapshotPathPolicy> for PathPolicyV1 {
    fn from(value: SnapshotPathPolicy) -> Self {
        Self {
            allowed_roots: value.allowed_roots,
            denied_globs: value.denied_globs,
            allowed_globs: value.allowed_globs,
            allow_dot_git: value.allow_dot_git,
            follow_symlinks: value.follow_symlinks,
            max_file_bytes: value.max_file_bytes,
            max_diff_bytes: value.max_diff_bytes,
            max_search_results: value.max_search_results,
            max_directory_entries: value.max_directory_entries,
        }
    }
}

impl From<PathPolicyV1> for SnapshotPathPolicy {
    fn from(value: PathPolicyV1) -> Self {
        Self {
            allowed_roots: value.allowed_roots,
            denied_globs: value.denied_globs,
            allowed_globs: value.allowed_globs,
            allow_dot_git: value.allow_dot_git,
            follow_symlinks: value.follow_symlinks,
            max_file_bytes: value.max_file_bytes,
            max_diff_bytes: value.max_diff_bytes,
            max_search_results: value.max_search_results,
            max_directory_entries: value.max_directory_entries,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChangeSpec {
    pub kind: ChangeKind,
    pub change_id: String,
    pub source_ref: String,
    pub target_ref: String,
    pub base_revision_id: String,
    pub head_revision_id: String,
    pub merge_base_revision_id: Option<String>,
    pub inline_diff: Option<String>,
    pub snapshot_mode: SnapshotMode,
    pub rename_detection: RenameDetection,
    pub changed_files: Vec<ChangedFileSpec>,
}

impl ChangeSpec {
    pub fn local(
        change_id: impl Into<String>,
        head_revision_id: impl Into<String>,
        changed_files: Vec<ChangedFileSpec>,
    ) -> Self {
        Self {
            kind: ChangeKind::LocalDiff,
            change_id: change_id.into(),
            source_ref: "head".to_string(),
            target_ref: "base".to_string(),
            base_revision_id: "base".to_string(),
            head_revision_id: head_revision_id.into(),
            merge_base_revision_id: None,
            inline_diff: None,
            snapshot_mode: SnapshotMode::WorktreeHead,
            rename_detection: RenameDetection::None,
            changed_files,
        }
    }
}

impl From<ChangeSpec> for ChangeScopeV1 {
    fn from(value: ChangeSpec) -> Self {
        Self {
            kind: value.kind.into(),
            change_id: value.change_id,
            source_ref: value.source_ref,
            target_ref: value.target_ref,
            base_revision_id: value.base_revision_id,
            head_revision_id: value.head_revision_id,
            merge_base_revision_id: value.merge_base_revision_id,
            changed_files_manifest_ref: None,
            diff_manifest_ref: None,
            inline_diff: value.inline_diff,
            snapshot_mode: value.snapshot_mode.into(),
            rename_detection: value.rename_detection.into(),
            changed_files: value
                .changed_files
                .into_iter()
                .map(ChangedFileEntryV1::from)
                .collect(),
        }
    }
}

impl From<ChangeScopeV1> for ChangeSpec {
    fn from(value: ChangeScopeV1) -> Self {
        Self {
            kind: value.kind.into(),
            change_id: value.change_id,
            source_ref: value.source_ref,
            target_ref: value.target_ref,
            base_revision_id: value.base_revision_id,
            head_revision_id: value.head_revision_id,
            merge_base_revision_id: value.merge_base_revision_id,
            inline_diff: value.inline_diff,
            snapshot_mode: value.snapshot_mode.into(),
            rename_detection: value.rename_detection.into(),
            changed_files: value
                .changed_files
                .into_iter()
                .map(ChangedFileSpec::from)
                .collect(),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    PullRequest,
    MergeRequest,
    LocalDiff,
}

impl From<ChangeKind> for ContractChangeKind {
    fn from(value: ChangeKind) -> Self {
        match value {
            ChangeKind::PullRequest => Self::PullRequest,
            ChangeKind::MergeRequest => Self::MergeRequest,
            ChangeKind::LocalDiff => Self::LocalDiff,
        }
    }
}

impl From<ContractChangeKind> for ChangeKind {
    fn from(value: ContractChangeKind) -> Self {
        match value {
            ContractChangeKind::PullRequest => Self::PullRequest,
            ContractChangeKind::MergeRequest => Self::MergeRequest,
            ContractChangeKind::LocalDiff => Self::LocalDiff,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SnapshotMode {
    WorktreeHead,
    BaseHeadManifests,
}

impl From<SnapshotMode> for ContractSnapshotMode {
    fn from(value: SnapshotMode) -> Self {
        match value {
            SnapshotMode::WorktreeHead => Self::WorktreeHead,
            SnapshotMode::BaseHeadManifests => Self::BaseHeadManifests,
        }
    }
}

impl From<ContractSnapshotMode> for SnapshotMode {
    fn from(value: ContractSnapshotMode) -> Self {
        match value {
            ContractSnapshotMode::WorktreeHead => Self::WorktreeHead,
            ContractSnapshotMode::BaseHeadManifests => Self::BaseHeadManifests,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RenameDetection {
    None,
    AppManifest,
}

impl From<RenameDetection> for ContractRenameDetection {
    fn from(value: RenameDetection) -> Self {
        match value {
            RenameDetection::None => Self::None,
            RenameDetection::AppManifest => Self::AppManifest,
        }
    }
}

impl From<ContractRenameDetection> for RenameDetection {
    fn from(value: ContractRenameDetection) -> Self {
        match value {
            ContractRenameDetection::None => Self::None,
            ContractRenameDetection::AppManifest => Self::AppManifest,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChangedFileSpec {
    pub status: ChangedFileStatus,
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub old_content_hash: Option<String>,
    pub new_content_hash: Option<String>,
    pub is_binary: bool,
    pub is_generated: bool,
}

impl ChangedFileSpec {
    pub fn modified(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            status: ChangedFileStatus::Modified,
            old_path: Some(path.clone()),
            new_path: Some(path),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        }
    }
}

impl From<ChangedFileSpec> for ChangedFileEntryV1 {
    fn from(value: ChangedFileSpec) -> Self {
        Self {
            status: value.status.into(),
            old_path: value.old_path,
            new_path: value.new_path,
            old_content_hash: value.old_content_hash,
            new_content_hash: value.new_content_hash,
            is_binary: value.is_binary,
            is_generated: value.is_generated,
        }
    }
}

impl From<ChangedFileEntryV1> for ChangedFileSpec {
    fn from(value: ChangedFileEntryV1) -> Self {
        Self {
            status: value.status.into(),
            old_path: value.old_path,
            new_path: value.new_path,
            old_content_hash: value.old_content_hash,
            new_content_hash: value.new_content_hash,
            is_binary: value.is_binary,
            is_generated: value.is_generated,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ChangedFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

impl From<ChangedFileStatus> for ContractChangedFileStatus {
    fn from(value: ChangedFileStatus) -> Self {
        match value {
            ChangedFileStatus::Added => Self::Added,
            ChangedFileStatus::Modified => Self::Modified,
            ChangedFileStatus::Deleted => Self::Deleted,
            ChangedFileStatus::Renamed => Self::Renamed,
            ChangedFileStatus::Copied => Self::Copied,
            ChangedFileStatus::TypeChanged => Self::TypeChanged,
        }
    }
}

impl From<ContractChangedFileStatus> for ChangedFileStatus {
    fn from(value: ContractChangedFileStatus) -> Self {
        match value {
            ContractChangedFileStatus::Added => Self::Added,
            ContractChangedFileStatus::Modified => Self::Modified,
            ContractChangedFileStatus::Deleted => Self::Deleted,
            ContractChangedFileStatus::Renamed => Self::Renamed,
            ContractChangedFileStatus::Copied => Self::Copied,
            ContractChangedFileStatus::TypeChanged => Self::TypeChanged,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    pub snapshot_id: SnapshotId,
}

#[derive(Debug, Clone)]
pub struct SnapshotReader {
    snapshot: Arc<RepoSnapshot>,
}

impl SnapshotReader {
    pub(crate) fn new(snapshot: Arc<RepoSnapshot>) -> Self {
        Self { snapshot }
    }

    pub fn snapshot_id(&self) -> &SnapshotId {
        &self.snapshot.snapshot_id
    }

    pub fn manifest(&self) -> SnapshotManifest {
        SnapshotManifest::from_snapshot(&self.snapshot)
    }

    pub fn read_text(&self, path: &RepoPath, max_bytes: usize) -> RuntimeResult<SnapshotTextFile> {
        let file = self.snapshot.lookup(path)?;
        if file.capture_status == SnapshotCaptureStatus::SkippedMemoryLimit {
            return Err(RuntimeError::LimitExceeded {
                kind: "snapshot_capture_bytes",
            });
        }
        let content_hash = file.content_hash.clone().ok_or(RuntimeError::Invariant(
            "text candidate missing snapshot content hash",
        ))?;
        let (bytes, truncated) = self.snapshot.read_bounded(file.file_id, max_bytes)?;
        let content = String::from_utf8(bytes)
            .map_err(|_| RuntimeError::InvalidInput("snapshot file is not UTF-8".to_string()))?;
        Ok(SnapshotTextFile {
            snapshot_id: self.snapshot.snapshot_id.clone(),
            path: file.rel_path.clone(),
            content_hash,
            bytes: content.len(),
            truncated,
            content,
        })
    }

    pub fn read_text_path(
        &self,
        path: impl AsRef<str>,
        max_bytes: usize,
    ) -> RuntimeResult<SnapshotTextFile> {
        let path = RepoPath::parse(path.as_ref())?;
        self.read_text(&path, max_bytes)
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotManifest {
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub path_policy_hash: String,
    pub capture_policy_hash: String,
    pub capture_policy: SnapshotCapturePolicy,
    pub file_count: usize,
    pub changed_file_count: usize,
    pub captured_text_file_count: usize,
    pub captured_text_bytes: usize,
    pub capture_skipped_file_count: usize,
    pub capture_skipped_bytes: u64,
    pub files: Vec<SnapshotFile>,
    pub changed_files: Vec<SnapshotChangedFile>,
}

impl SnapshotManifest {
    pub fn max_captured_text_bytes(&self) -> usize {
        self.capture_policy.max_captured_text_bytes
    }

    fn from_snapshot(snapshot: &RepoSnapshot) -> Self {
        let files = snapshot
            .manifest
            .files
            .iter()
            .map(|file| SnapshotFile {
                path: file.rel_path.clone(),
                content_hash: file.content_hash.clone(),
                is_changed: file.is_changed,
                is_text_candidate: file.is_text_candidate,
                captured: file.snapshot_content.is_some(),
                capture_status: file.capture_status,
            })
            .collect::<Vec<_>>();
        let captured_text_file_count = files.iter().filter(|file| file.captured).count();
        let captured_text_bytes = snapshot
            .manifest
            .files
            .iter()
            .filter_map(|file| file.snapshot_content.as_ref())
            .map(|content| content.len())
            .sum();
        let changed_files = snapshot
            .manifest
            .changed_file_entries
            .iter()
            .map(|file| SnapshotChangedFile {
                path: file.rel_path.clone(),
            })
            .collect::<Vec<_>>();
        Self {
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            path_policy_hash: snapshot.path_policy_hash.clone(),
            capture_policy_hash: snapshot.capture_policy_hash.clone(),
            capture_policy: snapshot.capture_policy.clone(),
            file_count: files.len(),
            changed_file_count: snapshot.manifest.changed_files.len(),
            captured_text_file_count,
            captured_text_bytes,
            capture_skipped_file_count: snapshot.capture_skipped_files,
            capture_skipped_bytes: snapshot.capture_skipped_bytes,
            files,
            changed_files,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotFile {
    pub path: RepoPath,
    pub content_hash: Option<String>,
    pub is_changed: bool,
    pub is_text_candidate: bool,
    pub captured: bool,
    pub capture_status: SnapshotCaptureStatus,
}

impl SnapshotFile {
    pub fn capture_skipped_memory_limit(&self) -> bool {
        self.capture_status == SnapshotCaptureStatus::SkippedMemoryLimit
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotChangedFile {
    pub path: RepoPath,
}

#[derive(Debug, Clone)]
pub struct SnapshotTextFile {
    pub snapshot_id: SnapshotId,
    pub path: RepoPath,
    pub content_hash: String,
    pub bytes: usize,
    pub truncated: bool,
    pub content: String,
}
