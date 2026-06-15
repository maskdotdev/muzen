use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{RuntimeError, RuntimeResult};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStoragePolicy {
    pub mode: SnapshotStorageMode,
    pub max_captured_text_bytes: usize,
    #[serde(skip, default)]
    remote_object_store: Option<Arc<dyn SnapshotObjectStore>>,
}

impl SnapshotStoragePolicy {
    pub fn memory(max_captured_text_bytes: usize) -> Self {
        Self {
            mode: SnapshotStorageMode::Memory,
            max_captured_text_bytes,
            remote_object_store: None,
        }
    }

    pub fn content_addressed_directory(
        root: impl Into<PathBuf>,
        max_captured_text_bytes: usize,
    ) -> Self {
        Self {
            mode: SnapshotStorageMode::ContentAddressedDirectory { root: root.into() },
            max_captured_text_bytes,
            remote_object_store: None,
        }
    }

    pub fn remote_object_store(
        base_uri: impl Into<String>,
        max_captured_text_bytes: usize,
        object_store: Arc<dyn SnapshotObjectStore>,
    ) -> RuntimeResult<Self> {
        Ok(Self {
            mode: SnapshotStorageMode::RemoteObjectStore {
                base_uri: normalize_remote_object_base_uri(base_uri.into())?,
            },
            max_captured_text_bytes,
            remote_object_store: Some(object_store),
        })
    }

    pub(crate) fn remote_store(&self) -> Option<Arc<dyn SnapshotObjectStore>> {
        self.remote_object_store.clone()
    }
}

impl std::fmt::Debug for SnapshotStoragePolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotStoragePolicy")
            .field("mode", &self.mode)
            .field("max_captured_text_bytes", &self.max_captured_text_bytes)
            .field(
                "has_remote_object_store",
                &self.remote_object_store.is_some(),
            )
            .finish()
    }
}

impl PartialEq for SnapshotStoragePolicy {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode && self.max_captured_text_bytes == other.max_captured_text_bytes
    }
}

impl Eq for SnapshotStoragePolicy {}

pub trait SnapshotObjectStore: Send + Sync {
    fn put_snapshot_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()>;

    fn read_snapshot_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>>;
}

impl Default for SnapshotStoragePolicy {
    fn default() -> Self {
        Self::memory(64 * 1024 * 1024)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStorageMode {
    Memory,
    ContentAddressedDirectory { root: PathBuf },
    RemoteObjectStore { base_uri: String },
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCaptureStatus {
    Captured,
    SkippedMemoryLimit,
    SkippedUnreadable,
    NotTextCandidate,
}

fn normalize_remote_object_base_uri(base_uri: String) -> RuntimeResult<String> {
    let normalized = base_uri.trim_end_matches('/').to_string();
    if normalized.is_empty()
        || !normalized.contains("://")
        || normalized.starts_with("file://")
        || normalized.chars().any(char::is_whitespace)
    {
        return Err(RuntimeError::InvalidInput(
            "remote snapshot object store requires a non-file URI base".to_string(),
        ));
    }
    Ok(normalized)
}
