use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    ReviewArtifact, ReviewCancelOptions, ReviewEvent, ReviewEventType, ReviewResult,
    ReviewSessionError, ReviewSessionId, ReviewSource, ReviewStatus,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSessionRecord {
    pub id: ReviewSessionId,
    pub workspace_id: Option<String>,
    pub user_id: Option<String>,
    pub status: ReviewStatus,
    pub source: ReviewSource,
    pub options: super::ReviewOptions,
    pub result: Option<ReviewResult>,
    pub events: Vec<ReviewEvent>,
    pub redacted_artifacts: Vec<ReviewArtifact>,
    pub raw_artifacts: Vec<ReviewArtifact>,
    pub config_snapshot: Option<super::EffectiveConfigSnapshot>,
    pub attempt: u32,
    pub run_after_unix_seconds: u64,
    pub lease: Option<ReviewWorkerLease>,
    pub cancellation: Option<ReviewCancellationRecord>,
    pub last_error: Option<String>,
    pub dedupe_key: Option<String>,
    pub created_at_utc: String,
    pub updated_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewWorkerLease {
    pub worker_id: String,
    pub attempt: u32,
    pub acquired_at_utc: String,
    pub expires_at_utc: String,
    pub expires_at_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCancellationRecord {
    #[serde(default)]
    pub reason: Option<String>,
    pub cancelled_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewWorkerClaim {
    pub review_id: ReviewSessionId,
    pub worker_id: String,
    pub attempt: u32,
    pub lease: ReviewWorkerLease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewWorkerClaimOptions {
    pub worker_id: String,
    pub max_sessions: usize,
    pub lease_seconds: u64,
    pub now_unix_seconds: Option<u64>,
    pub concurrency: ReviewWorkerConcurrencyLimits,
}

impl ReviewWorkerClaimOptions {
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            max_sessions: 1,
            lease_seconds: 60,
            now_unix_seconds: None,
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        }
    }

    fn now_unix_seconds(&self) -> u64 {
        self.now_unix_seconds.unwrap_or_else(current_unix_seconds)
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewWorkerConcurrencyLimits {
    pub max_running_global: Option<usize>,
    pub max_running_per_workspace: Option<usize>,
    pub max_running_per_user: Option<usize>,
    pub max_running_per_model_profile: Option<usize>,
    pub max_running_per_provider_profile: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLeaseExtension {
    pub worker_id: String,
    pub lease_seconds: u64,
    pub now_unix_seconds: Option<u64>,
}

impl ReviewLeaseExtension {
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            lease_seconds: 60,
            now_unix_seconds: None,
        }
    }

    fn now_unix_seconds(&self) -> u64 {
        self.now_unix_seconds.unwrap_or_else(current_unix_seconds)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff_seconds: u64,
    pub max_backoff_seconds: u64,
}

impl Default for ReviewRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_seconds: 30,
            max_backoff_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewAttemptFailure {
    pub error: String,
    pub retry_policy: ReviewRetryPolicy,
    pub now_unix_seconds: Option<u64>,
}

impl ReviewAttemptFailure {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            retry_policy: ReviewRetryPolicy::default(),
            now_unix_seconds: None,
        }
    }

    fn now_unix_seconds(&self) -> u64 {
        self.now_unix_seconds.unwrap_or_else(current_unix_seconds)
    }
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

    fn write_execution_result(
        &self,
        id: &ReviewSessionId,
        status: ReviewStatus,
        result: ReviewResult,
        events: Vec<ReviewEvent>,
        redacted_artifacts: Vec<ReviewArtifact>,
        raw_artifacts: Vec<ReviewArtifact>,
    ) -> Result<ReviewSessionRecord, ReviewSessionError>;

    fn request_cancellation(
        &self,
        id: &ReviewSessionId,
        options: ReviewCancelOptions,
    ) -> Result<ReviewSessionRecord, ReviewSessionError>;

    fn claim_ready(
        &self,
        options: ReviewWorkerClaimOptions,
    ) -> Result<Vec<ReviewWorkerClaim>, ReviewSessionError>;

    fn extend_lease(
        &self,
        id: &ReviewSessionId,
        options: ReviewLeaseExtension,
    ) -> Result<ReviewWorkerLease, ReviewSessionError>;

    fn record_attempt_failure(
        &self,
        id: &ReviewSessionId,
        failure: ReviewAttemptFailure,
    ) -> Result<ReviewSessionRecord, ReviewSessionError>;
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
        record.lease = None;
        record.updated_at_utc = crate::util::timestamp_utc();
        Ok(())
    }

    fn write_execution_result(
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
        let rebased_events = rebase_events(record, events);
        record.status = status;
        record.result = Some(result);
        record.events.extend(rebased_events);
        record.redacted_artifacts = redacted_artifacts;
        record.raw_artifacts = raw_artifacts;
        record.lease = None;
        record.updated_at_utc = crate::util::timestamp_utc();
        Ok(record.clone())
    }

    fn request_cancellation(
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
        let now = crate::util::timestamp_utc();
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

    fn claim_ready(
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

    fn extend_lease(
        &self,
        id: &ReviewSessionId,
        options: ReviewLeaseExtension,
    ) -> Result<ReviewWorkerLease, ReviewSessionError> {
        validate_worker_id(&options.worker_id)?;
        let mut state = self.lock_state()?;
        let record = state
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        let Some(existing) = &record.lease else {
            return Err(ReviewSessionError::Store(format!(
                "review session {id} does not have an active lease"
            )));
        };
        if existing.worker_id != options.worker_id {
            return Err(ReviewSessionError::Store(format!(
                "review session {id} is leased by another worker"
            )));
        }
        if record.status != ReviewStatus::Running {
            return Err(ReviewSessionError::Store(format!(
                "review session {id} is not running"
            )));
        }
        let now = options.now_unix_seconds();
        let lease = worker_lease(
            &options.worker_id,
            existing.attempt,
            now,
            options.lease_seconds.max(1),
        );
        record.lease = Some(lease.clone());
        record.updated_at_utc = lease.acquired_at_utc.clone();
        Ok(lease)
    }

    fn record_attempt_failure(
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

#[derive(Debug, Default)]
struct RunningCounts {
    global: usize,
    workspaces: BTreeMap<String, usize>,
    users: BTreeMap<String, usize>,
    model_profiles: BTreeMap<String, usize>,
    provider_profiles: BTreeMap<String, usize>,
}

impl RunningCounts {
    fn add_record(&mut self, record: &ReviewSessionRecord) {
        self.global += 1;
        increment_key(&mut self.workspaces, record.workspace_id.as_deref());
        increment_key(&mut self.users, record.user_id.as_deref());
        increment_key(&mut self.model_profiles, model_profile_key(record));
        increment_key(&mut self.provider_profiles, provider_profile_key(record));
    }
}

fn validate_worker_id(worker_id: &str) -> Result<(), ReviewSessionError> {
    if worker_id.trim().is_empty() {
        return Err(ReviewSessionError::Store(
            "worker id cannot be empty".to_string(),
        ));
    }
    Ok(())
}

fn is_claimable(record: &ReviewSessionRecord, now_unix_seconds: u64) -> bool {
    if record.status.is_terminal() || record.run_after_unix_seconds > now_unix_seconds {
        return false;
    }
    match record.status {
        ReviewStatus::Created | ReviewStatus::Queued => true,
        ReviewStatus::Running => !has_valid_lease(record, now_unix_seconds),
        ReviewStatus::Completed | ReviewStatus::Failed | ReviewStatus::Cancelled => false,
    }
}

fn has_valid_lease(record: &ReviewSessionRecord, now_unix_seconds: u64) -> bool {
    record
        .lease
        .as_ref()
        .is_some_and(|lease| lease.expires_at_unix_seconds > now_unix_seconds)
}

fn claim_limit_reached(
    record: &ReviewSessionRecord,
    running: &RunningCounts,
    limits: ReviewWorkerConcurrencyLimits,
) -> bool {
    limit_reached(running.global, limits.max_running_global)
        || keyed_limit_reached(
            record.workspace_id.as_deref(),
            &running.workspaces,
            limits.max_running_per_workspace,
        )
        || keyed_limit_reached(
            record.user_id.as_deref(),
            &running.users,
            limits.max_running_per_user,
        )
        || keyed_limit_reached(
            model_profile_key(record),
            &running.model_profiles,
            limits.max_running_per_model_profile,
        )
        || keyed_limit_reached(
            provider_profile_key(record),
            &running.provider_profiles,
            limits.max_running_per_provider_profile,
        )
}

fn limit_reached(current: usize, limit: Option<usize>) -> bool {
    let Some(limit) = limit else {
        return false;
    };
    current >= limit
}

fn keyed_limit_reached(
    key: Option<&str>,
    counts: &BTreeMap<String, usize>,
    limit: Option<usize>,
) -> bool {
    let Some(limit) = limit else {
        return false;
    };
    let Some(key) = key else {
        return false;
    };
    counts.get(key).copied().unwrap_or(0) >= limit
}

fn increment_key(counts: &mut BTreeMap<String, usize>, key: Option<&str>) {
    if let Some(key) = key {
        *counts.entry(key.to_string()).or_insert(0) += 1;
    }
}

fn model_profile_key(record: &ReviewSessionRecord) -> Option<&str> {
    record
        .config_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.model_profile.as_ref())
        .map(|profile| profile.id.as_str())
}

fn provider_profile_key(record: &ReviewSessionRecord) -> Option<&str> {
    record
        .config_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.provider_profile.as_ref())
        .map(|profile| profile.id.as_str())
}

fn worker_lease(
    worker_id: &str,
    attempt: u32,
    now_unix_seconds: u64,
    lease_seconds: u64,
) -> ReviewWorkerLease {
    let expires_at_unix_seconds = now_unix_seconds.saturating_add(lease_seconds);
    ReviewWorkerLease {
        worker_id: worker_id.to_string(),
        attempt,
        acquired_at_utc: timestamp_from_unix_seconds(now_unix_seconds),
        expires_at_utc: timestamp_from_unix_seconds(expires_at_unix_seconds),
        expires_at_unix_seconds,
    }
}

fn append_record_event(
    record: &mut ReviewSessionRecord,
    event_type: ReviewEventType,
    payload: serde_json::Value,
    timestamp_utc: String,
) {
    record.events.push(ReviewEvent {
        cursor: (record.events.len() + 1).to_string(),
        event_type,
        review_id: record.id.clone(),
        timestamp_utc,
        payload,
    });
}

fn rebase_events(record: &ReviewSessionRecord, events: Vec<ReviewEvent>) -> Vec<ReviewEvent> {
    let mut next_cursor = record.events.len();
    events
        .into_iter()
        .map(|mut event| {
            next_cursor += 1;
            event.cursor = next_cursor.to_string();
            event.review_id = record.id.clone();
            event
        })
        .collect()
}

fn retry_backoff_seconds(policy: ReviewRetryPolicy, attempt: u32) -> u64 {
    let initial = policy.initial_backoff_seconds.max(1);
    let max = policy.max_backoff_seconds.max(initial);
    let shift = attempt.saturating_sub(1).min(31);
    let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    initial.saturating_mul(multiplier).min(max)
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn timestamp_from_unix_seconds(seconds: u64) -> String {
    format!("{seconds}.000000000Z")
}
