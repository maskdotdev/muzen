use std::sync::Mutex;

use ::postgres::{Client, GenericClient, NoTls, Row};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use super::super::{
    ReviewArtifact, ReviewCancelOptions, ReviewEvent, ReviewEventType, ReviewResult,
    ReviewSessionError, ReviewSessionId, ReviewStatus,
};
use super::{
    append_record_event, claim_limit_reached, is_claimable, rebase_events, retry_backoff_seconds,
    timestamp_from_unix_seconds, validate_worker_id, worker_lease, ReviewAttemptFailure,
    ReviewCancellationRecord, ReviewLeaseExtension, ReviewLogEntry, ReviewLogRedactionPolicy,
    ReviewSessionRecord, ReviewSessionStore, ReviewWorkerClaim, ReviewWorkerClaimOptions,
    ReviewWorkerLease, RunningCounts,
};

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
        if record.status.is_terminal() {
            transaction.commit().map_err(postgres_store_error)?;
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

fn postgres_store_error(error: ::postgres::Error) -> ReviewSessionError {
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
