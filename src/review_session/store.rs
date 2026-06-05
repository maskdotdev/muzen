use std::collections::BTreeMap;
use std::sync::Mutex;

use super::{
    ReviewArtifact, ReviewEvent, ReviewResult, ReviewSessionError, ReviewSessionId, ReviewSource,
    ReviewStatus,
};

#[derive(Debug, Clone)]
pub struct ReviewSessionRecord {
    pub id: ReviewSessionId,
    pub status: ReviewStatus,
    pub source: ReviewSource,
    pub result: Option<ReviewResult>,
    pub events: Vec<ReviewEvent>,
    pub redacted_artifacts: Vec<ReviewArtifact>,
    pub raw_artifacts: Vec<ReviewArtifact>,
    pub dedupe_key: Option<String>,
    pub created_at_utc: String,
    pub updated_at_utc: String,
}

pub trait ReviewSessionStore: Send + Sync {
    fn insert(&self, record: ReviewSessionRecord) -> Result<(), ReviewSessionError>;

    fn get(&self, id: &ReviewSessionId) -> Result<Option<ReviewSessionRecord>, ReviewSessionError>;

    fn get_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError>;

    fn append_events(
        &self,
        id: &ReviewSessionId,
        events: Vec<ReviewEvent>,
    ) -> Result<(), ReviewSessionError>;

    fn events_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewEvent>, ReviewSessionError>;

    fn write_result(
        &self,
        id: &ReviewSessionId,
        status: ReviewStatus,
        result: ReviewResult,
    ) -> Result<(), ReviewSessionError>;
}

#[derive(Debug, Default)]
pub struct InMemoryReviewSessionStore {
    state: Mutex<StoreState>,
}

#[derive(Debug, Default)]
struct StoreState {
    sessions: BTreeMap<String, ReviewSessionRecord>,
    dedupe_index: BTreeMap<String, String>,
}

impl ReviewSessionStore for InMemoryReviewSessionStore {
    fn insert(&self, record: ReviewSessionRecord) -> Result<(), ReviewSessionError> {
        let mut state = self.lock_state()?;
        let id = record.id.as_str().to_string();
        if let Some(dedupe_key) = &record.dedupe_key {
            state.dedupe_index.insert(dedupe_key.clone(), id.clone());
        }
        state.sessions.insert(id, record);
        Ok(())
    }

    fn get(&self, id: &ReviewSessionId) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let state = self.lock_state()?;
        Ok(state.sessions.get(id.as_str()).cloned())
    }

    fn get_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let state = self.lock_state()?;
        let Some(id) = state.dedupe_index.get(dedupe_key) else {
            return Ok(None);
        };
        Ok(state.sessions.get(id).cloned())
    }

    fn append_events(
        &self,
        id: &ReviewSessionId,
        events: Vec<ReviewEvent>,
    ) -> Result<(), ReviewSessionError> {
        let mut state = self.lock_state()?;
        let record = state
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        record.events.extend(events);
        record.updated_at_utc = crate::util::timestamp_utc();
        Ok(())
    }

    fn events_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewEvent>, ReviewSessionError> {
        let state = self.lock_state()?;
        let record = state
            .sessions
            .get(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        let start = after
            .and_then(|cursor| {
                record
                    .events
                    .iter()
                    .position(|event| event.cursor == cursor)
            })
            .map_or(0, |index| index + 1);
        Ok(record.events[start..].to_vec())
    }

    fn write_result(
        &self,
        id: &ReviewSessionId,
        status: ReviewStatus,
        result: ReviewResult,
    ) -> Result<(), ReviewSessionError> {
        let mut state = self.lock_state()?;
        let record = state
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        record.status = status;
        record.result = Some(result);
        record.updated_at_utc = crate::util::timestamp_utc();
        Ok(())
    }
}

impl InMemoryReviewSessionStore {
    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, StoreState>, ReviewSessionError> {
        self.state
            .lock()
            .map_err(|_| ReviewSessionError::Store("review session store poisoned".to_string()))
    }
}
