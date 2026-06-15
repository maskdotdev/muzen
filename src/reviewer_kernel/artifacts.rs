use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::reviewer_kernel::kernel_types::{ArtifactId, ArtifactView, RuntimeError, RuntimeResult};
use crate::reviewer_kernel::tool_engine::ConcurrentArtifactStore as RuntimeArtifactStore;

use crate::reviewer_kernel::kernel_types::stable_id;

use crate::reviewer_kernel::adapters::capabilities;
use crate::reviewer_kernel::snapshots::*;
#[async_trait]
pub trait ArtifactReader: Send + Sync {
    fn get_artifact(&self, artifact_id: &ArtifactId) -> Option<ArtifactView>;
    fn list_artifacts(&self) -> Vec<ArtifactView>;
    fn export_with_policy(
        &self,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactExportManifest>;
    fn export_bundle(
        &self,
        root: &Path,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactBundleManifest>;
    fn persist_with_policy(
        &self,
        object_store: &dyn ArtifactObjectStore,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactPersistenceManifest>;
}

impl ArtifactReader for RuntimeArtifactStore {
    fn get_artifact(&self, artifact_id: &ArtifactId) -> Option<ArtifactView> {
        self.get(artifact_id)
    }

    fn list_artifacts(&self) -> Vec<ArtifactView> {
        self.list()
    }

    fn export_with_policy(
        &self,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactExportManifest> {
        let artifacts = artifacts_for_policy(
            if policy.include_raw() {
                self.list_raw()
            } else {
                self.list()
            },
            &policy,
        )?;
        Ok(ArtifactExportManifest {
            view: artifact_view_mode(&policy),
            retention: policy.retention_policy().clone(),
            artifact_count: artifacts.len(),
            total_bytes: artifacts.iter().map(|artifact| artifact.bytes).sum(),
            artifacts,
        })
    }

    fn export_bundle(
        &self,
        root: &Path,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactBundleManifest> {
        let artifacts = artifacts_for_policy(
            if policy.include_raw() {
                self.list_raw()
            } else {
                self.list()
            },
            &policy,
        )?;
        let artifact_dir = root.join(ARTIFACT_BUNDLE_ARTIFACTS_DIR);
        std::fs::create_dir_all(&artifact_dir).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to create artifact bundle: {error}"))
        })?;

        let mut entries = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            let relative_path = PathBuf::from(ARTIFACT_BUNDLE_ARTIFACTS_DIR)
                .join(format!("{}.txt", artifact.artifact_id.0));
            let artifact_path = root.join(&relative_path);
            std::fs::write(&artifact_path, artifact.content.as_bytes()).map_err(|error| {
                RuntimeError::RepoUnavailable(format!("failed to write artifact bundle: {error}"))
            })?;
            entries.push(ArtifactBundleEntry {
                artifact_id: artifact.artifact_id,
                bytes: artifact.bytes,
                content_hash: artifact.content_hash,
                relative_path,
            });
        }

        let total_bytes = entries.iter().map(|entry| entry.bytes).sum();
        let manifest_path = root.join(ARTIFACT_BUNDLE_MANIFEST_FILE);
        let view = artifact_view_mode(&policy);
        let manifest_json = serde_json::json!({
            "view": artifact_view_name(view),
            "artifactCount": entries.len(),
            "totalBytes": total_bytes,
            "retention": policy.retention_policy(),
            "artifacts": entries.iter().map(|entry| serde_json::json!({
                "artifactId": entry.artifact_id,
                "bytes": entry.bytes,
                "contentHash": entry.content_hash,
                "relativePath": entry.relative_path.to_string_lossy(),
            })).collect::<Vec<_>>(),
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest_json).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to serialize artifact bundle: {error}"))
        })?;
        std::fs::write(&manifest_path, manifest_bytes).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write artifact bundle: {error}"))
        })?;

        Ok(ArtifactBundleManifest {
            view,
            root: root.to_path_buf(),
            manifest_path,
            retention: policy.retention_policy().clone(),
            artifact_count: entries.len(),
            total_bytes,
            artifacts: entries,
        })
    }

    fn persist_with_policy(
        &self,
        object_store: &dyn ArtifactObjectStore,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactPersistenceManifest> {
        let artifacts = artifacts_for_policy(
            if policy.include_raw() {
                self.list_raw()
            } else {
                self.list()
            },
            &policy,
        )?;
        persist_artifacts_to_store(artifacts, object_store, policy)
    }
}

fn artifacts_for_policy(
    artifacts: Vec<ArtifactView>,
    policy: &ArtifactExportPolicy,
) -> RuntimeResult<Vec<ArtifactView>> {
    let artifacts = artifacts
        .into_iter()
        .filter(|artifact| policy.allows_artifact(&artifact.artifact_id))
        .collect::<Vec<_>>();
    policy.validate_retention(
        artifacts.len(),
        artifacts.iter().map(|artifact| artifact.bytes).sum(),
    )?;
    Ok(artifacts)
}

fn persist_artifacts_to_store(
    artifacts: Vec<ArtifactView>,
    object_store: &dyn ArtifactObjectStore,
    policy: ArtifactExportPolicy,
) -> RuntimeResult<ArtifactPersistenceManifest> {
    let view = artifact_view_mode(&policy);
    let total_bytes = artifacts.iter().map(|artifact| artifact.bytes).sum();
    let mut objects = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let object = ArtifactStoreObject {
            artifact_id: artifact.artifact_id,
            view,
            bytes: artifact.bytes,
            content_hash: artifact.content_hash,
            content: artifact.content,
        };
        objects.push(object_store.put_artifact_object(object)?);
    }
    Ok(ArtifactPersistenceManifest {
        view,
        retention: policy.retention_policy().clone(),
        artifact_count: objects.len(),
        total_bytes,
        objects,
    })
}

const ARTIFACT_BUNDLE_ARTIFACTS_DIR: &str = "artifacts";
const ARTIFACT_BUNDLE_MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactViewMode {
    Redacted,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactExportPolicy {
    include_raw: bool,
    allowed_artifact_ids: Option<Vec<ArtifactId>>,
    retention: ArtifactRetentionPolicy,
}

impl ArtifactExportPolicy {
    pub fn redacted_all() -> Self {
        Self::redacted_with_artifacts(None)
    }

    pub fn redacted_artifacts<I, S>(artifact_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::redacted_with_artifacts(Some(
            artifact_ids
                .into_iter()
                .map(|artifact_id| ArtifactId(artifact_id.as_ref().to_string()))
                .collect(),
        ))
    }

    pub fn redacted(capabilities: &capabilities::CapabilitySet) -> RuntimeResult<Self> {
        if capabilities.artifact_access.read_redacted || capabilities.artifact_access.read_raw {
            Ok(Self::redacted_with_artifacts(
                capabilities.artifact_access.allowed_artifact_ids.clone(),
            ))
        } else {
            Err(RuntimeError::InvalidInput(
                "redacted artifact export requires artifact read capability".to_string(),
            ))
        }
    }

    pub fn raw(capabilities: &capabilities::CapabilitySet) -> RuntimeResult<Self> {
        if capabilities.artifact_access.read_raw {
            Ok(Self {
                include_raw: true,
                allowed_artifact_ids: capabilities.artifact_access.allowed_artifact_ids.clone(),
                retention: ArtifactRetentionPolicy::unlimited(),
            })
        } else {
            Err(RuntimeError::InvalidInput(
                "raw artifact export requires raw artifact read capability".to_string(),
            ))
        }
    }

    pub fn include_raw(&self) -> bool {
        self.include_raw
    }

    fn redacted_with_artifacts(allowed_artifact_ids: Option<Vec<ArtifactId>>) -> Self {
        Self {
            include_raw: false,
            allowed_artifact_ids,
            retention: ArtifactRetentionPolicy::unlimited(),
        }
    }

    pub(crate) fn with_artifact_ids<I, S>(mut self, artifact_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.allowed_artifact_ids = Some(
            artifact_ids
                .into_iter()
                .map(|artifact_id| ArtifactId(artifact_id.as_ref().to_string()))
                .collect(),
        );
        self
    }

    pub fn with_retention_policy(mut self, retention: ArtifactRetentionPolicy) -> Self {
        self.retention = retention;
        self
    }

    pub fn retention_policy(&self) -> &ArtifactRetentionPolicy {
        &self.retention
    }

    pub fn allows_artifact(&self, artifact_id: &ArtifactId) -> bool {
        match &self.allowed_artifact_ids {
            Some(allowed) => allowed.iter().any(|allowed_id| allowed_id == artifact_id),
            None => true,
        }
    }

    pub(crate) fn validate_retention(
        &self,
        artifact_count: usize,
        total_bytes: usize,
    ) -> RuntimeResult<()> {
        self.retention.validate(artifact_count, total_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRetentionPolicy {
    pub max_artifacts: Option<usize>,
    pub max_bytes: Option<usize>,
}

impl ArtifactRetentionPolicy {
    pub fn unlimited() -> Self {
        Self {
            max_artifacts: None,
            max_bytes: None,
        }
    }

    pub fn max_artifacts(max_artifacts: usize) -> Self {
        Self {
            max_artifacts: Some(max_artifacts),
            max_bytes: None,
        }
    }

    pub fn max_bytes(max_bytes: usize) -> Self {
        Self {
            max_artifacts: None,
            max_bytes: Some(max_bytes),
        }
    }

    pub fn bounded(max_artifacts: usize, max_bytes: usize) -> Self {
        Self {
            max_artifacts: Some(max_artifacts),
            max_bytes: Some(max_bytes),
        }
    }

    fn validate(&self, artifact_count: usize, total_bytes: usize) -> RuntimeResult<()> {
        if self
            .max_artifacts
            .is_some_and(|max_artifacts| artifact_count > max_artifacts)
        {
            return Err(RuntimeError::LimitExceeded {
                kind: "artifact_retention_artifacts",
            });
        }
        if self
            .max_bytes
            .is_some_and(|max_bytes| total_bytes > max_bytes)
        {
            return Err(RuntimeError::LimitExceeded {
                kind: "artifact_retention_bytes",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactExportManifest {
    pub view: ArtifactViewMode,
    pub retention: ArtifactRetentionPolicy,
    pub artifact_count: usize,
    pub total_bytes: usize,
    pub artifacts: Vec<ArtifactView>,
}

impl ArtifactExportManifest {
    pub fn first_artifact_id(&self) -> Option<&str> {
        self.artifacts.first().map(ArtifactView::artifact_id)
    }

    pub fn contains_artifact_id(&self, artifact_id: impl AsRef<str>) -> bool {
        let artifact_id = artifact_id.as_ref();
        self.artifacts
            .iter()
            .any(|artifact| artifact.artifact_id() == artifact_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPersistenceManifest {
    pub view: ArtifactViewMode,
    pub retention: ArtifactRetentionPolicy,
    pub artifact_count: usize,
    pub total_bytes: usize,
    pub objects: Vec<ArtifactObjectRef>,
}

impl ArtifactPersistenceManifest {
    pub fn object_refs(&self) -> &[ArtifactObjectRef] {
        &self.objects
    }

    pub fn first_object_ref(&self) -> Option<&ArtifactObjectRef> {
        self.objects.first()
    }

    pub fn contains_artifact_id(&self, artifact_id: impl AsRef<str>) -> bool {
        let artifact_id = artifact_id.as_ref();
        self.objects
            .iter()
            .any(|object_ref| object_ref.artifact_id() == artifact_id)
    }

    pub fn validate_storage(
        &self,
        object_reader: &dyn ArtifactObjectReader,
    ) -> RuntimeResult<ArtifactObjectStorageValidationReport> {
        let mut report = ArtifactObjectStorageValidationReport::new(
            self.view,
            self.retention.clone(),
            self.artifact_count,
            self.total_bytes,
        );
        for object_ref in &self.objects {
            report.checked_objects += 1;
            report.checked_bytes = report.checked_bytes.saturating_add(object_ref.bytes);
            let Some(bytes) = object_reader.read_artifact_object(object_ref)? else {
                report.missing_objects.push(object_ref.clone());
                continue;
            };
            if !artifact_object_bytes_match(object_ref, &bytes) {
                report.stale_objects.push(object_ref.clone());
            }
        }
        report.valid = report.checked_objects == self.artifact_count
            && report.checked_bytes == self.total_bytes
            && report.missing_objects.is_empty()
            && report.stale_objects.is_empty();
        Ok(report)
    }

    pub fn cleanup_storage(
        &self,
        object_store: &dyn ArtifactObjectStore,
    ) -> RuntimeResult<ArtifactObjectStorageCleanupReport> {
        let mut report = ArtifactObjectStorageCleanupReport::new(
            self.view,
            self.retention.clone(),
            self.artifact_count,
            self.total_bytes,
        );
        for object_ref in &self.objects {
            report.checked_objects += 1;
            let Some(bytes) = object_store.read_artifact_object(object_ref)? else {
                report.missing_objects.push(object_ref.clone());
                continue;
            };
            report.checked_bytes = report.checked_bytes.saturating_add(bytes.len());
            if !artifact_object_bytes_match(object_ref, &bytes) {
                report.stale_objects.push(object_ref.clone());
            }
            if object_store.remove_artifact_object(object_ref)? {
                report.removed_bytes = report.removed_bytes.saturating_add(bytes.len());
                report.removed_objects.push(object_ref.clone());
            } else {
                report.missing_objects.push(object_ref.clone());
            }
        }
        Ok(report)
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactStoreObject {
    pub artifact_id: ArtifactId,
    pub view: ArtifactViewMode,
    pub bytes: usize,
    pub content_hash: String,
    pub content: String,
}

impl ArtifactStoreObject {
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactObjectRef {
    pub artifact_id: ArtifactId,
    pub view: ArtifactViewMode,
    pub bytes: usize,
    pub content_hash: String,
    pub uri: String,
    pub path: Option<PathBuf>,
}

impl ArtifactObjectRef {
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id.0
    }

    pub fn view(&self) -> ArtifactViewMode {
        self.view
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn has_local_path(&self) -> bool {
        self.path.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactObjectStorageValidationReport {
    pub view: ArtifactViewMode,
    pub retention: ArtifactRetentionPolicy,
    pub expected_objects: usize,
    pub expected_bytes: usize,
    pub checked_objects: usize,
    pub checked_bytes: usize,
    pub valid: bool,
    pub missing_objects: Vec<ArtifactObjectRef>,
    pub stale_objects: Vec<ArtifactObjectRef>,
}

impl ArtifactObjectStorageValidationReport {
    fn new(
        view: ArtifactViewMode,
        retention: ArtifactRetentionPolicy,
        expected_objects: usize,
        expected_bytes: usize,
    ) -> Self {
        Self {
            view,
            retention,
            expected_objects,
            expected_bytes,
            checked_objects: 0,
            checked_bytes: 0,
            valid: true,
            missing_objects: Vec::new(),
            stale_objects: Vec::new(),
        }
    }

    pub fn has_missing_artifact(&self, artifact_id: impl AsRef<str>) -> bool {
        let artifact_id = artifact_id.as_ref();
        self.missing_objects
            .iter()
            .any(|object_ref| object_ref.artifact_id() == artifact_id)
    }

    pub fn has_stale_artifact(&self, artifact_id: impl AsRef<str>) -> bool {
        let artifact_id = artifact_id.as_ref();
        self.stale_objects
            .iter()
            .any(|object_ref| object_ref.artifact_id() == artifact_id)
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactObjectStorageCleanupReport {
    pub view: ArtifactViewMode,
    pub retention: ArtifactRetentionPolicy,
    pub expected_objects: usize,
    pub expected_bytes: usize,
    pub checked_objects: usize,
    pub checked_bytes: usize,
    pub removed_objects: Vec<ArtifactObjectRef>,
    pub removed_bytes: usize,
    pub missing_objects: Vec<ArtifactObjectRef>,
    pub stale_objects: Vec<ArtifactObjectRef>,
}

impl ArtifactObjectStorageCleanupReport {
    fn new(
        view: ArtifactViewMode,
        retention: ArtifactRetentionPolicy,
        expected_objects: usize,
        expected_bytes: usize,
    ) -> Self {
        Self {
            view,
            retention,
            expected_objects,
            expected_bytes,
            checked_objects: 0,
            checked_bytes: 0,
            removed_objects: Vec::new(),
            removed_bytes: 0,
            missing_objects: Vec::new(),
            stale_objects: Vec::new(),
        }
    }

    pub fn has_removed_artifact(&self, artifact_id: impl AsRef<str>) -> bool {
        let artifact_id = artifact_id.as_ref();
        self.removed_objects
            .iter()
            .any(|object_ref| object_ref.artifact_id() == artifact_id)
    }

    pub fn has_missing_artifact(&self, artifact_id: impl AsRef<str>) -> bool {
        let artifact_id = artifact_id.as_ref();
        self.missing_objects
            .iter()
            .any(|object_ref| object_ref.artifact_id() == artifact_id)
    }

    pub fn has_stale_artifact(&self, artifact_id: impl AsRef<str>) -> bool {
        let artifact_id = artifact_id.as_ref();
        self.stale_objects
            .iter()
            .any(|object_ref| object_ref.artifact_id() == artifact_id)
    }
}

#[async_trait]
pub trait ArtifactObjectReader: Send + Sync {
    fn read_artifact_object(
        &self,
        object_ref: &ArtifactObjectRef,
    ) -> RuntimeResult<Option<Vec<u8>>>;
}

#[async_trait]
pub trait ArtifactObjectStore: ArtifactObjectReader {
    fn put_artifact_object(&self, object: ArtifactStoreObject) -> RuntimeResult<ArtifactObjectRef>;

    fn remove_artifact_object(&self, object_ref: &ArtifactObjectRef) -> RuntimeResult<bool>;
}

#[async_trait]
pub trait RemoteArtifactObjectClient: Send + Sync {
    fn put_remote_artifact_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()>;

    fn read_remote_artifact_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>>;

    fn remove_remote_artifact_object(&self, uri: &str) -> RuntimeResult<bool>;
}

pub struct RemoteArtifactObjectStore {
    base_uri: String,
    client: Arc<dyn RemoteArtifactObjectClient>,
}

impl RemoteArtifactObjectStore {
    pub fn new(
        base_uri: impl Into<String>,
        client: Arc<dyn RemoteArtifactObjectClient>,
    ) -> RuntimeResult<Self> {
        Ok(Self {
            base_uri: normalize_remote_store_base_uri(base_uri.into(), "artifact")?,
            client,
        })
    }

    pub fn base_uri(&self) -> &str {
        &self.base_uri
    }
}

impl std::fmt::Debug for RemoteArtifactObjectStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteArtifactObjectStore")
            .field("base_uri", &self.base_uri)
            .finish_non_exhaustive()
    }
}

impl ArtifactObjectReader for RemoteArtifactObjectStore {
    fn read_artifact_object(
        &self,
        object_ref: &ArtifactObjectRef,
    ) -> RuntimeResult<Option<Vec<u8>>> {
        let expected_uri =
            remote_artifact_object_uri(&self.base_uri, object_ref.view, &object_ref.content_hash)?;
        if object_ref.uri != expected_uri {
            return Err(RuntimeError::RepoAccessDenied);
        }
        self.client.read_remote_artifact_object(&object_ref.uri)
    }
}

impl ArtifactObjectStore for RemoteArtifactObjectStore {
    fn put_artifact_object(&self, object: ArtifactStoreObject) -> RuntimeResult<ArtifactObjectRef> {
        validate_artifact_store_object(&object)?;
        let uri = remote_artifact_object_uri(&self.base_uri, object.view, &object.content_hash)?;
        self.client
            .put_remote_artifact_object(&uri, object.content.into_bytes())?;
        Ok(ArtifactObjectRef {
            artifact_id: object.artifact_id,
            view: object.view,
            bytes: object.bytes,
            content_hash: object.content_hash,
            uri,
            path: None,
        })
    }

    fn remove_artifact_object(&self, object_ref: &ArtifactObjectRef) -> RuntimeResult<bool> {
        let expected_uri =
            remote_artifact_object_uri(&self.base_uri, object_ref.view, &object_ref.content_hash)?;
        if object_ref.uri != expected_uri {
            return Err(RuntimeError::RepoAccessDenied);
        }
        self.client.remove_remote_artifact_object(&object_ref.uri)
    }
}

#[derive(Debug)]
pub struct LocalArtifactObjectStore {
    root: PathBuf,
}

impl LocalArtifactObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl ArtifactObjectReader for LocalArtifactObjectStore {
    fn read_artifact_object(
        &self,
        object_ref: &ArtifactObjectRef,
    ) -> RuntimeResult<Option<Vec<u8>>> {
        let path =
            local_artifact_object_path(&self.root, object_ref.view, &object_ref.content_hash)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RuntimeError::RepoUnavailable(format!(
                "failed to read artifact object: {error}"
            ))),
        }
    }
}

impl ArtifactObjectStore for LocalArtifactObjectStore {
    fn put_artifact_object(&self, object: ArtifactStoreObject) -> RuntimeResult<ArtifactObjectRef> {
        validate_artifact_store_object(&object)?;
        let path = local_artifact_object_path(&self.root, object.view, &object.content_hash)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RuntimeError::RepoUnavailable(format!(
                    "failed to create artifact object store directory: {error}"
                ))
            })?;
        }
        fs::write(&path, object.content.as_bytes()).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write artifact object: {error}"))
        })?;
        Ok(ArtifactObjectRef {
            artifact_id: object.artifact_id,
            view: object.view,
            bytes: object.bytes,
            content_hash: object.content_hash,
            uri: path.to_string_lossy().to_string(),
            path: Some(path),
        })
    }

    fn remove_artifact_object(&self, object_ref: &ArtifactObjectRef) -> RuntimeResult<bool> {
        let path =
            local_artifact_object_path(&self.root, object_ref.view, &object_ref.content_hash)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(RuntimeError::RepoUnavailable(format!(
                "failed to remove artifact object: {error}"
            ))),
        }
    }
}

#[derive(Debug, Default)]
pub struct InMemoryArtifactObjectStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryArtifactObjectStore {
    pub fn read(&self, uri: &str) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .expect("in-memory artifact object store poisoned")
            .get(uri)
            .cloned()
    }

    pub fn read_object(&self, object_ref: &ArtifactObjectRef) -> RuntimeResult<Option<Vec<u8>>> {
        self.read_artifact_object(object_ref)
    }

    pub fn object_count(&self) -> usize {
        self.objects
            .lock()
            .expect("in-memory artifact object store poisoned")
            .len()
    }
}

impl ArtifactObjectReader for InMemoryArtifactObjectStore {
    fn read_artifact_object(
        &self,
        object_ref: &ArtifactObjectRef,
    ) -> RuntimeResult<Option<Vec<u8>>> {
        Ok(self.read(&object_ref.uri))
    }
}

impl ArtifactObjectStore for InMemoryArtifactObjectStore {
    fn put_artifact_object(&self, object: ArtifactStoreObject) -> RuntimeResult<ArtifactObjectRef> {
        validate_artifact_store_object(&object)?;
        let uri = format!(
            "memory://artifacts/{}/{}",
            artifact_view_name(object.view),
            object.content_hash
        );
        self.objects
            .lock()
            .expect("in-memory artifact object store poisoned")
            .insert(uri.clone(), object.content.into_bytes());
        Ok(ArtifactObjectRef {
            artifact_id: object.artifact_id,
            view: object.view,
            bytes: object.bytes,
            content_hash: object.content_hash,
            uri,
            path: None,
        })
    }

    fn remove_artifact_object(&self, object_ref: &ArtifactObjectRef) -> RuntimeResult<bool> {
        let mut objects = self
            .objects
            .lock()
            .expect("in-memory artifact object store poisoned");
        Ok(objects.remove(&object_ref.uri).is_some())
    }
}

#[derive(Debug, Default)]
pub struct InMemoryRemoteArtifactObjectClient {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryRemoteArtifactObjectClient {
    pub fn read(&self, uri: &str) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .expect("in-memory remote artifact object client poisoned")
            .get(uri)
            .cloned()
    }

    pub fn write(&self, uri: impl Into<String>, bytes: Vec<u8>) {
        self.objects
            .lock()
            .expect("in-memory remote artifact object client poisoned")
            .insert(uri.into(), bytes);
    }

    pub fn remove(&self, uri: &str) {
        self.objects
            .lock()
            .expect("in-memory remote artifact object client poisoned")
            .remove(uri);
    }

    pub fn object_count(&self) -> usize {
        self.objects
            .lock()
            .expect("in-memory remote artifact object client poisoned")
            .len()
    }
}

impl RemoteArtifactObjectClient for InMemoryRemoteArtifactObjectClient {
    fn put_remote_artifact_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()> {
        self.write(uri.to_string(), bytes);
        Ok(())
    }

    fn read_remote_artifact_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>> {
        Ok(self.read(uri))
    }

    fn remove_remote_artifact_object(&self, uri: &str) -> RuntimeResult<bool> {
        let mut objects = self
            .objects
            .lock()
            .expect("in-memory remote artifact object client poisoned");
        Ok(objects.remove(uri).is_some())
    }
}
fn validate_artifact_store_object(object: &ArtifactStoreObject) -> RuntimeResult<()> {
    if object.content.len() != object.bytes || stable_id(&[&object.content]) != object.content_hash
    {
        return Err(RuntimeError::InvalidInput(
            "artifact object content does not match metadata".to_string(),
        ));
    }
    Ok(())
}

fn artifact_object_bytes_match(object_ref: &ArtifactObjectRef, bytes: &[u8]) -> bool {
    if bytes.len() != object_ref.bytes {
        return false;
    }
    let Ok(content) = std::str::from_utf8(bytes) else {
        return false;
    };
    stable_id(&[content]) == object_ref.content_hash
}

pub(crate) fn remote_object_http_error(error: reqwest::Error) -> RuntimeError {
    RuntimeError::RepoUnavailable(format!("remote object-store HTTP request failed: {error}"))
}

pub(crate) fn remote_object_http_status_error(
    operation: &str,
    _uri: &str,
    status: reqwest::StatusCode,
) -> RuntimeError {
    RuntimeError::RepoUnavailable(format!(
        "remote object-store HTTP {operation} failed with status {status}"
    ))
}

pub(crate) fn normalize_remote_store_base_uri(
    base_uri: String,
    object_kind: &str,
) -> RuntimeResult<String> {
    let normalized = base_uri.trim_end_matches('/').to_string();
    if normalized.is_empty()
        || !normalized.contains("://")
        || normalized.starts_with("file://")
        || normalized.chars().any(char::is_whitespace)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "remote {object_kind} object store requires a non-file URI base"
        )));
    }
    Ok(normalized)
}

pub(crate) fn remote_artifact_object_uri(
    base_uri: &str,
    view: ArtifactViewMode,
    content_hash: &str,
) -> RuntimeResult<String> {
    if content_hash.len() != 64 || !content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RuntimeError::RepoAccessDenied);
    }
    Ok(format!(
        "{base_uri}/artifacts/{}/{}.txt",
        artifact_view_name(view),
        content_hash
    ))
}

fn local_artifact_object_path(
    root: &Path,
    view: ArtifactViewMode,
    content_hash: &str,
) -> RuntimeResult<PathBuf> {
    if content_hash.len() != 64 || !content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RuntimeError::RepoAccessDenied);
    }
    Ok(root
        .join(artifact_view_name(view))
        .join(&content_hash[..2])
        .join(format!("{content_hash}.txt")))
}

fn artifact_view_mode(policy: &ArtifactExportPolicy) -> ArtifactViewMode {
    if policy.include_raw() {
        ArtifactViewMode::Raw
    } else {
        ArtifactViewMode::Redacted
    }
}

fn artifact_view_name(view: ArtifactViewMode) -> &'static str {
    match view {
        ArtifactViewMode::Redacted => "redacted",
        ArtifactViewMode::Raw => "raw",
    }
}

pub(crate) fn evidence_kind_name(
    kind: crate::reviewer_kernel::review_contract::ArtifactKind,
) -> &'static str {
    match kind {
        crate::reviewer_kernel::review_contract::ArtifactKind::FileSlice => "file_slice",
        crate::reviewer_kernel::review_contract::ArtifactKind::DiffHunk => "diff_hunk",
        crate::reviewer_kernel::review_contract::ArtifactKind::SearchResults => "search_results",
        crate::reviewer_kernel::review_contract::ArtifactKind::FileList => "file_list",
        crate::reviewer_kernel::review_contract::ArtifactKind::ChangedFileList => {
            "changed_file_list"
        }
        crate::reviewer_kernel::review_contract::ArtifactKind::ImportSummary => "import_summary",
        crate::reviewer_kernel::review_contract::ArtifactKind::ToolSummary => "tool_summary",
        crate::reviewer_kernel::review_contract::ArtifactKind::RedactedView => "redacted_view",
    }
}

pub(crate) fn finding_severity_name(
    severity: crate::reviewer_kernel::review_contract::FindingSeverity,
) -> &'static str {
    match severity {
        crate::reviewer_kernel::review_contract::FindingSeverity::Blocker => "blocker",
        crate::reviewer_kernel::review_contract::FindingSeverity::High => "high",
        crate::reviewer_kernel::review_contract::FindingSeverity::Medium => "medium",
        crate::reviewer_kernel::review_contract::FindingSeverity::Low => "low",
        crate::reviewer_kernel::review_contract::FindingSeverity::Nit => "nit",
    }
}

pub(crate) fn validation_status_name(
    status: crate::reviewer_kernel::review_contract::ValidationStatus,
) -> &'static str {
    match status {
        crate::reviewer_kernel::review_contract::ValidationStatus::Candidate => "candidate",
        crate::reviewer_kernel::review_contract::ValidationStatus::Challenged => "challenged",
        crate::reviewer_kernel::review_contract::ValidationStatus::Validated => "validated",
        crate::reviewer_kernel::review_contract::ValidationStatus::Rejected => "rejected",
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactBundleManifest {
    pub view: ArtifactViewMode,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub retention: ArtifactRetentionPolicy,
    pub artifact_count: usize,
    pub total_bytes: usize,
    pub artifacts: Vec<ArtifactBundleEntry>,
}

impl ArtifactBundleManifest {
    pub fn new(
        view: ArtifactViewMode,
        root: impl Into<PathBuf>,
        retention: ArtifactRetentionPolicy,
        artifacts: Vec<ArtifactBundleEntry>,
    ) -> Self {
        let root = root.into();
        let artifact_count = artifacts.len();
        let total_bytes = artifacts.iter().map(|entry| entry.bytes).sum();
        let manifest_path = root.join(ARTIFACT_BUNDLE_MANIFEST_FILE);
        Self {
            view,
            root,
            manifest_path,
            retention,
            artifact_count,
            total_bytes,
            artifacts,
        }
    }

    pub fn with_manifest_path(mut self, manifest_path: impl Into<PathBuf>) -> Self {
        self.manifest_path = manifest_path.into();
        self
    }

    pub fn validate_storage(&self) -> RuntimeResult<ArtifactBundleValidationReport> {
        let manifest_path = safe_bundle_manifest_path(&self.root, &self.manifest_path)?;
        let mut report = ArtifactBundleValidationReport {
            root: self.root.clone(),
            manifest_path: manifest_path.clone(),
            view: self.view,
            retention: self.retention.clone(),
            checked_artifacts: 0,
            checked_bytes: 0,
            checked_objects: Vec::new(),
            manifest_present: manifest_path.exists(),
            valid: true,
            missing_artifacts: Vec::new(),
            stale_artifacts: Vec::new(),
        };
        for entry in &self.artifacts {
            let object = ArtifactBundleObject::from_entry(&self.root, entry)?;
            report.checked_artifacts += 1;
            report.checked_bytes = report.checked_bytes.saturating_add(entry.bytes);
            report.checked_objects.push(object.clone());
            let bytes = match fs::read(&object.path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    report.missing_artifacts.push(object);
                    continue;
                }
                Err(error) => {
                    return Err(RuntimeError::RepoUnavailable(format!(
                        "failed to read artifact bundle object: {error}"
                    )))
                }
            };
            let content_hash = std::str::from_utf8(&bytes)
                .ok()
                .map(|content| stable_id(&[content]));
            if bytes.len() != entry.bytes
                || content_hash.as_deref() != Some(entry.content_hash.as_str())
            {
                report.stale_artifacts.push(object);
            }
        }
        report.valid = report.manifest_present
            && report.missing_artifacts.is_empty()
            && report.stale_artifacts.is_empty();
        Ok(report)
    }

    pub fn cleanup_storage(&self) -> RuntimeResult<ArtifactBundleCleanupReport> {
        let manifest_path = safe_bundle_manifest_path(&self.root, &self.manifest_path)?;
        let mut report = ArtifactBundleCleanupReport {
            root: self.root.clone(),
            manifest_path: manifest_path.clone(),
            view: self.view,
            retention: self.retention.clone(),
            removed_artifacts: 0,
            removed_bytes: 0,
            removed_objects: Vec::new(),
            missing_artifacts: Vec::new(),
            removed_manifest: false,
            pruned_empty_directories: 0,
        };
        let mut candidate_dirs = Vec::new();
        for entry in &self.artifacts {
            let object = ArtifactBundleObject::from_entry(&self.root, entry)?;
            match fs::metadata(&object.path) {
                Ok(metadata) => {
                    fs::remove_file(&object.path).map_err(|error| {
                        RuntimeError::RepoUnavailable(format!(
                            "failed to remove artifact bundle object: {error}"
                        ))
                    })?;
                    report.removed_artifacts += 1;
                    report.removed_bytes =
                        report.removed_bytes.saturating_add(metadata.len() as usize);
                    if let Some(parent) = object.path.parent() {
                        candidate_dirs.push(parent.to_path_buf());
                    }
                    report.removed_objects.push(object);
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    report.missing_artifacts.push(object);
                }
                Err(error) => {
                    return Err(RuntimeError::RepoUnavailable(format!(
                        "failed to inspect artifact bundle object: {error}"
                    )))
                }
            }
        }
        match fs::remove_file(&manifest_path) {
            Ok(()) => report.removed_manifest = true,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(RuntimeError::RepoUnavailable(format!(
                    "failed to remove artifact bundle manifest: {error}"
                )))
            }
        }
        candidate_dirs.sort();
        candidate_dirs.dedup();
        for directory in candidate_dirs {
            if directory.starts_with(&self.root) && prune_empty_directory(&directory)? {
                report.pruned_empty_directories += 1;
            }
        }
        Ok(report)
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactBundleEntry {
    pub artifact_id: ArtifactId,
    pub bytes: usize,
    pub content_hash: String,
    pub relative_path: PathBuf,
}

impl ArtifactBundleEntry {
    pub fn new(
        artifact_id: impl Into<String>,
        bytes: usize,
        content_hash: impl Into<String>,
        relative_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            artifact_id: ArtifactId(artifact_id.into()),
            bytes,
            content_hash: content_hash.into(),
            relative_path: relative_path.into(),
        }
    }

    pub fn artifact_id(&self) -> &str {
        &self.artifact_id.0
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactBundleObject {
    pub artifact_id: ArtifactId,
    pub bytes: usize,
    pub content_hash: String,
    pub relative_path: PathBuf,
    pub path: PathBuf,
}

impl ArtifactBundleObject {
    fn from_entry(root: &Path, entry: &ArtifactBundleEntry) -> RuntimeResult<Self> {
        Ok(Self {
            artifact_id: entry.artifact_id.clone(),
            bytes: entry.bytes,
            content_hash: entry.content_hash.clone(),
            relative_path: entry.relative_path.clone(),
            path: safe_bundle_entry_path(root, &entry.relative_path)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactBundleValidationReport {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub view: ArtifactViewMode,
    pub retention: ArtifactRetentionPolicy,
    pub checked_artifacts: usize,
    pub checked_bytes: usize,
    pub checked_objects: Vec<ArtifactBundleObject>,
    pub manifest_present: bool,
    pub valid: bool,
    pub missing_artifacts: Vec<ArtifactBundleObject>,
    pub stale_artifacts: Vec<ArtifactBundleObject>,
}

#[derive(Debug, Clone)]
pub struct ArtifactBundleCleanupReport {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub view: ArtifactViewMode,
    pub retention: ArtifactRetentionPolicy,
    pub removed_artifacts: usize,
    pub removed_bytes: usize,
    pub removed_objects: Vec<ArtifactBundleObject>,
    pub missing_artifacts: Vec<ArtifactBundleObject>,
    pub removed_manifest: bool,
    pub pruned_empty_directories: usize,
}

fn safe_bundle_entry_path(root: &Path, relative_path: &Path) -> RuntimeResult<PathBuf> {
    if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
        return Err(RuntimeError::RepoAccessDenied);
    }
    for component in relative_path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(RuntimeError::RepoAccessDenied);
        }
    }
    Ok(root.join(relative_path))
}

fn safe_bundle_manifest_path(root: &Path, manifest_path: &Path) -> RuntimeResult<PathBuf> {
    let expected = root.join(ARTIFACT_BUNDLE_MANIFEST_FILE);
    if manifest_path == expected {
        Ok(manifest_path.to_path_buf())
    } else {
        Err(RuntimeError::RepoAccessDenied)
    }
}
