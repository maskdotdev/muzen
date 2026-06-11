use std::path::Path;
use std::time::Duration;

use ::libsql::params::IntoParams;
use ::libsql::{params, Builder, Connection, Row, TransactionBehavior};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::super::{
    ModelProfile, ModelProfileInput, ProviderProfile, ProviderProfileInput, ReviewArtifact,
    ReviewCancelOptions, ReviewEvent, ReviewEventType, ReviewResult, ReviewSessionError,
    ReviewSessionId, ReviewStatus, WorkspaceProfileStore,
};
use super::{
    append_record_event, claim_limit_reached, is_claimable, rebase_events, retry_backoff_seconds,
    timestamp_from_unix_seconds, validate_worker_id, worker_lease, ReviewAttemptFailure,
    ReviewCancellationRecord, ReviewLeaseExtension, ReviewLogEntry, ReviewLogRedactionPolicy,
    ReviewSessionRecord, ReviewSessionStore, ReviewWorkerClaim, ReviewWorkerClaimOptions,
    ReviewWorkerLease, RunningCounts,
};

const REVIEW_SESSION_SCHEMA_VERSION: i64 = 1;
const WORKSPACE_PROFILE_SCHEMA_VERSION: i64 = 1;

pub struct LibsqlReviewSessionStore {
    _database: ::libsql::Database,
    connection: Mutex<Connection>,
}

impl std::fmt::Debug for LibsqlReviewSessionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibsqlReviewSessionStore")
            .finish_non_exhaustive()
    }
}

impl LibsqlReviewSessionStore {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ReviewSessionError> {
        let path = path.as_ref();
        ensure_parent_dir(path)?;
        let database = Builder::new_local(path)
            .build()
            .await
            .map_err(libsql_review_error)?;
        let connection = database.connect().map_err(libsql_review_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(libsql_review_error)?;
        let store = Self {
            _database: database,
            connection: Mutex::new(connection),
        };
        store.migrate().await?;
        Ok(store)
    }

    pub async fn migrate(&self) -> Result<(), ReviewSessionError> {
        let connection = self.connection.lock().await;
        connection
            .execute_batch(REVIEW_SESSION_SCHEMA_BOOTSTRAP_SQL)
            .await
            .map_err(libsql_review_error)?;
        let version = current_schema_version(&connection, "review_sessions").await?;
        if version != Some(REVIEW_SESSION_SCHEMA_VERSION) {
            connection
                .execute_batch(REVIEW_SESSION_SCHEMA_RESET_SQL)
                .await
                .map_err(libsql_review_error)?;
            upsert_schema_version(
                &connection,
                "review_sessions",
                REVIEW_SESSION_SCHEMA_VERSION,
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl ReviewSessionStore for LibsqlReviewSessionStore {
    async fn insert(&self, record: ReviewSessionRecord) -> Result<(), ReviewSessionError> {
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        upsert_libsql_record(&transaction, &record).await?;
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(())
    }

    async fn get(
        &self,
        id: &ReviewSessionId,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        libsql_record(&connection, id).await
    }

    async fn get_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        let mut rows = connection
            .query(
                &format!(
                    "SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions WHERE dedupe_key = ?1"
                ),
                params![dedupe_key],
            )
            .await
            .map_err(libsql_review_error)?;
        rows.next()
            .await
            .map_err(libsql_review_error)?
            .map(|row| libsql_row_to_record(&row))
            .transpose()
    }

    async fn append_events(
        &self,
        id: &ReviewSessionId,
        events: Vec<ReviewEvent>,
    ) -> Result<(), ReviewSessionError> {
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let mut record = libsql_record_required(&transaction, id).await?;
        record.events.extend(events);
        record.updated_at_utc = crate::util::timestamp_utc();
        upsert_libsql_record(&transaction, &record).await?;
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(())
    }

    async fn events_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewEvent>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        let record = libsql_record_required(&connection, id).await?;
        let after_cursor = cursor_after(after)?;
        Ok(record
            .events
            .into_iter()
            .filter(|event| {
                event
                    .cursor
                    .parse::<i64>()
                    .map(|cursor| cursor > after_cursor)
                    .unwrap_or(false)
            })
            .collect())
    }

    async fn append_logs(
        &self,
        id: &ReviewSessionId,
        logs: Vec<ReviewLogEntry>,
        redaction: ReviewLogRedactionPolicy,
    ) -> Result<(), ReviewSessionError> {
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let mut record = libsql_record_required(&transaction, id).await?;
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
        upsert_libsql_record(&transaction, &record).await?;
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(())
    }

    async fn logs_after(
        &self,
        id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<Vec<ReviewLogEntry>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        let record = libsql_record_required(&connection, id).await?;
        let after_cursor = cursor_after(after)?;
        Ok(record
            .logs
            .into_iter()
            .filter(|log| {
                log.cursor
                    .parse::<i64>()
                    .map(|cursor| cursor > after_cursor)
                    .unwrap_or(false)
            })
            .collect())
    }

    async fn write_result(
        &self,
        id: &ReviewSessionId,
        status: ReviewStatus,
        result: ReviewResult,
    ) -> Result<(), ReviewSessionError> {
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let mut record = libsql_record_required(&transaction, id).await?;
        record.status = status;
        record.result = Some(result);
        record.lease = None;
        record.updated_at_utc = crate::util::timestamp_utc();
        upsert_libsql_record(&transaction, &record).await?;
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(())
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
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let mut record = libsql_record_required(&transaction, id).await?;
        if record.status.is_terminal() {
            transaction.commit().await.map_err(libsql_review_error)?;
            return Ok(record);
        }
        let rebased_events = rebase_events(&record, events);
        record.status = status;
        record.result = Some(result);
        record.events.extend(rebased_events);
        record.redacted_artifacts = redacted_artifacts;
        record.raw_artifacts = raw_artifacts;
        record.lease = None;
        record.updated_at_utc = crate::util::timestamp_utc();
        upsert_libsql_record(&transaction, &record).await?;
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(record)
    }

    async fn request_cancellation(
        &self,
        id: &ReviewSessionId,
        options: ReviewCancelOptions,
    ) -> Result<ReviewSessionRecord, ReviewSessionError> {
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let mut record = libsql_record_required(&transaction, id).await?;
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
            upsert_libsql_record(&transaction, &record).await?;
        }
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(record)
    }

    async fn claim_ready(
        &self,
        options: ReviewWorkerClaimOptions,
    ) -> Result<Vec<ReviewWorkerClaim>, ReviewSessionError> {
        validate_worker_id(&options.worker_id)?;
        if options.max_sessions == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let now = options.now_unix_seconds();
        let now_i64 = u64_to_i64(now, "now_unix_seconds")?;
        let mut running_rows = transaction
            .query(
                &format!(
                    "SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions
                     WHERE status = 'running' AND lease_expires_at_unix_seconds > ?1
                     ORDER BY run_after_unix_seconds ASC, created_at_utc ASC, id ASC"
                ),
                params![now_i64],
            )
            .await
            .map_err(libsql_review_error)?;
        let mut running = RunningCounts::default();
        while let Some(row) = running_rows.next().await.map_err(libsql_review_error)? {
            let record = libsql_row_to_record(&row)?;
            running.add_record(&record);
        }

        let mut candidate_rows = transaction
            .query(
                &format!(
                    "SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions
                     WHERE status IN ('created', 'queued', 'running')
                       AND run_after_unix_seconds <= ?1
                       AND (status <> 'running' OR lease_expires_at_unix_seconds <= ?1)
                     ORDER BY run_after_unix_seconds ASC, created_at_utc ASC, id ASC"
                ),
                params![now_i64],
            )
            .await
            .map_err(libsql_review_error)?;

        let lease_seconds = options.lease_seconds.max(1);
        let mut claims = Vec::new();
        while let Some(row) = candidate_rows.next().await.map_err(libsql_review_error)? {
            if claims.len() >= options.max_sessions {
                break;
            }
            let mut record = libsql_row_to_record(&row)?;
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
            upsert_libsql_record(&transaction, &record).await?;
            running.add_record(&record);
            claims.push(ReviewWorkerClaim {
                review_id: record.id,
                worker_id: options.worker_id.clone(),
                attempt: record.attempt,
                lease,
            });
        }
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(claims)
    }

    async fn extend_lease(
        &self,
        id: &ReviewSessionId,
        options: ReviewLeaseExtension,
    ) -> Result<ReviewWorkerLease, ReviewSessionError> {
        validate_worker_id(&options.worker_id)?;
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let mut record = libsql_record_required(&transaction, id).await?;
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
        upsert_libsql_record(&transaction, &record).await?;
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(lease)
    }

    async fn record_attempt_failure(
        &self,
        id: &ReviewSessionId,
        failure: ReviewAttemptFailure,
    ) -> Result<ReviewSessionRecord, ReviewSessionError> {
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_review_error)?;
        let mut record = libsql_record_required(&transaction, id).await?;
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
            upsert_libsql_record(&transaction, &record).await?;
        }
        transaction.commit().await.map_err(libsql_review_error)?;
        Ok(record)
    }
}

pub struct LibsqlWorkspaceProfileStore {
    _database: ::libsql::Database,
    connection: Mutex<Connection>,
}

impl std::fmt::Debug for LibsqlWorkspaceProfileStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibsqlWorkspaceProfileStore")
            .finish_non_exhaustive()
    }
}

impl LibsqlWorkspaceProfileStore {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ReviewSessionError> {
        let path = path.as_ref();
        ensure_parent_dir(path)?;
        let database = Builder::new_local(path)
            .build()
            .await
            .map_err(libsql_profile_error)?;
        let connection = database.connect().map_err(libsql_profile_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(libsql_profile_error)?;
        let store = Self {
            _database: database,
            connection: Mutex::new(connection),
        };
        store.migrate().await?;
        Ok(store)
    }

    pub async fn migrate(&self) -> Result<(), ReviewSessionError> {
        let connection = self.connection.lock().await;
        connection
            .execute_batch(REVIEW_SESSION_SCHEMA_BOOTSTRAP_SQL)
            .await
            .map_err(libsql_profile_error)?;
        let version = current_schema_version(&connection, "workspace_profiles").await?;
        if version != Some(WORKSPACE_PROFILE_SCHEMA_VERSION) {
            connection
                .execute_batch(WORKSPACE_PROFILE_SCHEMA_RESET_SQL)
                .await
                .map_err(libsql_profile_error)?;
            upsert_schema_version(
                &connection,
                "workspace_profiles",
                WORKSPACE_PROFILE_SCHEMA_VERSION,
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl WorkspaceProfileStore for LibsqlWorkspaceProfileStore {
    async fn set_model_profile(
        &self,
        workspace_id: &str,
        name: String,
        input: ModelProfileInput,
    ) -> Result<ModelProfile, ReviewSessionError> {
        validate_profile_key(workspace_id, &name)?;
        if input.model.trim().is_empty() {
            return Err(ReviewSessionError::Profile(
                "model profile model cannot be empty".to_string(),
            ));
        }
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_profile_error)?;
        let current = query_optional_string(
            &transaction,
            "SELECT version FROM muzen_workspace_model_profiles
             WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id, name.as_str()],
        )
        .await?;
        let version = next_version(current.as_deref());
        let profile = ModelProfile {
            workspace_id: workspace_id.to_string(),
            name,
            version,
            provider: input.provider,
            model: input.model,
            secret_ref: input.secret_ref,
            base_url: input.base_url,
            routing: input.routing,
            updated_at_utc: crate::util::timestamp_utc(),
        };
        let profile_json = serialize_json_string(&profile, "model profile")
            .map_err(store_json_error_to_profile)?;
        transaction
            .execute(
                "INSERT INTO muzen_workspace_model_profiles
                    (workspace_id, name, version, profile, updated_at_utc)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (workspace_id, name) DO UPDATE SET
                    version = excluded.version,
                    profile = excluded.profile,
                    updated_at_utc = excluded.updated_at_utc",
                params![
                    profile.workspace_id.as_str(),
                    profile.name.as_str(),
                    profile.version.as_str(),
                    profile_json.as_str(),
                    profile.updated_at_utc.as_str()
                ],
            )
            .await
            .map_err(libsql_profile_error)?;
        transaction.commit().await.map_err(libsql_profile_error)?;
        Ok(profile)
    }

    async fn get_model_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ModelProfile>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        query_optional_profile(
            &connection,
            "SELECT profile FROM muzen_workspace_model_profiles
             WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id, name],
            "model",
        )
        .await
    }

    async fn list_model_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ModelProfile>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        query_profiles(
            &connection,
            "SELECT profile FROM muzen_workspace_model_profiles
             WHERE workspace_id = ?1
             ORDER BY name ASC",
            params![workspace_id],
            "model",
        )
        .await
    }

    async fn set_provider_profile(
        &self,
        workspace_id: &str,
        name: String,
        input: ProviderProfileInput,
    ) -> Result<ProviderProfile, ReviewSessionError> {
        validate_profile_key(workspace_id, &name)?;
        let connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(libsql_profile_error)?;
        let current = query_optional_string(
            &transaction,
            "SELECT version FROM muzen_workspace_provider_profiles
             WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id, name.as_str()],
        )
        .await?;
        let version = next_version(current.as_deref());
        let profile = ProviderProfile {
            workspace_id: workspace_id.to_string(),
            name,
            version,
            provider: input.provider,
            secret_ref: input.secret_ref,
            base_url: input.base_url,
            routing: input.routing,
            updated_at_utc: crate::util::timestamp_utc(),
        };
        let profile_json = serialize_json_string(&profile, "provider profile")
            .map_err(store_json_error_to_profile)?;
        transaction
            .execute(
                "INSERT INTO muzen_workspace_provider_profiles
                    (workspace_id, name, version, profile, updated_at_utc)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (workspace_id, name) DO UPDATE SET
                    version = excluded.version,
                    profile = excluded.profile,
                    updated_at_utc = excluded.updated_at_utc",
                params![
                    profile.workspace_id.as_str(),
                    profile.name.as_str(),
                    profile.version.as_str(),
                    profile_json.as_str(),
                    profile.updated_at_utc.as_str()
                ],
            )
            .await
            .map_err(libsql_profile_error)?;
        transaction.commit().await.map_err(libsql_profile_error)?;
        Ok(profile)
    }

    async fn get_provider_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        query_optional_profile(
            &connection,
            "SELECT profile FROM muzen_workspace_provider_profiles
             WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id, name],
            "provider",
        )
        .await
    }

    async fn list_provider_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ProviderProfile>, ReviewSessionError> {
        let connection = self.connection.lock().await;
        query_profiles(
            &connection,
            "SELECT profile FROM muzen_workspace_provider_profiles
             WHERE workspace_id = ?1
             ORDER BY name ASC",
            params![workspace_id],
            "provider",
        )
        .await
    }
}

const REVIEW_SESSION_COLUMNS: &str = "id, workspace_id, user_id, status, source, options, result,
events, logs, redacted_artifacts, raw_artifacts, config_snapshot, attempt,
run_after_unix_seconds, lease, cancellation, last_error, dedupe_key,
created_at_utc, updated_at_utc";

const REVIEW_SESSION_SCHEMA_BOOTSTRAP_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS muzen_schema_versions (
    name TEXT PRIMARY KEY,
    version INTEGER NOT NULL
);
"#;

const REVIEW_SESSION_SCHEMA_RESET_SQL: &str = r#"
DROP TABLE IF EXISTS muzen_review_sessions;

CREATE TABLE IF NOT EXISTS muzen_review_sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT,
    user_id TEXT,
    status TEXT NOT NULL,
    source TEXT NOT NULL,
    options TEXT NOT NULL,
    result TEXT,
    events TEXT NOT NULL,
    logs TEXT NOT NULL,
    redacted_artifacts TEXT NOT NULL,
    raw_artifacts TEXT NOT NULL,
    config_snapshot TEXT,
    attempt INTEGER NOT NULL DEFAULT 0,
    run_after_unix_seconds INTEGER NOT NULL DEFAULT 0,
    lease TEXT,
    lease_expires_at_unix_seconds INTEGER NOT NULL DEFAULT 0,
    cancellation TEXT,
    last_error TEXT,
    dedupe_key TEXT UNIQUE,
    created_at_utc TEXT NOT NULL,
    updated_at_utc TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS muzen_review_sessions_ready_idx
    ON muzen_review_sessions (status, run_after_unix_seconds, lease_expires_at_unix_seconds, created_at_utc, id);

CREATE INDEX IF NOT EXISTS muzen_review_sessions_workspace_idx
    ON muzen_review_sessions (workspace_id, status);
"#;

const WORKSPACE_PROFILE_SCHEMA_RESET_SQL: &str = r#"
DROP TABLE IF EXISTS muzen_workspace_model_profiles;
DROP TABLE IF EXISTS muzen_workspace_provider_profiles;

CREATE TABLE IF NOT EXISTS muzen_workspace_model_profiles (
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    profile TEXT NOT NULL,
    updated_at_utc TEXT NOT NULL,
    PRIMARY KEY (workspace_id, name)
);

CREATE TABLE IF NOT EXISTS muzen_workspace_provider_profiles (
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    profile TEXT NOT NULL,
    updated_at_utc TEXT NOT NULL,
    PRIMARY KEY (workspace_id, name)
);
"#;

async fn libsql_record(
    connection: &Connection,
    id: &ReviewSessionId,
) -> Result<Option<ReviewSessionRecord>, ReviewSessionError> {
    let mut rows = connection
        .query(
            &format!("SELECT {REVIEW_SESSION_COLUMNS} FROM muzen_review_sessions WHERE id = ?1"),
            params![id.as_str()],
        )
        .await
        .map_err(libsql_review_error)?;
    rows.next()
        .await
        .map_err(libsql_review_error)?
        .map(|row| libsql_row_to_record(&row))
        .transpose()
}

async fn libsql_record_required(
    connection: &Connection,
    id: &ReviewSessionId,
) -> Result<ReviewSessionRecord, ReviewSessionError> {
    libsql_record(connection, id)
        .await?
        .ok_or_else(|| ReviewSessionError::Store(format!("unknown review session {id}")))
}

async fn upsert_libsql_record(
    connection: &Connection,
    record: &ReviewSessionRecord,
) -> Result<(), ReviewSessionError> {
    let status = serialize_status(record.status)?;
    let source = serialize_json_string(&record.source, "source")?;
    let options = serialize_json_string(&record.options, "options")?;
    let result = serialize_optional_json_string(&record.result, "result")?;
    let events = serialize_json_string(&record.events, "events")?;
    let logs = serialize_json_string(&record.logs, "logs")?;
    let redacted_artifacts =
        serialize_json_string(&record.redacted_artifacts, "redacted_artifacts")?;
    let raw_artifacts = serialize_json_string(&record.raw_artifacts, "raw_artifacts")?;
    let config_snapshot =
        serialize_optional_json_string(&record.config_snapshot, "config_snapshot")?;
    let attempt = u32_to_i64(record.attempt, "attempt")?;
    let run_after = u64_to_i64(record.run_after_unix_seconds, "run_after_unix_seconds")?;
    let lease = serialize_optional_json_string(&record.lease, "lease")?;
    let lease_expires = record
        .lease
        .as_ref()
        .map(|lease| {
            u64_to_i64(
                lease.expires_at_unix_seconds,
                "lease_expires_at_unix_seconds",
            )
        })
        .transpose()?
        .unwrap_or(0);
    let cancellation = serialize_optional_json_string(&record.cancellation, "cancellation")?;
    connection
        .execute(
            "INSERT INTO muzen_review_sessions (
                id, workspace_id, user_id, status, source, options, result, events, logs,
                redacted_artifacts, raw_artifacts, config_snapshot, attempt,
                run_after_unix_seconds, lease, lease_expires_at_unix_seconds, cancellation,
                last_error, dedupe_key, created_at_utc, updated_at_utc
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21
             )
             ON CONFLICT (id) DO UPDATE SET
                workspace_id = excluded.workspace_id,
                user_id = excluded.user_id,
                status = excluded.status,
                source = excluded.source,
                options = excluded.options,
                result = excluded.result,
                events = excluded.events,
                logs = excluded.logs,
                redacted_artifacts = excluded.redacted_artifacts,
                raw_artifacts = excluded.raw_artifacts,
                config_snapshot = excluded.config_snapshot,
                attempt = excluded.attempt,
                run_after_unix_seconds = excluded.run_after_unix_seconds,
                lease = excluded.lease,
                lease_expires_at_unix_seconds = excluded.lease_expires_at_unix_seconds,
                cancellation = excluded.cancellation,
                last_error = excluded.last_error,
                dedupe_key = excluded.dedupe_key,
                updated_at_utc = excluded.updated_at_utc",
            params![
                record.id.as_str(),
                record.workspace_id.clone(),
                record.user_id.clone(),
                status.as_str(),
                source.as_str(),
                options.as_str(),
                result.as_deref(),
                events.as_str(),
                logs.as_str(),
                redacted_artifacts.as_str(),
                raw_artifacts.as_str(),
                config_snapshot.as_deref(),
                attempt,
                run_after,
                lease.as_deref(),
                lease_expires,
                cancellation.as_deref(),
                record.last_error.clone(),
                record.dedupe_key.clone(),
                record.created_at_utc.as_str(),
                record.updated_at_utc.as_str()
            ],
        )
        .await
        .map_err(libsql_review_error)?;
    Ok(())
}

fn libsql_row_to_record(row: &Row) -> Result<ReviewSessionRecord, ReviewSessionError> {
    let id: String = row_text(row, 0)?;
    let review_id = ReviewSessionId::new(id)?;
    let status: String = row_text(row, 3)?;
    let attempt: i64 = row_i64(row, 12)?;
    let run_after: i64 = row_i64(row, 13)?;
    Ok(ReviewSessionRecord {
        id: review_id,
        workspace_id: row_optional_text(row, 1)?,
        user_id: row_optional_text(row, 2)?,
        status: deserialize_status(&status)?,
        source: deserialize_json_string(row_text(row, 4)?, "source")?,
        options: deserialize_json_string(row_text(row, 5)?, "options")?,
        result: deserialize_optional_json_string(row_optional_text(row, 6)?, "result")?,
        events: deserialize_json_string(row_text(row, 7)?, "events")?,
        logs: deserialize_json_string(row_text(row, 8)?, "logs")?,
        redacted_artifacts: deserialize_json_string(row_text(row, 9)?, "redacted_artifacts")?,
        raw_artifacts: deserialize_json_string(row_text(row, 10)?, "raw_artifacts")?,
        config_snapshot: deserialize_optional_json_string(
            row_optional_text(row, 11)?,
            "config_snapshot",
        )?,
        attempt: i64_to_u32(attempt, "attempt")?,
        run_after_unix_seconds: i64_to_u64(run_after, "run_after_unix_seconds")?,
        lease: deserialize_optional_json_string(row_optional_text(row, 14)?, "lease")?,
        cancellation: deserialize_optional_json_string(
            row_optional_text(row, 15)?,
            "cancellation",
        )?,
        last_error: row_optional_text(row, 16)?,
        dedupe_key: row_optional_text(row, 17)?,
        created_at_utc: row_text(row, 18)?,
        updated_at_utc: row_text(row, 19)?,
    })
}

async fn current_schema_version(
    connection: &Connection,
    name: &str,
) -> Result<Option<i64>, ReviewSessionError> {
    let mut rows = connection
        .query(
            "SELECT version FROM muzen_schema_versions WHERE name = ?1",
            params![name],
        )
        .await
        .map_err(libsql_review_error)?;
    rows.next()
        .await
        .map_err(libsql_review_error)?
        .map(|row| row_i64(&row, 0))
        .transpose()
}

async fn upsert_schema_version(
    connection: &Connection,
    name: &str,
    version: i64,
) -> Result<(), ReviewSessionError> {
    connection
        .execute(
            "INSERT INTO muzen_schema_versions (name, version)
             VALUES (?1, ?2)
             ON CONFLICT (name) DO UPDATE SET version = excluded.version",
            params![name, version],
        )
        .await
        .map_err(libsql_review_error)?;
    Ok(())
}

async fn query_optional_string(
    connection: &Connection,
    sql: &str,
    params: impl IntoParams,
) -> Result<Option<String>, ReviewSessionError> {
    let mut rows = connection
        .query(sql, params)
        .await
        .map_err(libsql_profile_error)?;
    rows.next()
        .await
        .map_err(libsql_profile_error)?
        .map(|row| row_text(&row, 0).map_err(store_error_to_profile))
        .transpose()
}

async fn query_optional_profile<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    params: impl IntoParams,
    kind: &'static str,
) -> Result<Option<T>, ReviewSessionError> {
    let mut rows = connection
        .query(sql, params)
        .await
        .map_err(libsql_profile_error)?;
    rows.next()
        .await
        .map_err(libsql_profile_error)?
        .map(|row| {
            let payload = row_text(&row, 0).map_err(store_error_to_profile)?;
            serde_json::from_str(&payload).map_err(|error| {
                ReviewSessionError::Profile(format!("{kind} profile JSON error: {error}"))
            })
        })
        .transpose()
}

async fn query_profiles<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    params: impl IntoParams,
    kind: &'static str,
) -> Result<Vec<T>, ReviewSessionError> {
    let mut rows = connection
        .query(sql, params)
        .await
        .map_err(libsql_profile_error)?;
    let mut profiles = Vec::new();
    while let Some(row) = rows.next().await.map_err(libsql_profile_error)? {
        let payload = row_text(&row, 0).map_err(store_error_to_profile)?;
        profiles.push(serde_json::from_str(&payload).map_err(|error| {
            ReviewSessionError::Profile(format!("{kind} profile JSON error: {error}"))
        })?);
    }
    Ok(profiles)
}

fn row_text(row: &Row, index: i32) -> Result<String, ReviewSessionError> {
    row.get(index).map_err(libsql_review_error)
}

fn row_optional_text(row: &Row, index: i32) -> Result<Option<String>, ReviewSessionError> {
    row.get(index).map_err(libsql_review_error)
}

fn row_i64(row: &Row, index: i32) -> Result<i64, ReviewSessionError> {
    row.get(index).map_err(libsql_review_error)
}

fn cursor_after(after: Option<&str>) -> Result<i64, ReviewSessionError> {
    after
        .map(|cursor| {
            cursor.parse::<i64>().map_err(|_| {
                ReviewSessionError::Store(format!("invalid review event/log cursor {cursor}"))
            })
        })
        .transpose()
        .map(|cursor| cursor.unwrap_or(0))
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

fn serialize_json_string<T: Serialize>(
    value: &T,
    label: &'static str,
) -> Result<String, ReviewSessionError> {
    serde_json::to_string(value).map_err(json_store_error(label))
}

fn serialize_optional_json_string<T: Serialize>(
    value: &Option<T>,
    label: &'static str,
) -> Result<Option<String>, ReviewSessionError> {
    value
        .as_ref()
        .map(|value| serialize_json_string(value, label))
        .transpose()
}

fn deserialize_json_string<T: DeserializeOwned>(
    value: String,
    label: &'static str,
) -> Result<T, ReviewSessionError> {
    serde_json::from_str(&value).map_err(json_store_error(label))
}

fn deserialize_optional_json_string<T: DeserializeOwned>(
    value: Option<String>,
    label: &'static str,
) -> Result<Option<T>, ReviewSessionError> {
    value
        .map(|value| deserialize_json_string(value, label))
        .transpose()
}

fn json_store_error(label: &'static str) -> impl FnOnce(serde_json::Error) -> ReviewSessionError {
    move |error| ReviewSessionError::Store(format!("libsql {label} JSON error: {error}"))
}

fn store_error_to_profile(error: ReviewSessionError) -> ReviewSessionError {
    ReviewSessionError::Profile(error.to_string())
}

fn store_json_error_to_profile(error: ReviewSessionError) -> ReviewSessionError {
    match error {
        ReviewSessionError::Store(message) => ReviewSessionError::Profile(message),
        error => error,
    }
}

fn libsql_review_error(error: ::libsql::Error) -> ReviewSessionError {
    ReviewSessionError::Store(format!("libsql review store error: {error}"))
}

fn libsql_profile_error(error: ::libsql::Error) -> ReviewSessionError {
    ReviewSessionError::Profile(format!("libsql profile store error: {error}"))
}

fn ensure_parent_dir(path: &Path) -> Result<(), ReviewSessionError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            ReviewSessionError::Store(format!(
                "failed to create sqlite store directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    Ok(())
}

fn validate_profile_key(workspace_id: &str, name: &str) -> Result<(), ReviewSessionError> {
    if workspace_id.trim().is_empty() {
        return Err(ReviewSessionError::Profile(
            "workspace id cannot be empty".to_string(),
        ));
    }
    if name.trim().is_empty() {
        return Err(ReviewSessionError::Profile(
            "profile name cannot be empty".to_string(),
        ));
    }
    Ok(())
}

fn next_version(previous: Option<&str>) -> String {
    previous
        .and_then(|version| version.parse::<u64>().ok())
        .map_or(1, |version| version + 1)
        .to_string()
}

fn u32_to_i64(value: u32, label: &str) -> Result<i64, ReviewSessionError> {
    i64::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} exceeds i64")))
}

fn i64_to_u32(value: i64, label: &str) -> Result<u32, ReviewSessionError> {
    u32::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} is negative")))
}

fn u64_to_i64(value: u64, label: &str) -> Result<i64, ReviewSessionError> {
    i64::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} exceeds i64")))
}

fn i64_to_u64(value: i64, label: &str) -> Result<u64, ReviewSessionError> {
    u64::try_from(value).map_err(|_| ReviewSessionError::Store(format!("{label} is negative")))
}
