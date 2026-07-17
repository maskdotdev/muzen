use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ignore::WalkBuilder;
use moka::sync::Cache;

use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::review_contract::{ChangeScopeV1, ChangedFileStatus, PathPolicyV1};
use crate::reviewer_kernel::snapshots::SnapshotSourceRoot;
use crate::workspace::is_textish;

const SNAPSHOT_CONTENT_CACHE_BYTES: u64 = 128 * 1024 * 1024;

static SNAPSHOT_CONTENT_CACHE: OnceLock<Cache<String, Arc<Vec<u8>>>> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct RepoSnapshot {
    pub(crate) snapshot_id: SnapshotId,
    pub(crate) source: SnapshotSourceRoot,
    /// Canonicalized checkout root the snapshot was captured from.
    /// Read-only git history access (co-change mining) roots here.
    pub(crate) source_root: PathBuf,
    pub(crate) manifest_hash: String,
    pub(crate) path_policy_hash: String,
    #[cfg(test)]
    pub(crate) capture_policy_hash: String,
    #[cfg(test)]
    pub(crate) capture_policy: SnapshotCapturePolicy,
    #[cfg(test)]
    pub(crate) capture_skipped_files: usize,
    #[cfg(test)]
    pub(crate) capture_skipped_bytes: u64,
    pub(crate) manifest: Arc<FileManifest>,
    pub(crate) diff: Arc<DiffArtifact>,
}

#[derive(Debug)]
pub(crate) struct FileManifest {
    pub(crate) by_path: HashMap<RepoPath, FileId>,
    pub(crate) files: Vec<FileMeta>,
    pub(crate) changed_files: Vec<FileId>,
    pub(crate) changed_file_entries: Vec<ChangedFileMeta>,
}

#[derive(Debug, Clone)]
pub(crate) struct FileMeta {
    pub(crate) file_id: FileId,
    pub(crate) rel_path: RepoPath,
    pub(crate) size: u64,
    pub(crate) fingerprint: String,
    pub(crate) content_hash: Option<String>,
    pub(crate) content_ref: Option<SnapshotContentRef>,
    pub(crate) capture_status: SnapshotCaptureStatus,
    pub(crate) is_changed: bool,
    pub(crate) is_text_candidate: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum SnapshotContentRef {
    Eager(Arc<[u8]>),
    Disk {
        expected_size: u64,
        expected_hash: String,
    },
}

impl SnapshotContentRef {
    pub(crate) fn eager_len(&self) -> Option<usize> {
        match self {
            Self::Eager(content) => Some(content.len()),
            Self::Disk { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_eager(&self) -> bool {
        matches!(self, Self::Eager(_))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChangedFileMeta {
    pub(crate) rel_path: RepoPath,
    pub(crate) summary: String,
}

struct SnapshotCandidateFile {
    path: PathBuf,
    repo_path: RepoPath,
    meta: fs::Metadata,
}

#[derive(Debug)]
pub(crate) struct DiffArtifact {
    pub(crate) content: String,
    pub(crate) content_hash: String,
}

impl RepoSnapshot {
    #[cfg(test)]
    pub(crate) fn build(
        root: &Path,
        policy: &PathPolicyV1,
        change: &ChangeScopeV1,
    ) -> RuntimeResult<Arc<Self>> {
        Self::build_with_capture_policy(root, policy, change, SnapshotCapturePolicy::default())
    }

    pub(crate) fn build_with_capture_policy(
        root: &Path,
        policy: &PathPolicyV1,
        change: &ChangeScopeV1,
        capture_policy: SnapshotCapturePolicy,
    ) -> RuntimeResult<Arc<Self>> {
        Self::build_with_source_root(
            SnapshotSourceRoot::external(root),
            policy,
            change,
            capture_policy,
        )
    }

    pub(crate) fn build_with_source_root(
        source: SnapshotSourceRoot,
        policy: &PathPolicyV1,
        change: &ChangeScopeV1,
        capture_policy: SnapshotCapturePolicy,
    ) -> RuntimeResult<Arc<Self>> {
        let root = source.path();
        let root_path = fs::canonicalize(root).map_err(|error| {
            RuntimeError::RepoUnavailable(format!(
                "failed to canonicalize repo root {}: {error}",
                root.display()
            ))
        })?;
        if !root_path.is_dir() {
            return Err(RuntimeError::RepoUnavailable(format!(
                "repo root is not a directory: {}",
                root_path.display()
            )));
        }
        let changed_paths = changed_paths(change);
        let mut files = Vec::new();
        let mut by_path = HashMap::new();
        let mut changed_files = Vec::new();
        let changed_file_entries = changed_file_entries(change);
        let mut captured_text_bytes = 0usize;
        #[cfg(test)]
        let mut capture_skipped_files = 0usize;
        #[cfg(test)]
        let mut capture_skipped_bytes = 0u64;

        let mut walker = WalkBuilder::new(&root_path);
        walker
            .hidden(false)
            .parents(false)
            .git_ignore(false)
            .git_exclude(false)
            .follow_links(false);

        let mut candidate_files = Vec::new();
        for entry in walker.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let file_type = match entry.file_type() {
                Some(file_type) => file_type,
                None => continue,
            };
            if !file_type.is_file() {
                continue;
            }
            let rel = match entry.path().strip_prefix(&root_path) {
                Ok(rel) => rel,
                Err(_) => continue,
            };
            let rel_text = match rel.to_str() {
                Some(value) => value,
                None => continue,
            };
            let repo_path = match RepoPath::parse(rel_text) {
                Ok(path) => path,
                Err(_) => continue,
            };
            if is_denied(policy, repo_path.as_path()) || !is_allowed(policy, repo_path.as_path()) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if !meta.is_file() {
                continue;
            }
            candidate_files.push(SnapshotCandidateFile {
                path: entry.path().to_path_buf(),
                repo_path,
                meta,
            });
        }
        candidate_files.sort_by(|left, right| {
            let left_changed = changed_paths.contains(&left.repo_path.display());
            let right_changed = changed_paths.contains(&right.repo_path.display());
            right_changed
                .cmp(&left_changed)
                .then_with(|| left.repo_path.display().cmp(&right.repo_path.display()))
        });

        for candidate in candidate_files {
            if files.len() >= policy.max_directory_entries {
                break;
            }
            let repo_path = candidate.repo_path;
            let size = candidate.meta.len();
            let can_read_text =
                is_textish(repo_path.as_path()) && size <= policy.max_file_bytes as u64;
            let (content_hash, content_ref, capture_status, is_text_candidate) = if can_read_text {
                match snapshot_file_content(&candidate.path, policy.max_file_bytes) {
                    Ok(content) => {
                        let content_hash = content.hash.clone();
                        let budgeted_size = content.bytes.len();
                        if captured_text_bytes.saturating_add(budgeted_size)
                            <= capture_policy.max_captured_text_bytes
                        {
                            captured_text_bytes =
                                captured_text_bytes.saturating_add(content.bytes.len());
                            (
                                Some(content_hash),
                                Some(SnapshotContentRef::Eager(capture_snapshot_content(content))),
                                SnapshotCaptureStatus::Captured,
                                true,
                            )
                        } else {
                            #[cfg(test)]
                            {
                                capture_skipped_files += 1;
                                capture_skipped_bytes += size;
                            }
                            (
                                Some(content_hash.clone()),
                                Some(SnapshotContentRef::Disk {
                                    expected_size: size,
                                    expected_hash: content_hash,
                                }),
                                SnapshotCaptureStatus::SkippedMemoryLimit,
                                true,
                            )
                        }
                    }
                    Err(_) => (None, None, SnapshotCaptureStatus::SkippedUnreadable, false),
                }
            } else {
                (None, None, SnapshotCaptureStatus::NotTextCandidate, false)
            };
            let is_changed = changed_paths.contains(&repo_path.display());
            let file_id = FileId(files.len() as u32);
            let fingerprint = stable_id(&[
                &repo_path.display(),
                &size.to_string(),
                content_hash.as_deref().unwrap_or(""),
            ]);
            if is_changed {
                changed_files.push(file_id);
            }
            by_path.insert(repo_path.clone(), file_id);
            files.push(FileMeta {
                file_id,
                rel_path: repo_path,
                size,
                fingerprint,
                content_hash,
                content_ref,
                capture_status,
                is_changed,
                is_text_candidate,
            });
        }

        files.sort_by(|left, right| left.rel_path.display().cmp(&right.rel_path.display()));
        by_path.clear();
        changed_files.clear();
        for (index, file) in files.iter_mut().enumerate() {
            file.file_id = FileId(index as u32);
            if file.is_changed {
                changed_files.push(file.file_id);
            }
            by_path.insert(file.rel_path.clone(), file.file_id);
        }

        let diff = Arc::new(build_diff(change));
        let path_policy_hash = path_policy_hash(policy);
        let capture_policy_hash = capture_policy_hash(&capture_policy);
        let manifest_hash = stable_id(
            &files
                .iter()
                .map(|file| {
                    format!(
                        "{}:{}",
                        file.rel_path.display(),
                        file.content_hash.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        let snapshot_id = SnapshotId(stable_id(&[
            &root_path.display().to_string(),
            &change.change_id,
            &change.base_revision_id,
            &change.head_revision_id,
            &diff.content_hash,
            &manifest_hash,
            &path_policy_hash,
            &capture_policy_hash,
            &CONCURRENT_CONTRACT_VERSION.to_string(),
            &REDACTION_POLICY_VERSION.to_string(),
        ]));
        let manifest = Arc::new(FileManifest {
            by_path,
            files,
            changed_files,
            changed_file_entries,
        });
        Ok(Arc::new(Self {
            snapshot_id,
            source,
            source_root: root_path,
            manifest_hash,
            path_policy_hash,
            #[cfg(test)]
            capture_policy_hash,
            #[cfg(test)]
            capture_policy,
            #[cfg(test)]
            capture_skipped_files,
            #[cfg(test)]
            capture_skipped_bytes,
            manifest,
            diff,
        }))
    }

    pub(crate) fn lookup(&self, path: &RepoPath) -> RuntimeResult<&FileMeta> {
        let file_id = self
            .manifest
            .by_path
            .get(path)
            .ok_or(RuntimeError::RepoAccessDenied)?;
        self.file(*file_id)
    }

    pub(crate) fn file(&self, file_id: FileId) -> RuntimeResult<&FileMeta> {
        self.manifest
            .files
            .get(file_id.0 as usize)
            .ok_or(RuntimeError::Invariant("file_id not present in manifest"))
    }

    pub(crate) fn read_bounded(
        &self,
        file_id: FileId,
        max_bytes: usize,
    ) -> RuntimeResult<(Vec<u8>, bool)> {
        let file = self.file(file_id)?;
        if !file.is_text_candidate && file.size > max_bytes as u64 {
            return Err(RuntimeError::LimitExceeded { kind: "file_bytes" });
        }
        if !file.is_text_candidate {
            return match file.capture_status {
                SnapshotCaptureStatus::SkippedMemoryLimit => Err(RuntimeError::LimitExceeded {
                    kind: "snapshot_capture_bytes",
                }),
                SnapshotCaptureStatus::SkippedUnreadable => Err(RuntimeError::InvalidInput(
                    "file is not text-readable".to_string(),
                )),
                _ => Err(RuntimeError::InvalidInput(
                    "file is not text-readable".to_string(),
                )),
            };
        }
        file.content_hash.as_ref().ok_or(RuntimeError::Invariant(
            "text candidate missing snapshot content hash",
        ))?;
        if file.size > max_bytes as u64 {
            return Err(RuntimeError::LimitExceeded { kind: "file_bytes" });
        }
        let content_ref = file.content_ref.as_ref().ok_or(RuntimeError::Invariant(
            "text candidate missing snapshot content ref",
        ))?;
        let meta_expected_hash = file.content_hash.as_ref().ok_or(RuntimeError::Invariant(
            "text candidate missing snapshot content hash",
        ))?;
        match content_ref {
            SnapshotContentRef::Eager(content) => {
                if content.len() > max_bytes {
                    return Err(RuntimeError::LimitExceeded { kind: "file_bytes" });
                }
                let bytes = content.to_vec();
                if content_hash(&bytes) != *meta_expected_hash {
                    return Err(RuntimeError::SnapshotStale {
                        path: file.rel_path.display(),
                    });
                }
                Ok((bytes, false))
            }
            SnapshotContentRef::Disk {
                expected_size,
                expected_hash: disk_expected_hash,
            } => {
                if *expected_size != file.size || disk_expected_hash != meta_expected_hash {
                    return Err(RuntimeError::Invariant(
                        "disk content ref does not match file meta",
                    ));
                }
                let cache_key = stable_id(&[
                    &self.snapshot_id.0,
                    &file.file_id.0.to_string(),
                    &file.fingerprint,
                    disk_expected_hash,
                ]);
                let cache = snapshot_content_cache();
                if let Some(bytes) = cache.get(&cache_key) {
                    return Ok((bytes.as_ref().clone(), false));
                }
                let path = self.source.path().join(file.rel_path.as_path());
                let content = snapshot_file_content(&path, max_bytes)?;
                if content.bytes.len() as u64 != *expected_size
                    || content.hash != *disk_expected_hash
                {
                    return Err(RuntimeError::SnapshotStale {
                        path: file.rel_path.display(),
                    });
                }
                let bytes = Arc::new(content.bytes);
                cache.insert(cache_key, Arc::clone(&bytes));
                Ok((bytes.as_ref().clone(), false))
            }
        }
    }
}

fn snapshot_content_cache() -> &'static Cache<String, Arc<Vec<u8>>> {
    SNAPSHOT_CONTENT_CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(SNAPSHOT_CONTENT_CACHE_BYTES)
            .weigher(|_key, value: &Arc<Vec<u8>>| value.len().try_into().unwrap_or(u32::MAX))
            .build()
    })
}

#[derive(Debug, Clone)]
struct CapturedFileContent {
    hash: String,
    bytes: Vec<u8>,
}

fn snapshot_file_content(path: &Path, max_bytes: usize) -> RuntimeResult<CapturedFileContent> {
    let mut file = fs::File::open(path)
        .map_err(|error| RuntimeError::RepoUnavailable(format!("open failed: {error}")))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| RuntimeError::RepoUnavailable(format!("read failed: {error}")))?;
    if bytes.len() > max_bytes || bytes.contains(&0) {
        return Err(RuntimeError::InvalidInput(
            "file is not text-readable".to_string(),
        ));
    }
    Ok(CapturedFileContent {
        hash: content_hash(&bytes),
        bytes,
    })
}

fn capture_snapshot_content(content: CapturedFileContent) -> Arc<[u8]> {
    Arc::from(content.bytes.into_boxed_slice())
}

fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn path_policy_hash(policy: &PathPolicyV1) -> String {
    let mut parts = Vec::new();
    parts.push(format!("allow_dot_git={}", policy.allow_dot_git));
    parts.push(format!("follow_symlinks={}", policy.follow_symlinks));
    parts.push(format!("max_file_bytes={}", policy.max_file_bytes));
    parts.push(format!("max_diff_bytes={}", policy.max_diff_bytes));
    parts.push(format!("max_search_results={}", policy.max_search_results));
    parts.push(format!(
        "max_directory_entries={}",
        policy.max_directory_entries
    ));
    parts.extend(
        policy
            .allowed_roots
            .iter()
            .map(|path| format!("allowed={}", path.to_string_lossy().replace('\\', "/"))),
    );
    parts.extend(
        policy
            .denied_globs
            .iter()
            .map(|glob| format!("denied={glob}")),
    );
    if let Some(allowed_globs) = &policy.allowed_globs {
        parts.extend(
            allowed_globs
                .iter()
                .map(|glob| format!("allowed_glob={glob}")),
        );
    }
    let refs = parts.iter().map(String::as_str).collect::<Vec<_>>();
    stable_id(&refs)
}

fn capture_policy_hash(policy: &SnapshotCapturePolicy) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "max_captured_text_bytes={}",
        policy.max_captured_text_bytes
    ));
    let refs = parts.iter().map(String::as_str).collect::<Vec<_>>();
    stable_id(&refs)
}

fn changed_paths(change: &ChangeScopeV1) -> HashSet<String> {
    change
        .changed_files
        .iter()
        .filter_map(|file| file.new_path.as_ref().or(file.old_path.as_ref()))
        .filter_map(|path| path.to_str())
        .map(ToOwned::to_owned)
        .collect()
}

fn changed_file_entries(change: &ChangeScopeV1) -> Vec<ChangedFileMeta> {
    change
        .changed_files
        .iter()
        .filter_map(|file| {
            let path = file.new_path.as_ref().or(file.old_path.as_ref())?;
            let text = path.to_str()?;
            let rel_path = RepoPath::parse(text).ok()?;
            let status = match file.status {
                ChangedFileStatus::Added => "Added",
                ChangedFileStatus::Modified => "Modified",
                ChangedFileStatus::Deleted => "Deleted",
                ChangedFileStatus::Renamed => "Renamed",
                ChangedFileStatus::Copied => "Copied",
                ChangedFileStatus::TypeChanged => "TypeChanged",
            };
            Some(ChangedFileMeta {
                summary: format!("{status} {}", rel_path.display()),
                rel_path,
            })
        })
        .collect()
}

fn build_diff(change: &ChangeScopeV1) -> DiffArtifact {
    if let Some(content) = change
        .inline_diff
        .as_ref()
        .filter(|content| !content.trim().is_empty())
    {
        let content_hash = stable_id(&[content]);
        return DiffArtifact {
            content: content.clone(),
            content_hash,
        };
    }
    let mut content = format!(
        "change {} {}..{}\n",
        change.change_id, change.base_revision_id, change.head_revision_id
    );
    for file in &change.changed_files {
        let path = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|value| value.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let status = match file.status {
            ChangedFileStatus::Added => "added",
            ChangedFileStatus::Modified => "modified",
            ChangedFileStatus::Deleted => "deleted",
            ChangedFileStatus::Renamed => "renamed",
            ChangedFileStatus::Copied => "copied",
            ChangedFileStatus::TypeChanged => "type_changed",
        };
        content.push_str(status);
        content.push(' ');
        content.push_str(&path);
        content.push('\n');
    }
    let content_hash = stable_id(&[&content]);
    DiffArtifact {
        content,
        content_hash,
    }
}

fn is_denied(policy: &PathPolicyV1, clean: &Path) -> bool {
    for component in clean.components() {
        let std::path::Component::Normal(part) = component else {
            continue;
        };
        let name = part.to_string_lossy();
        if !policy.allow_dot_git && name == ".git" {
            return true;
        }
        if policy
            .denied_globs
            .iter()
            .any(|glob| glob == name.as_ref() || glob == &clean.to_string_lossy())
        {
            return true;
        }
    }
    false
}

fn is_allowed(policy: &PathPolicyV1, clean: &Path) -> bool {
    policy.allowed_roots.iter().any(|root| {
        if root == Path::new(".") {
            return true;
        }
        let Ok(root) = RepoPath::from_path(root.clone()) else {
            return false;
        };
        root.as_path() == Path::new(".")
            || clean == root.as_path()
            || clean.starts_with(root.as_path())
    })
}

#[cfg(test)]
mod tests;
