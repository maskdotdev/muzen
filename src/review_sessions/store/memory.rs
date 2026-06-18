use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;

use super::super::{
    ReviewArtifact, ReviewCancelOptions, ReviewEvent, ReviewEventType, ReviewResult,
    ReviewSessionError, ReviewSessionId, ReviewStatus,
};
use super::{
    append_record_event, claim_limit_reached, has_valid_lease, is_claimable, rebase_events,
    retry_backoff_seconds, timestamp_from_unix_seconds, validate_worker_id, worker_lease,
    ReviewAttemptFailure, ReviewCancellationRecord, ReviewSessionRecord, ReviewSessionStore,
    ReviewWorkerClaim, ReviewWorkerClaimOptions, RunningCounts,
};

#[derive(Debug, Default)]
pub struct InMemoryReviewSessionStore {
    state: Mutex<StoreState>,
}

#[derive(Debug, Default)]
struct StoreState {
    sessions: BTreeMap<String, ReviewSessionRecord>,
    dedupe_index: BTreeMap<String, String>,
}

#[async_trait]
impl ReviewSessionStore for InMemoryReviewSessionStore {
    async fn insert(&self, record: ReviewSessionRecord) -> Result<(), ReviewSessionError> {
        let mut state = self.lock_state()?;
        let id = record.id.as_str().to_string();
        if let Some(dedupe_key) = &record.dedupe_key {
            state.dedupe_index.insert(dedupe_key.clone(), id.clone());
        }
        state.sessions.insert(id, record);
        Ok(())
    }

    async fn get(
        &self,
        id: &ReviewSessionId,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let state = self.lock_state()?;
        Ok(state.sessions.get(id.as_str()).cloned())
    }

    async fn get_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let state = self.lock_state()?;
        let Some(id) = state.dedupe_index.get(dedupe_key) else {
            return Ok(None);
        };
        Ok(state.sessions.get(id).cloned())
    }

    async fn events_after(
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

    async fn write_execution_result(
        &self,
        id: &ReviewSessionId,
        status: ReviewStatus,
        result: ReviewResult,
        events: Vec<ReviewEvent>,
        redacted_artifacts: Vec<ReviewArtifact>,
        raw_artifacts: Vec<ReviewArtifact>,
    ) -> Result<ReviewSessionRecord, ReviewSessionError> {
        let mut state = self.lock_state()?;
        let record = state
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        if record.status.is_terminal() {
            return Ok(record.clone());
        }
        let rebased_events = rebase_events(record, events);
        record.status = status;
        record.result = Some(result);
        record.events.extend(rebased_events);
        record.redacted_artifacts = redacted_artifacts;
        record.raw_artifacts = raw_artifacts;
        record.lease = None;
        record.updated_at_utc = crate::reviewer_kernel::system::timestamp_utc();
        Ok(record.clone())
    }

    async fn request_cancellation(
        &self,
        id: &ReviewSessionId,
        options: ReviewCancelOptions,
    ) -> Result<ReviewSessionRecord, ReviewSessionError> {
        let mut state = self.lock_state()?;
        let record = state
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        if record.status.is_terminal() {
            return Ok(record.clone());
        }
        let now = crate::reviewer_kernel::system::timestamp_utc();
        record.status = ReviewStatus::Cancelled;
        record.lease = None;
        record.cancellation = Some(ReviewCancellationRecord {
            reason: options.reason.clone(),
            cancelled_at_utc: now.clone(),
        });
        append_record_event(
            record,
            ReviewEventType::SessionCancelled,
            json!({ "reason": options.reason }),
            now.clone(),
        );
        record.updated_at_utc = now;
        Ok(record.clone())
    }

    async fn claim_ready(
        &self,
        options: ReviewWorkerClaimOptions,
    ) -> Result<Vec<ReviewWorkerClaim>, ReviewSessionError> {
        validate_worker_id(&options.worker_id)?;
        if options.max_sessions == 0 {
            return Ok(Vec::new());
        }

        let mut state = self.lock_state()?;
        let now = options.now_unix_seconds();
        let lease_seconds = options.lease_seconds.max(1);
        let mut running = state.running_counts(now);
        let mut claims = Vec::new();
        let ids = state.sessions.keys().cloned().collect::<Vec<_>>();

        for id in ids {
            if claims.len() >= options.max_sessions {
                break;
            }
            let Some(record) = state.sessions.get_mut(&id) else {
                continue;
            };
            if !is_claimable(record, now) {
                continue;
            }
            if claim_limit_reached(record, &running, options.concurrency) {
                continue;
            }

            record.status = ReviewStatus::Running;
            record.attempt = record.attempt.saturating_add(1).max(1);
            let lease = worker_lease(&options.worker_id, record.attempt, now, lease_seconds);
            record.lease = Some(lease.clone());
            record.updated_at_utc = lease.acquired_at_utc.clone();
            append_record_event(
                record,
                ReviewEventType::SessionClaimed,
                json!({ "attempt": record.attempt }),
                lease.acquired_at_utc.clone(),
            );
            running.add_record(record);
            claims.push(ReviewWorkerClaim {
                review_id: record.id.clone(),
                worker_id: options.worker_id.clone(),
                attempt: record.attempt,
                lease,
            });
        }

        Ok(claims)
    }

    async fn record_attempt_failure(
        &self,
        id: &ReviewSessionId,
        failure: ReviewAttemptFailure,
    ) -> Result<ReviewSessionRecord, ReviewSessionError> {
        let mut state = self.lock_state()?;
        let record = state
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        if record.status.is_terminal() {
            return Ok(record.clone());
        }

        let now = failure.now_unix_seconds();
        let now_utc = timestamp_from_unix_seconds(now);
        let max_attempts = failure.retry_policy.max_attempts.max(1);
        record.lease = None;
        record.last_error = Some(failure.error.clone());

        if record.attempt >= max_attempts {
            record.status = ReviewStatus::Failed;
            append_record_event(
                record,
                ReviewEventType::SessionFailed,
                json!({ "attempt": record.attempt, "error": failure.error }),
                now_utc.clone(),
            );
        } else {
            let backoff_seconds = retry_backoff_seconds(failure.retry_policy, record.attempt);
            record.status = ReviewStatus::Queued;
            record.run_after_unix_seconds = now.saturating_add(backoff_seconds);
            append_record_event(
                record,
                ReviewEventType::SessionQueued,
                json!({
                    "retry": true,
                    "attempt": record.attempt,
                    "runAfterUtc": timestamp_from_unix_seconds(record.run_after_unix_seconds)
                }),
                now_utc.clone(),
            );
        }
        record.updated_at_utc = now_utc;
        Ok(record.clone())
    }
}

impl InMemoryReviewSessionStore {
    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, StoreState>, ReviewSessionError> {
        self.state
            .lock()
            .map_err(|_| ReviewSessionError::Store("review session store poisoned".to_string()))
    }
}

impl StoreState {
    fn running_counts(&self, now_unix_seconds: u64) -> RunningCounts {
        let mut running = RunningCounts::default();
        for record in self.sessions.values() {
            if record.status != ReviewStatus::Running || !has_valid_lease(record, now_unix_seconds)
            {
                continue;
            }
            running.add_record(record);
        }
        running
    }
}
