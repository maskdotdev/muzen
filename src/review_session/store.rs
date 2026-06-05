use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postgres::{Client, GenericClient, NoTls, Row};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

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
    #[serde(default)]
    pub logs: Vec<ReviewLogEntry>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewLogEntry {
    pub cursor: String,
    pub review_id: ReviewSessionId,
    pub timestamp_utc: String,
    pub stream: ReviewLogStream,
    pub message: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl ReviewLogEntry {
    pub fn new(
        review_id: ReviewSessionId,
        stream: ReviewLogStream,
        message: impl Into<String>,
    ) -> Self {
        Self {
            cursor: String::new(),
            review_id,
            timestamp_utc: crate::util::timestamp_utc(),
            stream,
            message: message.into(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLogStream {
    System,
    Worker,
    Agent,
    ToolStdout,
    ToolStderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLogRedactionPolicy {
    pub secrets: Vec<String>,
    pub sensitive_keys: Vec<String>,
}

impl ReviewLogRedactionPolicy {
    pub fn new(secrets: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            secrets: secrets
                .into_iter()
                .map(Into::into)
                .filter(|secret: &String| !secret.is_empty())
                .collect(),
            ..Self::default()
        }
    }

    pub fn redact_entry(&self, mut entry: ReviewLogEntry) -> ReviewLogEntry {
        entry.message = self.redact_string(&entry.message);
        entry.metadata = entry
            .metadata
            .into_iter()
            .map(|(key, value)| {
                let redacted = if self.is_sensitive_key(&key) {
                    Value::String("[redacted]".to_string())
                } else {
                    self.redact_value(value)
                };
                (key, redacted)
            })
            .collect();
        entry
    }

    fn redact_value(&self, value: Value) -> Value {
        match value {
            Value::String(value) => Value::String(self.redact_string(&value)),
            Value::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(|value| self.redact_value(value))
                    .collect(),
            ),
            Value::Object(values) => {
                let mut redacted = Map::new();
                for (key, value) in values {
                    let value = if self.is_sensitive_key(&key) {
                        Value::String("[redacted]".to_string())
                    } else {
                        self.redact_value(value)
                    };
                    redacted.insert(key, value);
                }
                Value::Object(redacted)
            }
            value => value,
        }
    }

    fn redact_string(&self, value: &str) -> String {
        let mut redacted = value.to_string();
        for secret in &self.secrets {
            redacted = redacted.replace(secret, "[redacted]");
        }
        redacted
    }

    fn is_sensitive_key(&self, key: &str) -> bool {
        let normalized = normalize_sensitive_key(key);
        self.sensitive_keys
            .iter()
            .any(|sensitive| normalize_sensitive_key(sensitive) == normalized)
    }
}

impl Default for ReviewLogRedactionPolicy {
    fn default() -> Self {
        Self {
            secrets: Vec::new(),
            sensitive_keys: vec![
                "apiKey".to_string(),
                "api_key".to_string(),
                "token".to_string(),
                "accessToken".to_string(),
                "authorization".to_string(),
                "secret".to_string(),
            ],
        }
    }
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

    fn append_logs(
        &self,
        id: &ReviewSessionId,
        logs: Vec<ReviewLogEntry>,
        redaction: ReviewLogRedactionPolicy,
    ) -> Result<(), ReviewSessionError>;

    fn logs_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewLogEntry>, ReviewSessionError>;

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

    fn append_logs(
        &self,
        id: &ReviewSessionId,
        logs: Vec<ReviewLogEntry>,
        redaction: ReviewLogRedactionPolicy,
    ) -> Result<(), ReviewSessionError> {
        let mut state = self.lock_state()?;
        let record = state
            .sessions
            .get_mut(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        let start = record.logs.len();
        let review_id = record.id.clone();
        let rebased = logs
            .into_iter()
            .enumerate()
            .map(|(index, mut log)| {
                log.cursor = (start + index + 1).to_string();
                log.review_id = review_id.clone();
                if log.timestamp_utc.trim().is_empty() {
                    log.timestamp_utc = crate::util::timestamp_utc();
                }
                redaction.redact_entry(log)
            })
            .collect::<Vec<_>>();
        record.logs.extend(rebased);
        record.updated_at_utc = crate::util::timestamp_utc();
        Ok(())
    }

    fn logs_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewLogEntry>, ReviewSessionError> {
        let state = self.lock_state()?;
        let record = state
            .sessions
            .get(id.as_str())
            .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))?;
        let start = after
            .and_then(|cursor| record.logs.iter().position(|log| log.cursor == cursor))
            .map_or(0, |index| index + 1);
        Ok(record.logs[start..].to_vec())
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

pub struct PostgresReviewSessionStore {
    client: Mutex<Client>,
}

impl std::fmt::Debug for PostgresReviewSessionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresReviewSessionStore")
            .finish_non_exhaustive()
    }
}

impl PostgresReviewSessionStore {
    pub fn connect(database_url: &str) -> Result<Self, ReviewSessionError> {
        let client = Client::connect(database_url, NoTls).map_err(|error| {
            ReviewSessionError::Store(format!("postgres connect failed: {error}"))
        })?;
        let store = Self {
            client: Mutex::new(client),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn migrate(&self) -> Result<(), ReviewSessionError> {
        let mut client = self.lock_client()?;
        client
            .batch_execute(REVIEW_SESSION_SCHEMA_SQL)
            .map_err(postgres_store_error)?;
        Ok(())
    }

    fn lock_client(&self) -> Result<std::sync::MutexGuard<'_, Client>, ReviewSessionError> {
        self.client
            .lock()
            .map_err(|_| ReviewSessionError::Store("postgres review store poisoned".to_string()))
    }
}

impl ReviewSessionStore for PostgresReviewSessionStore {
    fn insert(&self, record: ReviewSessionRecord) -> Result<(), ReviewSessionError> {
        let mut client = self.lock_client()?;
        upsert_postgres_record(&mut *client, &record)
    }

    fn get(&self, id: &ReviewSessionId) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let mut client = self.lock_client()?;
        let sql =
            format!("SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions WHERE id = $1");
        let row = client
            .query_opt(&sql, &[&id.as_str()])
            .map_err(postgres_store_error)?;
        row.map(|row| postgres_row_to_record(&row)).transpose()
    }

    fn get_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let mut client = self.lock_client()?;
        let sql = format!(
            "SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions WHERE dedupe_key = $1"
        );
        let row = client
            .query_opt(&sql, &[&dedupe_key])
            .map_err(postgres_store_error)?;
        row.map(|row| postgres_row_to_record(&row)).transpose()
    }

    fn append_events(
        &self,
        id: &ReviewSessionId,
        events: Vec<ReviewEvent>,
    ) -> Result<(), ReviewSessionError> {
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let mut record = postgres_record_for_update(&mut transaction, id)?;
        record.events.extend(events);
        record.updated_at_utc = crate::util::timestamp_utc();
        upsert_postgres_record(&mut transaction, &record)?;
        transaction.commit().map_err(postgres_store_error)?;
        Ok(())
    }

    fn events_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewEvent>, ReviewSessionError> {
        let Some(record) = self.get(id)? else {
            return Err(ReviewSessionError::Store(format!(
                "unknown review session {id}"
            )));
        };
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

    fn append_logs(
        &self,
        id: &ReviewSessionId,
        logs: Vec<ReviewLogEntry>,
        redaction: ReviewLogRedactionPolicy,
    ) -> Result<(), ReviewSessionError> {
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let mut record = postgres_record_for_update(&mut transaction, id)?;
        let start = record.logs.len();
        let review_id = record.id.clone();
        record
            .logs
            .extend(logs.into_iter().enumerate().map(|(index, mut log)| {
                log.cursor = (start + index + 1).to_string();
                log.review_id = review_id.clone();
                if log.timestamp_utc.trim().is_empty() {
                    log.timestamp_utc = crate::util::timestamp_utc();
                }
                redaction.redact_entry(log)
            }));
        record.updated_at_utc = crate::util::timestamp_utc();
        upsert_postgres_record(&mut transaction, &record)?;
        transaction.commit().map_err(postgres_store_error)?;
        Ok(())
    }

    fn logs_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewLogEntry>, ReviewSessionError> {
        let Some(record) = self.get(id)? else {
            return Err(ReviewSessionError::Store(format!(
                "unknown review session {id}"
            )));
        };
        let start = after
            .and_then(|cursor| record.logs.iter().position(|log| log.cursor == cursor))
            .map_or(0, |index| index + 1);
        Ok(record.logs[start..].to_vec())
    }

    fn write_result(
        &self,
        id: &ReviewSessionId,
        status: ReviewStatus,
        result: ReviewResult,
    ) -> Result<(), ReviewSessionError> {
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let mut record = postgres_record_for_update(&mut transaction, id)?;
        record.status = status;
        record.result = Some(result);
        record.lease = None;
        record.updated_at_utc = crate::util::timestamp_utc();
        upsert_postgres_record(&mut transaction, &record)?;
        transaction.commit().map_err(postgres_store_error)?;
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
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let mut record = postgres_record_for_update(&mut transaction, id)?;
        let rebased_events = rebase_events(&record, events);
        record.status = status;
        record.result = Some(result);
        record.events.extend(rebased_events);
        record.redacted_artifacts = redacted_artifacts;
        record.raw_artifacts = raw_artifacts;
        record.lease = None;
        record.updated_at_utc = crate::util::timestamp_utc();
        upsert_postgres_record(&mut transaction, &record)?;
        transaction.commit().map_err(postgres_store_error)?;
        Ok(record)
    }

    fn request_cancellation(
        &self,
        id: &ReviewSessionId,
        options: ReviewCancelOptions,
    ) -> Result<ReviewSessionRecord, ReviewSessionError> {
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let mut record = postgres_record_for_update(&mut transaction, id)?;
        if !record.status.is_terminal() {
            let now = crate::util::timestamp_utc();
            record.status = ReviewStatus::Cancelled;
            record.lease = None;
            record.cancellation = Some(ReviewCancellationRecord {
                reason: options.reason.clone(),
                cancelled_at_utc: now.clone(),
            });
            append_record_event(
                &mut record,
                ReviewEventType::SessionCancelled,
                json!({ "reason": options.reason }),
                now.clone(),
            );
            record.updated_at_utc = now;
            upsert_postgres_record(&mut transaction, &record)?;
        }
        transaction.commit().map_err(postgres_store_error)?;
        Ok(record)
    }

    fn claim_ready(
        &self,
        options: ReviewWorkerClaimOptions,
    ) -> Result<Vec<ReviewWorkerClaim>, ReviewSessionError> {
        validate_worker_id(&options.worker_id)?;
        if options.max_sessions == 0 {
            return Ok(Vec::new());
        }

        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let now = options.now_unix_seconds();
        let now_i64 = u64_to_i64(now, "now_unix_seconds")?;
        let running_rows = transaction
            .query(
                &format!(
                    "SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions \
                     WHERE status = 'running' \
                       AND COALESCE((lease->>'expiresAtUnixSeconds')::bigint, 0) > $1 \
                     FOR UPDATE"
                ),
                &[&now_i64],
            )
            .map_err(postgres_store_error)?;
        let mut running = RunningCounts::default();
        for row in running_rows {
            running.add_record(&postgres_row_to_record(&row)?);
        }

        let max_sessions = usize_to_i64(options.max_sessions, "max_sessions")?;
        let candidate_rows = transaction
            .query(
                &format!(
                    "SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions \
                     WHERE status IN ('created', 'queued', 'running') \
                       AND run_after_unix_seconds <= $1 \
                       AND (status <> 'running' \
                            OR COALESCE((lease->>'expiresAtUnixSeconds')::bigint, 0) <= $1) \
                     ORDER BY run_after_unix_seconds ASC, created_at_utc ASC, id ASC \
                     FOR UPDATE SKIP LOCKED \
                     LIMIT $2"
                ),
                &[&now_i64, &max_sessions],
            )
            .map_err(postgres_store_error)?;

        let lease_seconds = options.lease_seconds.max(1);
        let mut claims = Vec::new();
        for row in candidate_rows {
            if claims.len() >= options.max_sessions {
                break;
            }
            let mut record = postgres_row_to_record(&row)?;
            if !is_claimable(&record, now)
                || claim_limit_reached(&record, &running, options.concurrency)
            {
                continue;
            }
            record.status = ReviewStatus::Running;
            record.attempt = record.attempt.saturating_add(1).max(1);
            let lease = worker_lease(&options.worker_id, record.attempt, now, lease_seconds);
            record.lease = Some(lease.clone());
            record.updated_at_utc = lease.acquired_at_utc.clone();
            let attempt = record.attempt;
            append_record_event(
                &mut record,
                ReviewEventType::SessionClaimed,
                json!({ "attempt": attempt }),
                lease.acquired_at_utc.clone(),
            );
            upsert_postgres_record(&mut transaction, &record)?;
            running.add_record(&record);
            claims.push(ReviewWorkerClaim {
                review_id: record.id,
                worker_id: options.worker_id.clone(),
                attempt: record.attempt,
                lease,
            });
        }
        transaction.commit().map_err(postgres_store_error)?;
        Ok(claims)
    }

    fn extend_lease(
        &self,
        id: &ReviewSessionId,
        options: ReviewLeaseExtension,
    ) -> Result<ReviewWorkerLease, ReviewSessionError> {
        validate_worker_id(&options.worker_id)?;
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let mut record = postgres_record_for_update(&mut transaction, id)?;
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
        upsert_postgres_record(&mut transaction, &record)?;
        transaction.commit().map_err(postgres_store_error)?;
        Ok(lease)
    }

    fn record_attempt_failure(
        &self,
        id: &ReviewSessionId,
        failure: ReviewAttemptFailure,
    ) -> Result<ReviewSessionRecord, ReviewSessionError> {
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_store_error)?;
        let mut record = postgres_record_for_update(&mut transaction, id)?;
        if !record.status.is_terminal() {
            let now = failure.now_unix_seconds();
            let now_utc = timestamp_from_unix_seconds(now);
            let max_attempts = failure.retry_policy.max_attempts.max(1);
            record.lease = None;
            record.last_error = Some(failure.error.clone());
            if record.attempt >= max_attempts {
                record.status = ReviewStatus::Failed;
                let attempt = record.attempt;
                let error = failure.error.clone();
                append_record_event(
                    &mut record,
                    ReviewEventType::SessionFailed,
                    json!({ "attempt": attempt, "error": error }),
                    now_utc.clone(),
                );
            } else {
                let backoff_seconds = retry_backoff_seconds(failure.retry_policy, record.attempt);
                record.status = ReviewStatus::Queued;
                record.run_after_unix_seconds = now.saturating_add(backoff_seconds);
                let attempt = record.attempt;
                let run_after_utc = timestamp_from_unix_seconds(record.run_after_unix_seconds);
                append_record_event(
                    &mut record,
                    ReviewEventType::SessionQueued,
                    json!({
                        "retry": true,
                        "attempt": attempt,
                        "runAfterUtc": run_after_utc
                    }),
                    now_utc.clone(),
                );
            }
            record.updated_at_utc = now_utc;
            upsert_postgres_record(&mut transaction, &record)?;
        }
        transaction.commit().map_err(postgres_store_error)?;
        Ok(record)
    }
}

const REVIEW_SESSION_COLUMNS: &str = "id, workspace_id, user_id, status, source, options, result, events, logs, redacted_artifacts, raw_artifacts, config_snapshot, attempt, run_after_unix_seconds, lease, cancellation, last_error, dedupe_key, created_at_utc, updated_at_utc";

const REVIEW_SESSION_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS muzen_review_sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT,
    user_id TEXT,
    status TEXT NOT NULL,
    source JSONB NOT NULL,
    options JSONB NOT NULL,
    result JSONB,
    events JSONB NOT NULL DEFAULT '[]'::jsonb,
    logs JSONB NOT NULL DEFAULT '[]'::jsonb,
    redacted_artifacts JSONB NOT NULL DEFAULT '[]'::jsonb,
    raw_artifacts JSONB NOT NULL DEFAULT '[]'::jsonb,
    config_snapshot JSONB,
    attempt INTEGER NOT NULL DEFAULT 0,
    run_after_unix_seconds BIGINT NOT NULL DEFAULT 0,
    lease JSONB,
    cancellation JSONB,
    last_error TEXT,
    dedupe_key TEXT UNIQUE,
    created_at_utc TEXT NOT NULL,
    updated_at_utc TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS muzen_review_sessions_ready_idx
    ON muzen_review_sessions (status, run_after_unix_seconds, created_at_utc, id);

CREATE INDEX IF NOT EXISTS muzen_review_sessions_workspace_idx
    ON muzen_review_sessions (workspace_id, status);
"#;

fn postgres_record_for_update(
    client: &mut impl GenericClient,
    id: &ReviewSessionId,
) -> Result<ReviewSessionRecord, ReviewSessionError> {
    let sql = format!(
        "SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions WHERE id = $1 FOR UPDATE"
    );
    client
        .query_opt(&sql, &[&id.as_str()])
        .map_err(postgres_store_error)?
        .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))
        .and_then(|row| postgres_row_to_record(&row))
}

fn upsert_postgres_record(
    client: &mut impl GenericClient,
    record: &ReviewSessionRecord,
) -> Result<(), ReviewSessionError> {
    let id = record.id.as_str().to_string();
    let status = serialize_status(record.status)?;
    let source = serialize_json(&record.source, "source")?;
    let options = serialize_json(&record.options, "options")?;
    let result = serialize_optional_json(&record.result, "result")?;
    let events = serialize_json(&record.events, "events")?;
    let logs = serialize_json(&record.logs, "logs")?;
    let redacted_artifacts = serialize_json(&record.redacted_artifacts, "redacted_artifacts")?;
    let raw_artifacts = serialize_json(&record.raw_artifacts, "raw_artifacts")?;
    let config_snapshot = serialize_optional_json(&record.config_snapshot, "config_snapshot")?;
    let attempt = u32_to_i32(record.attempt, "attempt")?;
    let run_after = u64_to_i64(record.run_after_unix_seconds, "run_after_unix_seconds")?;
    let lease = serialize_optional_json(&record.lease, "lease")?;
    let cancellation = serialize_optional_json(&record.cancellation, "cancellation")?;
    client
        .execute(
            "INSERT INTO muzen_review_sessions (
                id, workspace_id, user_id, status, source, options, result, events, logs,
                redacted_artifacts, raw_artifacts, config_snapshot, attempt,
                run_after_unix_seconds, lease, cancellation, last_error, dedupe_key,
                created_at_utc, updated_at_utc
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                $11, $12, $13, $14, $15, $16, $17, $18, $19, $20
             )
             ON CONFLICT (id) DO UPDATE SET
                workspace_id = EXCLUDED.workspace_id,
                user_id = EXCLUDED.user_id,
                status = EXCLUDED.status,
                source = EXCLUDED.source,
                options = EXCLUDED.options,
                result = EXCLUDED.result,
                events = EXCLUDED.events,
                logs = EXCLUDED.logs,
                redacted_artifacts = EXCLUDED.redacted_artifacts,
                raw_artifacts = EXCLUDED.raw_artifacts,
                config_snapshot = EXCLUDED.config_snapshot,
                attempt = EXCLUDED.attempt,
                run_after_unix_seconds = EXCLUDED.run_after_unix_seconds,
                lease = EXCLUDED.lease,
                cancellation = EXCLUDED.cancellation,
                last_error = EXCLUDED.last_error,
                dedupe_key = EXCLUDED.dedupe_key,
                updated_at_utc = EXCLUDED.updated_at_utc",
            &[
                &id,
                &record.workspace_id,
                &record.user_id,
                &status,
                &source,
                &options,
                &result,
                &events,
                &logs,
                &redacted_artifacts,
                &raw_artifacts,
                &config_snapshot,
                &attempt,
                &run_after,
                &lease,
                &cancellation,
                &record.last_error,
                &record.dedupe_key,
                &record.created_at_utc,
                &record.updated_at_utc,
            ],
        )
        .map_err(postgres_store_error)?;
    Ok(())
}

fn postgres_row_to_record(row: &Row) -> Result<ReviewSessionRecord, ReviewSessionError> {
    let id: String = row.get("id");
    let status: String = row.get("status");
    let attempt: i32 = row.get("attempt");
    let run_after: i64 = row.get("run_after_unix_seconds");
    Ok(ReviewSessionRecord {
        id: ReviewSessionId::new(id)?,
        workspace_id: row.get("workspace_id"),
        user_id: row.get("user_id"),
        status: deserialize_status(&status)?,
        source: deserialize_json(row.get("source"), "source")?,
        options: deserialize_json(row.get("options"), "options")?,
        result: deserialize_optional_json(row.get("result"), "result")?,
        events: deserialize_json(row.get("events"), "events")?,
        logs: deserialize_json(row.get("logs"), "logs")?,
        redacted_artifacts: deserialize_json(row.get("redacted_artifacts"), "redacted_artifacts")?,
        raw_artifacts: deserialize_json(row.get("raw_artifacts"), "raw_artifacts")?,
        config_snapshot: deserialize_optional_json(row.get("config_snapshot"), "config_snapshot")?,
        attempt: i32_to_u32(attempt, "attempt")?,
        run_after_unix_seconds: i64_to_u64(run_after, "run_after_unix_seconds")?,
        lease: deserialize_optional_json(row.get("lease"), "lease")?,
        cancellation: deserialize_optional_json(row.get("cancellation"), "cancellation")?,
        last_error: row.get("last_error"),
        dedupe_key: row.get("dedupe_key"),
        created_at_utc: row.get("created_at_utc"),
        updated_at_utc: row.get("updated_at_utc"),
    })
}

fn serialize_status(status: ReviewStatus) -> Result<String, ReviewSessionError> {
    match serde_json::to_value(status).map_err(json_store_error("status"))? {
        Value::String(status) => Ok(status),
        _ => Err(ReviewSessionError::Store(
            "failed to serialize review status as string".to_string(),
        )),
    }
}

fn deserialize_status(status: &str) -> Result<ReviewStatus, ReviewSessionError> {
    serde_json::from_value(Value::String(status.to_string())).map_err(json_store_error("status"))
}

fn serialize_json<T: Serialize>(
    value: &T,
    label: &'static str,
) -> Result<Value, ReviewSessionError> {
    serde_json::to_value(value).map_err(json_store_error(label))
}

fn serialize_optional_json<T: Serialize>(
    value: &Option<T>,
    label: &'static str,
) -> Result<Option<Value>, ReviewSessionError> {
    value
        .as_ref()
        .map(|value| serialize_json(value, label))
        .transpose()
}

fn deserialize_json<T: DeserializeOwned>(
    value: Value,
    label: &'static str,
) -> Result<T, ReviewSessionError> {
    serde_json::from_value(value).map_err(json_store_error(label))
}

fn deserialize_optional_json<T: DeserializeOwned>(
    value: Option<Value>,
    label: &'static str,
) -> Result<Option<T>, ReviewSessionError> {
    value
        .map(|value| deserialize_json(value, label))
        .transpose()
}

fn json_store_error(label: &'static str) -> impl FnOnce(serde_json::Error) -> ReviewSessionError {
    move |error| ReviewSessionError::Store(format!("postgres {label} JSON error: {error}"))
}

fn postgres_store_error(error: postgres::Error) -> ReviewSessionError {
    ReviewSessionError::Store(format!("postgres review store error: {error}"))
}

fn u32_to_i32(value: u32, label: &str) -> Result<i32, ReviewSessionError> {
    i32::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} exceeds i32")))
}

fn i32_to_u32(value: i32, label: &str) -> Result<u32, ReviewSessionError> {
    u32::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} is negative")))
}

fn u64_to_i64(value: u64, label: &str) -> Result<i64, ReviewSessionError> {
    i64::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} exceeds i64")))
}

fn i64_to_u64(value: i64, label: &str) -> Result<u64, ReviewSessionError> {
    u64::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} is negative")))
}

fn usize_to_i64(value: usize, label: &str) -> Result<i64, ReviewSessionError> {
    i64::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} exceeds i64")))
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

fn normalize_sensitive_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
