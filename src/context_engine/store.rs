use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::runtime::contracts::{RuntimeError, RuntimeResult, SnapshotId};

use super::{ContextIndex, ContextLearning};

pub trait ContextIndexStore: Send + Sync {
    fn put_index(&self, index: ContextIndex) -> RuntimeResult<()>;
    fn get_index(&self, snapshot_id: &SnapshotId) -> Option<Arc<ContextIndex>>;
    fn remove_index(&self, snapshot_id: &SnapshotId) -> RuntimeResult<bool>;
}

pub trait ContextLearningStore: Send + Sync {
    fn put_learning(&self, learning: ContextLearning) -> RuntimeResult<()>;
    fn get_learning(&self, learning_id: &str) -> Option<ContextLearning>;
    fn update_learning(
        &self,
        learning_id: &str,
        update: &mut dyn FnMut(&mut ContextLearning) -> RuntimeResult<()>,
    ) -> RuntimeResult<ContextLearning>;
    fn list_learnings(&self) -> Vec<ContextLearning>;
}

#[derive(Debug, Default)]
pub struct InMemoryContextIndexStore {
    indexes: Mutex<HashMap<SnapshotId, Arc<ContextIndex>>>,
}

impl InMemoryContextIndexStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContextIndexStore for InMemoryContextIndexStore {
    fn put_index(&self, index: ContextIndex) -> RuntimeResult<()> {
        self.indexes
            .lock()
            .expect("context index store poisoned")
            .insert(index.snapshot_id.clone(), Arc::new(index));
        Ok(())
    }

    fn get_index(&self, snapshot_id: &SnapshotId) -> Option<Arc<ContextIndex>> {
        self.indexes
            .lock()
            .expect("context index store poisoned")
            .get(snapshot_id)
            .cloned()
    }

    fn remove_index(&self, snapshot_id: &SnapshotId) -> RuntimeResult<bool> {
        Ok(self
            .indexes
            .lock()
            .expect("context index store poisoned")
            .remove(snapshot_id)
            .is_some())
    }
}

#[derive(Debug, Default)]
pub struct InMemoryContextLearningStore {
    learnings: Mutex<HashMap<String, ContextLearning>>,
}

impl InMemoryContextLearningStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContextLearningStore for InMemoryContextLearningStore {
    fn put_learning(&self, learning: ContextLearning) -> RuntimeResult<()> {
        self.learnings
            .lock()
            .expect("context learning store poisoned")
            .insert(learning.id.clone(), learning);
        Ok(())
    }

    fn get_learning(&self, learning_id: &str) -> Option<ContextLearning> {
        self.learnings
            .lock()
            .expect("context learning store poisoned")
            .get(learning_id)
            .cloned()
    }

    fn update_learning(
        &self,
        learning_id: &str,
        update: &mut dyn FnMut(&mut ContextLearning) -> RuntimeResult<()>,
    ) -> RuntimeResult<ContextLearning> {
        let mut learnings = self
            .learnings
            .lock()
            .expect("context learning store poisoned");
        let learning = learnings
            .get_mut(learning_id)
            .ok_or_else(|| RuntimeError::InvalidInput("context learning not found".to_string()))?;
        update(learning)?;
        Ok(learning.clone())
    }

    fn list_learnings(&self) -> Vec<ContextLearning> {
        self.learnings
            .lock()
            .expect("context learning store poisoned")
            .values()
            .cloned()
            .collect()
    }
}

#[derive(Debug)]
pub struct FileContextLearningStore {
    path: PathBuf,
    learnings: Mutex<HashMap<String, ContextLearning>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextLearningStoreFile {
    schema_version: String,
    learnings: HashMap<String, ContextLearning>,
}

impl FileContextLearningStore {
    pub fn open(path: impl AsRef<Path>) -> RuntimeResult<Self> {
        let path = path.as_ref().to_path_buf();
        let mut learnings = if path.exists() {
            let contents = std::fs::read_to_string(&path).map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "failed to read context learning store {}: {error}",
                    path.display()
                ))
            })?;
            if contents.trim().is_empty() {
                HashMap::new()
            } else {
                serde_json::from_str::<ContextLearningStoreFile>(&contents)
                    .map_err(|error| {
                        RuntimeError::InvalidInput(format!(
                            "invalid context learning store {}: {error}",
                            path.display()
                        ))
                    })?
                    .learnings
            }
        } else {
            HashMap::new()
        };
        let original_len = learnings.len();
        learnings.retain(|_, learning| !learning_is_expired_for_retention(learning));
        let pruned_expired = learnings.len() != original_len;
        let store = Self {
            path,
            learnings: Mutex::new(learnings),
        };
        if pruned_expired {
            store.persist(
                &store
                    .learnings
                    .lock()
                    .expect("context learning store poisoned"),
            )?;
        }
        Ok(store)
    }

    fn persist(&self, learnings: &HashMap<String, ContextLearning>) -> RuntimeResult<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "failed to create context learning store dir {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let file = ContextLearningStoreFile {
            schema_version: "muzen.context_learning_store.v1".to_string(),
            learnings: learnings.clone(),
        };
        let contents = serde_json::to_string_pretty(&file).map_err(|error| {
            RuntimeError::InvalidInput(format!("failed to encode context learning store: {error}"))
        })?;
        std::fs::write(&self.path, contents).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "failed to write context learning store {}: {error}",
                self.path.display()
            ))
        })
    }
}

fn learning_is_expired_for_retention(learning: &ContextLearning) -> bool {
    let Some(expires_at) = &learning.expires_at_utc else {
        return false;
    };
    let Ok(expires_at) = expires_at
        .trim_end_matches('Z')
        .split('.')
        .next()
        .unwrap_or(expires_at)
        .parse::<u64>()
    else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    expires_at <= now
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_engine::{
        ContextLearningScope, ContextLearningSource, ContextLearningStatus,
    };

    #[test]
    fn file_learning_store_prunes_expired_records_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context-learnings.json");
        let store = FileContextLearningStore::open(&path).unwrap();
        store
            .put_learning(ContextLearning {
                id: "expired".to_string(),
                snapshot_id: SnapshotId("snapshot".to_string()),
                source: ContextLearningSource::ManualRule,
                status: ContextLearningStatus::Approved,
                scope: ContextLearningScope::Repository,
                evidence_ids: Vec::new(),
                summary: "expired learning".to_string(),
                created_at_utc: "1".to_string(),
                expires_at_utc: Some("1".to_string()),
            })
            .unwrap();
        assert_eq!(store.list_learnings().len(), 1);

        let reopened = FileContextLearningStore::open(&path).unwrap();
        assert!(reopened.list_learnings().is_empty());
        let persisted: ContextLearningStoreFile =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(persisted.learnings.is_empty());
    }
}

impl ContextLearningStore for FileContextLearningStore {
    fn put_learning(&self, learning: ContextLearning) -> RuntimeResult<()> {
        let mut learnings = self
            .learnings
            .lock()
            .expect("context learning store poisoned");
        learnings.insert(learning.id.clone(), learning);
        self.persist(&learnings)
    }

    fn get_learning(&self, learning_id: &str) -> Option<ContextLearning> {
        self.learnings
            .lock()
            .expect("context learning store poisoned")
            .get(learning_id)
            .cloned()
    }

    fn update_learning(
        &self,
        learning_id: &str,
        update: &mut dyn FnMut(&mut ContextLearning) -> RuntimeResult<()>,
    ) -> RuntimeResult<ContextLearning> {
        let mut learnings = self
            .learnings
            .lock()
            .expect("context learning store poisoned");
        let updated = {
            let learning = learnings.get_mut(learning_id).ok_or_else(|| {
                RuntimeError::InvalidInput("context learning not found".to_string())
            })?;
            update(learning)?;
            learning.clone()
        };
        self.persist(&learnings)?;
        Ok(updated)
    }

    fn list_learnings(&self) -> Vec<ContextLearning> {
        self.learnings
            .lock()
            .expect("context learning store poisoned")
            .values()
            .cloned()
            .collect()
    }
}
