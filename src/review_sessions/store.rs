use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    ReviewArtifact, ReviewCancelOptions, ReviewEvent, ReviewEventType, ReviewResult,
    ReviewSessionError, ReviewSessionId, ReviewSource, ReviewStatus,
};

mod libsql;
mod memory;

pub(crate) use libsql::{LibsqlProjectProfileStore, LibsqlReviewSessionStore};
pub(crate) use memory::InMemoryReviewSessionStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewSessionRecord {
    pub id: ReviewSessionId,
    pub project_id: Option<String>,
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
pub(crate) struct ReviewWorkerLease {
    pub worker_id: String,
    pub attempt: u32,
    pub acquired_at_utc: String,
    pub expires_at_utc: String,
    pub expires_at_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewCancellationRecord {
    #[serde(default)]
    pub reason: Option<String>,
    pub cancelled_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewWorkerClaim {
    pub review_id: ReviewSessionId,
    pub worker_id: String,
    pub attempt: u32,
    pub lease: ReviewWorkerLease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewWorkerClaimOptions {
    pub worker_id: String,
    pub max_sessions: usize,
    pub lease_seconds: u64,
    pub now_unix_seconds: Option<u64>,
    pub concurrency: ReviewWorkerConcurrencyLimits,
}

impl ReviewWorkerClaimOptions {
    fn now_unix_seconds(&self) -> u64 {
        self.now_unix_seconds.unwrap_or_else(current_unix_seconds)
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewWorkerConcurrencyLimits {
    pub max_running_global: Option<usize>,
    pub max_running_per_project: Option<usize>,
    pub max_running_per_user: Option<usize>,
    pub max_running_per_model_profile: Option<usize>,
    pub max_running_per_provider_profile: Option<usize>,
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
pub(crate) struct ReviewAttemptFailure {
    pub error: String,
    pub retry_policy: ReviewRetryPolicy,
    pub now_unix_seconds: Option<u64>,
}

impl ReviewAttemptFailure {
    fn now_unix_seconds(&self) -> u64 {
        self.now_unix_seconds.unwrap_or_else(current_unix_seconds)
    }
}

#[async_trait]
pub(crate) trait ReviewSessionStore: Send + Sync {
    async fn insert(&self, record: ReviewSessionRecord) -> Result<(), ReviewSessionError>;

    async fn get(
        &self,
        id: &ReviewSessionId,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError>;

    async fn get_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError>;

    async fn events_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewEvent>, ReviewSessionError>;

    async fn write_execution_result(
        &self,
        id: &ReviewSessionId,
        status: ReviewStatus,
        result: ReviewResult,
        events: Vec<ReviewEvent>,
        redacted_artifacts: Vec<ReviewArtifact>,
        raw_artifacts: Vec<ReviewArtifact>,
    ) -> Result<ReviewSessionRecord, ReviewSessionError>;

    async fn request_cancellation(
        &self,
        id: &ReviewSessionId,
        options: ReviewCancelOptions,
    ) -> Result<ReviewSessionRecord, ReviewSessionError>;

    async fn claim_ready(
        &self,
        options: ReviewWorkerClaimOptions,
    ) -> Result<Vec<ReviewWorkerClaim>, ReviewSessionError>;

    async fn record_attempt_failure(
        &self,
        id: &ReviewSessionId,
        failure: ReviewAttemptFailure,
    ) -> Result<ReviewSessionRecord, ReviewSessionError>;
}

pub const DEFAULT_MUZEN_STORE_URL: &str = "sqlite://.muzen/muzen.db";
pub const MUZEN_STORE_URL_ENV: &str = "MUZEN_STORE_URL";

pub(crate) struct MuzenStoreBundle {
    pub session_store: std::sync::Arc<dyn ReviewSessionStore>,
    pub profile_store: std::sync::Arc<dyn super::ProjectProfileStore>,
}

impl std::fmt::Debug for MuzenStoreBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MuzenStoreBundle")
            .finish_non_exhaustive()
    }
}

pub(crate) async fn stores_from_url(
    store_url: &str,
) -> Result<MuzenStoreBundle, ReviewSessionError> {
    let store_url = store_url.trim();
    if store_url == "memory://" {
        return Ok(MuzenStoreBundle {
            session_store: std::sync::Arc::new(InMemoryReviewSessionStore::default()),
            profile_store: std::sync::Arc::new(super::InMemoryProjectProfileStore::default()),
        });
    }
    if let Some(path) = sqlite_path_from_url(store_url) {
        let path = path?;
        return Ok(MuzenStoreBundle {
            session_store: std::sync::Arc::new(LibsqlReviewSessionStore::connect(&path).await?),
            profile_store: std::sync::Arc::new(LibsqlProjectProfileStore::connect(&path).await?),
        });
    }
    Err(ReviewSessionError::Store(format!(
        "unsupported Muzen store URL `{store_url}`"
    )))
}

pub(crate) fn sqlite_path_from_url(store_url: &str) -> Option<Result<PathBuf, ReviewSessionError>> {
    let raw_path = store_url.strip_prefix("sqlite://")?;
    if raw_path.trim().is_empty() {
        return Some(Err(ReviewSessionError::Store(
            "sqlite store URL must include a database path".to_string(),
        )));
    }
    if raw_path.starts_with("libsql:")
        || raw_path.starts_with("http:")
        || raw_path.starts_with("https:")
    {
        return Some(Err(ReviewSessionError::Store(
            "sqlite store URL only supports local file paths in v1".to_string(),
        )));
    }
    Some(Ok(Path::new(raw_path).to_path_buf()))
}

#[derive(Debug, Default)]
struct RunningCounts {
    global: usize,
    projects: BTreeMap<String, usize>,
    users: BTreeMap<String, usize>,
    model_profiles: BTreeMap<String, usize>,
    provider_profiles: BTreeMap<String, usize>,
}

impl RunningCounts {
    fn add_record(&mut self, record: &ReviewSessionRecord) {
        self.global += 1;
        increment_key(&mut self.projects, record.project_id.as_deref());
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
            record.project_id.as_deref(),
            &running.projects,
            limits.max_running_per_project,
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
