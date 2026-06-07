use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;

use crate::runtime::contracts::{stable_id, ArtifactId, ArtifactKey, ArtifactView};

#[derive(Debug, Default)]
pub struct ConcurrentArtifactStore {
    by_id: DashMap<String, Arc<ConcurrentArtifact>>,
    order: Mutex<Vec<String>>,
}

impl ConcurrentArtifactStore {
    pub(crate) fn insert(&self, key: ArtifactKey, content: String) -> ArtifactId {
        self.insert_views(key, content.clone(), content)
    }

    pub(crate) fn insert_views(
        &self,
        key: ArtifactKey,
        raw_content: String,
        redacted_content: String,
    ) -> ArtifactId {
        let content_hash = stable_id(&[&redacted_content]);
        let raw_content_hash = stable_id(&[&raw_content]);
        let artifact_id = ArtifactId(format!(
            "art_{}",
            stable_id(&[&key.0, &raw_content_hash, &content_hash])
        ));
        if self
            .by_id
            .insert(
                artifact_id.0.clone(),
                Arc::new(ConcurrentArtifact {
                    artifact_id: artifact_id.clone(),
                    bytes: redacted_content.len(),
                    content_hash,
                    content: redacted_content,
                    raw_bytes: raw_content.len(),
                    raw_content_hash,
                    raw_content,
                }),
            )
            .is_none()
        {
            self.order.lock().push(artifact_id.0.clone());
        }
        artifact_id
    }

    pub fn stats(&self) -> (usize, usize) {
        let artifacts = self.by_id.iter().collect::<Vec<_>>();
        let bytes = artifacts.iter().map(|item| item.bytes).sum();
        (artifacts.len(), bytes)
    }

    pub fn get(&self, artifact_id: &ArtifactId) -> Option<ArtifactView> {
        self.by_id
            .get(&artifact_id.0)
            .map(|artifact| artifact.as_ref().view())
    }

    pub fn get_raw(&self, artifact_id: &ArtifactId) -> Option<ArtifactView> {
        self.by_id
            .get(&artifact_id.0)
            .map(|artifact| artifact.as_ref().raw_view())
    }

    pub fn list(&self) -> Vec<ArtifactView> {
        self.order
            .lock()
            .iter()
            .filter_map(|artifact_id| self.by_id.get(artifact_id))
            .map(|artifact| artifact.as_ref().view())
            .collect()
    }

    pub fn list_raw(&self) -> Vec<ArtifactView> {
        self.order
            .lock()
            .iter()
            .filter_map(|artifact_id| self.by_id.get(artifact_id))
            .map(|artifact| artifact.as_ref().raw_view())
            .collect()
    }

    pub(crate) fn merge_from(&self, other: &ConcurrentArtifactStore) {
        for redacted in other.list() {
            let raw = other
                .get_raw(&redacted.artifact_id)
                .unwrap_or_else(|| redacted.clone());
            self.insert_existing(redacted, raw);
        }
    }

    fn insert_existing(&self, redacted: ArtifactView, raw: ArtifactView) {
        let artifact_id = redacted.artifact_id.clone();
        if self
            .by_id
            .insert(
                artifact_id.0.clone(),
                Arc::new(ConcurrentArtifact {
                    artifact_id: artifact_id.clone(),
                    bytes: redacted.bytes,
                    content_hash: redacted.content_hash,
                    content: redacted.content,
                    raw_bytes: raw.bytes,
                    raw_content_hash: raw.content_hash,
                    raw_content: raw.content,
                }),
            )
            .is_none()
        {
            self.order.lock().push(artifact_id.0);
        }
    }
}

#[derive(Debug)]
struct ConcurrentArtifact {
    artifact_id: ArtifactId,
    bytes: usize,
    content_hash: String,
    content: String,
    raw_bytes: usize,
    raw_content_hash: String,
    raw_content: String,
}

impl ConcurrentArtifact {
    fn view(&self) -> ArtifactView {
        ArtifactView {
            artifact_id: self.artifact_id.clone(),
            bytes: self.bytes,
            content_hash: self.content_hash.clone(),
            content: self.content.clone(),
        }
    }

    fn raw_view(&self) -> ArtifactView {
        ArtifactView {
            artifact_id: self.artifact_id.clone(),
            bytes: self.raw_bytes,
            content_hash: self.raw_content_hash.clone(),
            content: self.raw_content.clone(),
        }
    }
}
