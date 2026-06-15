use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::reviewer_kernel::kernel_types::{
    RepoPath, RuntimeError, RuntimeResult, SnapshotCaptureStatus, SnapshotId, SnapshotObjectStore,
    SnapshotStorageMode, SnapshotStoragePolicy,
};

use crate::reviewer_kernel::review_contract::{
    ChangeKind as ContractChangeKind, ChangeScopeV1, ChangedFileEntryV1,
    ChangedFileStatus as ContractChangedFileStatus, PathPolicyV1,
    RenameDetection as ContractRenameDetection, SnapshotMode as ContractSnapshotMode,
};
use crate::workspace::{remote_content_addressed_uri, RepoSnapshot, SnapshotContentRef};

use crate::reviewer_kernel::artifacts::*;
pub struct SnapshotSpec {
    pub snapshot_id: Option<SnapshotId>,
    pub repo_root: PathBuf,
    pub default_cwd: Option<PathBuf>,
    pub change: ChangeSpec,
    pub path_policy: SnapshotPathPolicy,
    pub storage_policy: SnapshotStoragePolicy,
}

impl SnapshotSpec {
    pub fn new(repo_root: impl Into<PathBuf>, change: ChangeSpec) -> Self {
        Self {
            snapshot_id: None,
            repo_root: repo_root.into(),
            default_cwd: None,
            change,
            path_policy: SnapshotPathPolicy::default(),
            storage_policy: SnapshotStoragePolicy::default(),
        }
    }

    pub fn with_default_cwd(mut self, default_cwd: impl Into<PathBuf>) -> Self {
        self.default_cwd = Some(default_cwd.into());
        self
    }

    pub fn with_snapshot_id(mut self, snapshot_id: SnapshotId) -> Self {
        self.snapshot_id = Some(snapshot_id);
        self
    }

    pub fn with_path_policy(mut self, path_policy: SnapshotPathPolicy) -> Self {
        self.path_policy = path_policy;
        self
    }

    pub fn with_storage_policy(mut self, storage_policy: SnapshotStoragePolicy) -> Self {
        self.storage_policy = storage_policy;
        self
    }

    pub fn with_memory_storage_limit(mut self, max_captured_text_bytes: usize) -> Self {
        self.storage_policy = SnapshotStoragePolicy::memory(max_captured_text_bytes);
        self
    }

    pub fn with_content_addressed_storage(
        mut self,
        root: impl Into<PathBuf>,
        max_captured_text_bytes: usize,
    ) -> Self {
        self.storage_policy =
            SnapshotStoragePolicy::content_addressed_directory(root, max_captured_text_bytes);
        self
    }

    pub fn with_remote_object_storage(
        mut self,
        base_uri: impl Into<String>,
        max_captured_text_bytes: usize,
        object_store: Arc<dyn SnapshotObjectStore>,
    ) -> RuntimeResult<Self> {
        self.storage_policy = SnapshotStoragePolicy::remote_object_store(
            base_uri,
            max_captured_text_bytes,
            object_store,
        )?;
        Ok(self)
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

    pub fn validate_storage(&self) -> RuntimeResult<SnapshotStorageValidationReport> {
        let mut report = SnapshotStorageValidationReport::new(
            self.snapshot.snapshot_id.clone(),
            self.snapshot.storage_policy.clone(),
        );
        for file in &self.snapshot.manifest.files {
            let Some(content) = file.snapshot_content.as_ref() else {
                continue;
            };
            let expected_hash = file.content_hash.clone().ok_or(RuntimeError::Invariant(
                "captured snapshot file missing content hash",
            ))?;
            let object = SnapshotStorageObject {
                path: file.rel_path.clone(),
                content_hash: expected_hash.clone(),
                bytes: content.len(),
                store_path: storage_object_path(content),
                store_uri: storage_object_uri(content),
            };
            report.checked_files += 1;
            report.checked_bytes += content.len();
            report.checked_objects.push(object.clone());
            let bytes = match content {
                SnapshotContentRef::Memory(bytes) => bytes.to_vec(),
                SnapshotContentRef::ContentAddressedFile { path, .. } => match fs::read(path) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        report.missing_files.push(object);
                        continue;
                    }
                    Err(error) => {
                        return Err(RuntimeError::RepoUnavailable(format!(
                            "failed to read snapshot storage object: {error}"
                        )))
                    }
                },
                SnapshotContentRef::RemoteObject { uri, store, .. } => {
                    let Some(bytes) = store.read_snapshot_object(uri)? else {
                        report.missing_files.push(object);
                        continue;
                    };
                    bytes
                }
            };
            if snapshot_content_hash(&bytes) != expected_hash {
                report.stale_files.push(object);
            }
        }
        report.valid = report.missing_files.is_empty() && report.stale_files.is_empty();
        Ok(report)
    }

    pub fn cleanup_storage(&self) -> RuntimeResult<SnapshotStorageCleanupReport> {
        let mut report = SnapshotStorageCleanupReport::new(
            self.snapshot.snapshot_id.clone(),
            self.snapshot.storage_policy.clone(),
        );
        let mut candidate_dirs = Vec::new();
        for file in &self.snapshot.manifest.files {
            let Some(content) = file.snapshot_content.as_ref() else {
                continue;
            };
            let expected_hash = file.content_hash.clone().ok_or(RuntimeError::Invariant(
                "captured snapshot file missing content hash",
            ))?;
            let object = SnapshotStorageObject {
                path: file.rel_path.clone(),
                content_hash: expected_hash,
                bytes: content.len(),
                store_path: storage_object_path(content),
                store_uri: storage_object_uri(content),
            };
            match content {
                SnapshotContentRef::Memory(_) => {}
                SnapshotContentRef::ContentAddressedFile { path, .. } => match fs::metadata(path) {
                    Ok(metadata) => {
                        fs::remove_file(path).map_err(|error| {
                            RuntimeError::RepoUnavailable(format!(
                                "failed to remove snapshot storage object: {error}"
                            ))
                        })?;
                        report.removed_files += 1;
                        report.removed_bytes =
                            report.removed_bytes.saturating_add(metadata.len() as usize);
                        report.removed_objects.push(object);
                        if let Some(parent) = path.parent() {
                            candidate_dirs.push(parent.to_path_buf());
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        report.missing_files.push(object);
                    }
                    Err(error) => {
                        return Err(RuntimeError::RepoUnavailable(format!(
                            "failed to inspect snapshot storage object: {error}"
                        )))
                    }
                },
                SnapshotContentRef::RemoteObject { uri, store, .. } => {
                    if store.remove_snapshot_object(uri)? {
                        report.removed_files += 1;
                        report.removed_bytes = report.removed_bytes.saturating_add(content.len());
                        report.removed_objects.push(object);
                    } else {
                        report.missing_files.push(object);
                    }
                }
            }
        }
        candidate_dirs.sort();
        candidate_dirs.dedup();
        for directory in candidate_dirs {
            if prune_empty_directory(&directory)? {
                report.pruned_empty_directories += 1;
            }
        }
        Ok(report)
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotManifest {
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub path_policy_hash: String,
    pub storage_policy_hash: String,
    pub storage_policy: SnapshotStoragePolicy,
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
    pub fn uses_content_addressed_storage(&self) -> bool {
        matches!(
            self.storage_policy.mode,
            SnapshotStorageMode::ContentAddressedDirectory { .. }
        )
    }

    pub fn uses_remote_object_storage(&self) -> bool {
        matches!(
            self.storage_policy.mode,
            SnapshotStorageMode::RemoteObjectStore { .. }
        )
    }

    pub fn max_captured_text_bytes(&self) -> usize {
        self.storage_policy.max_captured_text_bytes
    }

    fn from_snapshot(snapshot: &RepoSnapshot) -> Self {
        let files = snapshot
            .manifest
            .files
            .iter()
            .map(|file| SnapshotFile {
                path: file.rel_path.clone(),
                size: file.size,
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
                summary: file.summary.clone(),
            })
            .collect::<Vec<_>>();
        Self {
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            path_policy_hash: snapshot.path_policy_hash.clone(),
            storage_policy_hash: snapshot.storage_policy_hash.clone(),
            storage_policy: snapshot.storage_policy.clone(),
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
    pub size: u64,
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
    pub summary: String,
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

#[derive(Debug, Clone)]
pub struct SnapshotStorageObject {
    pub path: RepoPath,
    pub content_hash: String,
    pub bytes: usize,
    pub store_path: Option<PathBuf>,
    pub store_uri: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotStorageValidationReport {
    pub snapshot_id: SnapshotId,
    pub storage_policy: SnapshotStoragePolicy,
    pub checked_files: usize,
    pub checked_bytes: usize,
    pub checked_objects: Vec<SnapshotStorageObject>,
    pub valid: bool,
    pub missing_files: Vec<SnapshotStorageObject>,
    pub stale_files: Vec<SnapshotStorageObject>,
}

impl SnapshotStorageValidationReport {
    pub fn uses_content_addressed_storage(&self) -> bool {
        matches!(
            self.storage_policy.mode,
            SnapshotStorageMode::ContentAddressedDirectory { .. }
        )
    }

    pub fn uses_remote_object_storage(&self) -> bool {
        matches!(
            self.storage_policy.mode,
            SnapshotStorageMode::RemoteObjectStore { .. }
        )
    }

    fn new(snapshot_id: SnapshotId, storage_policy: SnapshotStoragePolicy) -> Self {
        Self {
            snapshot_id,
            storage_policy,
            checked_files: 0,
            checked_bytes: 0,
            checked_objects: Vec::new(),
            valid: true,
            missing_files: Vec::new(),
            stale_files: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotStorageCleanupReport {
    pub snapshot_id: SnapshotId,
    pub storage_policy: SnapshotStoragePolicy,
    pub removed_files: usize,
    pub removed_bytes: usize,
    pub removed_objects: Vec<SnapshotStorageObject>,
    pub missing_files: Vec<SnapshotStorageObject>,
    pub pruned_empty_directories: usize,
}

impl SnapshotStorageCleanupReport {
    fn new(snapshot_id: SnapshotId, storage_policy: SnapshotStoragePolicy) -> Self {
        Self {
            snapshot_id,
            storage_policy,
            removed_files: 0,
            removed_bytes: 0,
            removed_objects: Vec::new(),
            missing_files: Vec::new(),
            pruned_empty_directories: 0,
        }
    }
}

#[async_trait]
pub trait RemoteSnapshotObjectClient: Send + Sync {
    fn put_remote_snapshot_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()>;

    fn read_remote_snapshot_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>>;

    fn remove_remote_snapshot_object(&self, uri: &str) -> RuntimeResult<bool>;
}

pub struct RemoteSnapshotObjectStore {
    base_uri: String,
    client: Arc<dyn RemoteSnapshotObjectClient>,
}

impl RemoteSnapshotObjectStore {
    pub fn new(
        base_uri: impl Into<String>,
        client: Arc<dyn RemoteSnapshotObjectClient>,
    ) -> RuntimeResult<Self> {
        Ok(Self {
            base_uri: normalize_remote_store_base_uri(base_uri.into(), "snapshot")?,
            client,
        })
    }

    pub fn base_uri(&self) -> &str {
        &self.base_uri
    }
}

impl std::fmt::Debug for RemoteSnapshotObjectStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteSnapshotObjectStore")
            .field("base_uri", &self.base_uri)
            .finish_non_exhaustive()
    }
}

impl SnapshotObjectStore for RemoteSnapshotObjectStore {
    fn put_snapshot_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()> {
        validate_remote_snapshot_object_uri(&self.base_uri, uri)?;
        self.client.put_remote_snapshot_object(uri, bytes)
    }

    fn read_snapshot_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>> {
        validate_remote_snapshot_object_uri(&self.base_uri, uri)?;
        self.client.read_remote_snapshot_object(uri)
    }

    fn remove_snapshot_object(&self, uri: &str) -> RuntimeResult<bool> {
        validate_remote_snapshot_object_uri(&self.base_uri, uri)?;
        self.client.remove_remote_snapshot_object(uri)
    }
}

#[derive(Debug, Default)]
pub struct InMemoryRemoteSnapshotObjectClient {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryRemoteSnapshotObjectClient {
    pub fn read(&self, uri: &str) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .expect("in-memory remote snapshot object client poisoned")
            .get(uri)
            .cloned()
    }

    pub fn write(&self, uri: impl Into<String>, bytes: Vec<u8>) {
        self.objects
            .lock()
            .expect("in-memory remote snapshot object client poisoned")
            .insert(uri.into(), bytes);
    }

    pub fn remove(&self, uri: &str) {
        self.objects
            .lock()
            .expect("in-memory remote snapshot object client poisoned")
            .remove(uri);
    }

    pub fn object_count(&self) -> usize {
        self.objects
            .lock()
            .expect("in-memory remote snapshot object client poisoned")
            .len()
    }
}

impl RemoteSnapshotObjectClient for InMemoryRemoteSnapshotObjectClient {
    fn put_remote_snapshot_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()> {
        self.write(uri.to_string(), bytes);
        Ok(())
    }

    fn read_remote_snapshot_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>> {
        Ok(self.read(uri))
    }

    fn remove_remote_snapshot_object(&self, uri: &str) -> RuntimeResult<bool> {
        let mut objects = self
            .objects
            .lock()
            .expect("in-memory remote snapshot object client poisoned");
        Ok(objects.remove(uri).is_some())
    }
}

#[derive(Debug, Clone)]
pub struct HttpRemoteObjectClient {
    http: reqwest::blocking::Client,
    authorization_header: Option<String>,
}

impl HttpRemoteObjectClient {
    pub fn new() -> RuntimeResult<Self> {
        Self::with_authorization_header(None)
    }

    pub fn bearer_token(token: impl Into<String>) -> RuntimeResult<Self> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "remote object-store bearer token must not be empty".to_string(),
            ));
        }
        Self::with_authorization_header(Some(format!("Bearer {token}")))
    }

    pub fn with_authorization_header(authorization_header: Option<String>) -> RuntimeResult<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| {
                RuntimeError::RepoUnavailable(format!(
                    "failed to build remote object-store HTTP client: {error}"
                ))
            })?;
        Ok(Self {
            http,
            authorization_header,
        })
    }

    fn put_remote_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()> {
        let response = self
            .with_auth(self.http.put(uri).body(bytes))
            .send()
            .map_err(remote_object_http_error)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(remote_object_http_status_error(
                "put",
                uri,
                response.status(),
            ))
        }
    }

    fn read_remote_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>> {
        let response = self
            .with_auth(self.http.get(uri))
            .send()
            .map_err(remote_object_http_error)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(remote_object_http_status_error(
                "read",
                uri,
                response.status(),
            ));
        }
        let bytes = response.bytes().map_err(remote_object_http_error)?;
        Ok(Some(bytes.to_vec()))
    }

    fn remove_remote_object(&self, uri: &str) -> RuntimeResult<bool> {
        let response = self
            .with_auth(self.http.delete(uri))
            .send()
            .map_err(remote_object_http_error)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if response.status().is_success() {
            Ok(true)
        } else {
            Err(remote_object_http_status_error(
                "remove",
                uri,
                response.status(),
            ))
        }
    }

    fn with_auth(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        match &self.authorization_header {
            Some(header) => request.header(reqwest::header::AUTHORIZATION, header),
            None => request,
        }
    }
}

impl RemoteSnapshotObjectClient for HttpRemoteObjectClient {
    fn put_remote_snapshot_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()> {
        self.put_remote_object(uri, bytes)
    }

    fn read_remote_snapshot_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>> {
        self.read_remote_object(uri)
    }

    fn remove_remote_snapshot_object(&self, uri: &str) -> RuntimeResult<bool> {
        self.remove_remote_object(uri)
    }
}

impl RemoteArtifactObjectClient for HttpRemoteObjectClient {
    fn put_remote_artifact_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()> {
        self.put_remote_object(uri, bytes)
    }

    fn read_remote_artifact_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>> {
        self.read_remote_object(uri)
    }

    fn remove_remote_artifact_object(&self, uri: &str) -> RuntimeResult<bool> {
        self.remove_remote_object(uri)
    }
}

fn storage_object_path(content: &SnapshotContentRef) -> Option<PathBuf> {
    match content {
        SnapshotContentRef::Memory(_) => None,
        SnapshotContentRef::ContentAddressedFile { path, .. } => Some(path.clone()),
        SnapshotContentRef::RemoteObject { .. } => None,
    }
}

fn storage_object_uri(content: &SnapshotContentRef) -> Option<String> {
    match content {
        SnapshotContentRef::Memory(_) | SnapshotContentRef::ContentAddressedFile { .. } => None,
        SnapshotContentRef::RemoteObject { uri, .. } => Some(uri.clone()),
    }
}

fn validate_remote_snapshot_object_uri(base_uri: &str, uri: &str) -> RuntimeResult<()> {
    let prefix = format!("{}/snapshots/", base_uri.trim_end_matches('/'));
    let Some(hash) = uri.strip_prefix(&prefix) else {
        return Err(RuntimeError::RepoAccessDenied);
    };
    if remote_content_addressed_uri(base_uri, hash)? != uri {
        return Err(RuntimeError::RepoAccessDenied);
    }
    Ok(())
}

pub(crate) fn snapshot_content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub(crate) fn prune_empty_directory(path: &Path) -> RuntimeResult<bool> {
    let mut entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(RuntimeError::RepoUnavailable(format!(
                "failed to inspect snapshot storage directory: {error}"
            )))
        }
    };
    if entries.next().is_some() {
        return Ok(false);
    }
    fs::remove_dir(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!(
            "failed to prune snapshot storage directory: {error}"
        ))
    })?;
    Ok(true)
}
