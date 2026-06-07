use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::runtime::contracts::{RuntimeResult, SnapshotId};

use super::ContextIndex;

pub trait ContextIndexStore: Send + Sync {
    fn put_index(&self, index: ContextIndex) -> RuntimeResult<()>;
    fn get_index(&self, snapshot_id: &SnapshotId) -> Option<Arc<ContextIndex>>;
    fn remove_index(&self, snapshot_id: &SnapshotId) -> RuntimeResult<bool>;
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
