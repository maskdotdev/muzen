use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeResult};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactViewMode {
    Redacted,
    Raw,
}

pub trait RemoteArtifactObjectClient: Send + Sync {
    fn put_remote_artifact_object(&self, uri: &str, bytes: Vec<u8>) -> RuntimeResult<()>;

    fn read_remote_artifact_object(&self, uri: &str) -> RuntimeResult<Option<Vec<u8>>>;

    fn remove_remote_artifact_object(&self, uri: &str) -> RuntimeResult<bool>;
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

fn artifact_view_name(view: ArtifactViewMode) -> &'static str {
    match view {
        ArtifactViewMode::Redacted => "redacted",
        ArtifactViewMode::Raw => "raw",
    }
}
