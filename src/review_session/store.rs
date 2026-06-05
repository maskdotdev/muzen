use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    ReviewArtifact, ReviewCancelOptions, ReviewEvent, ReviewEventType, ReviewResult,
    ReviewSessionError, ReviewSessionId, ReviewSource, ReviewStatus,
};

mod memory;
mod postgres;

pub use memory::InMemoryReviewSessionStore;
pub use postgres::PostgresReviewSessionStore;

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
