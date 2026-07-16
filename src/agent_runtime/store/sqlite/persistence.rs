use libsql::{params, Connection, TransactionBehavior};
use serde::de::DeserializeOwned;

use super::super::{StoredRun, StoredSession};
use super::PersistedRun;
use crate::agent_runtime::{
    AgentEvent, AgentMessage, IdempotencyKey, MuzenError, RunId, SessionId,
};

pub(super) async fn immediate(connection: &Connection) -> Result<libsql::Transaction, MuzenError> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(sql_error)
}

pub(super) async fn session_required(
    connection: &Connection,
    id: &SessionId,
) -> Result<StoredSession, MuzenError> {
    let mut rows = connection
        .query(
            "SELECT record FROM muzen_agent_sessions WHERE id = ?1",
            params![id.as_str()],
        )
        .await
        .map_err(sql_error)?;
    let row = rows
        .next()
        .await
        .map_err(sql_error)?
        .ok_or_else(|| MuzenError::not_found(format!("agent session {id} was not found")))?;
    from_json(row.get::<String>(0).map_err(sql_error)?, "session")
}

pub(super) async fn replay_session_missing(
    connection: &Connection,
    id: &SessionId,
) -> Result<bool, MuzenError> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM muzen_agent_sessions WHERE id = ?1",
            params![id.as_str()],
        )
        .await
        .map_err(sql_error)?;
    Ok(rows.next().await.map_err(sql_error)?.is_none())
}

pub(super) async fn run_required(
    connection: &Connection,
    id: &RunId,
) -> Result<PersistedRun, MuzenError> {
    let mut rows = connection
        .query(
            "SELECT record, cancel_sequence FROM muzen_agent_runs WHERE id = ?1",
            params![id.as_str()],
        )
        .await
        .map_err(sql_error)?;
    let row = rows
        .next()
        .await
        .map_err(sql_error)?
        .ok_or_else(|| MuzenError::not_found(format!("run {id} was not found")))?;
    let cancel_sequence = row
        .get::<Option<i64>>(1)
        .map_err(sql_error)?
        .map(|value| {
            u64::try_from(value)
                .map_err(|_| MuzenError::internal("negative cancel event sequence in store"))
        })
        .transpose()?;
    Ok(PersistedRun {
        stored: from_json(row.get::<String>(0).map_err(sql_error)?, "run")?,
        cancel_sequence,
    })
}

pub(super) async fn insert_session(
    connection: &Connection,
    stored: &StoredSession,
) -> Result<(), MuzenError> {
    let record = to_json(stored, "session")?;
    connection
        .execute(
            "INSERT INTO muzen_agent_sessions (id, record) VALUES (?1, ?2)",
            params![stored.snapshot.id.as_str(), record],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

pub(super) async fn update_session(
    connection: &Connection,
    stored: &StoredSession,
) -> Result<(), MuzenError> {
    let record = to_json(stored, "session")?;
    let changed = connection
        .execute(
            "UPDATE muzen_agent_sessions SET record = ?2 WHERE id = ?1",
            params![stored.snapshot.id.as_str(), record],
        )
        .await
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(MuzenError::internal("agent session update lost its row"));
    }
    Ok(())
}

pub(super) async fn insert_run(
    connection: &Connection,
    stored: &StoredRun,
) -> Result<(), MuzenError> {
    let record = to_json(stored, "run")?;
    connection
        .execute(
            "INSERT INTO muzen_agent_runs (id, record, cancel_sequence)
             VALUES (?1, ?2, NULL)",
            params![stored.snapshot.id.as_str(), record],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

pub(super) async fn update_run(
    connection: &Connection,
    persisted: &PersistedRun,
) -> Result<(), MuzenError> {
    let record = to_json(&persisted.stored, "run")?;
    let cancel = persisted
        .cancel_sequence
        .map(|sequence| i64::try_from(sequence).unwrap_or(i64::MAX));
    let changed = connection
        .execute(
            "UPDATE muzen_agent_runs SET record = ?2, cancel_sequence = ?3 WHERE id = ?1",
            params![persisted.stored.snapshot.id.as_str(), record, cancel],
        )
        .await
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(MuzenError::internal("run update lost its row"));
    }
    Ok(())
}

pub(super) async fn insert_message(
    connection: &Connection,
    message: &AgentMessage,
) -> Result<(), MuzenError> {
    let ordinal = next_message_ordinal(connection, &message.session_id).await?;
    let record = to_json(message, "message")?;
    connection
        .execute(
            "INSERT INTO muzen_agent_messages (session_id, ordinal, message_id, message)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                message.session_id.as_str(),
                ordinal,
                message.id.as_str(),
                record
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

pub(super) async fn next_message_ordinal(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<i64, MuzenError> {
    let mut rows = connection
        .query(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM muzen_agent_messages
             WHERE session_id = ?1",
            params![session_id.as_str()],
        )
        .await
        .map_err(sql_error)?;
    rows.next()
        .await
        .map_err(sql_error)?
        .ok_or_else(|| MuzenError::internal("message ordinal query returned no row"))?
        .get(0)
        .map_err(sql_error)
}

pub(super) async fn message_ordinal(
    connection: &Connection,
    session_id: &SessionId,
    message_id: &str,
) -> Result<Option<i64>, MuzenError> {
    let mut rows = connection
        .query(
            "SELECT ordinal FROM muzen_agent_messages
             WHERE session_id = ?1 AND message_id = ?2",
            params![session_id.as_str(), message_id],
        )
        .await
        .map_err(sql_error)?;
    rows.next()
        .await
        .map_err(sql_error)?
        .map(|row| row.get(0).map_err(sql_error))
        .transpose()
}

pub(super) async fn insert_event(
    connection: &Connection,
    event: &AgentEvent,
) -> Result<(), MuzenError> {
    let sequence = i64::try_from(event.sequence)
        .map_err(|_| MuzenError::internal("event sequence exceeds SQLite integer range"))?;
    let record = to_json(event, "event")?;
    connection
        .execute(
            "INSERT INTO muzen_agent_events (run_id, sequence, event) VALUES (?1, ?2, ?3)",
            params![event.run_id.as_str(), sequence, record],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

pub(super) async fn replay_id(
    connection: &Connection,
    scope: &str,
    key: Option<&IdempotencyKey>,
    digest: [u8; 32],
) -> Result<Option<String>, MuzenError> {
    let Some(key) = key else {
        return Ok(None);
    };
    let mut rows = connection
        .query(
            "SELECT digest, resource_id FROM muzen_agent_idempotency
             WHERE scope = ?1 AND key = ?2",
            params![scope, key.as_str()],
        )
        .await
        .map_err(sql_error)?;
    let Some(row) = rows.next().await.map_err(sql_error)? else {
        return Ok(None);
    };
    let stored = row.get::<Vec<u8>>(0).map_err(sql_error)?;
    if stored.as_slice() != digest {
        return Err(MuzenError::conflict(format!(
            "idempotency key {key} was already used with a different body"
        )));
    }
    row.get(1).map(Some).map_err(sql_error)
}

pub(super) async fn remember_id(
    connection: &Connection,
    scope: &str,
    key: Option<&IdempotencyKey>,
    digest: [u8; 32],
    resource_id: &str,
) -> Result<(), MuzenError> {
    let Some(key) = key else {
        return Ok(());
    };
    connection
        .execute(
            "INSERT INTO muzen_agent_idempotency (scope, key, digest, resource_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![scope, key.as_str(), digest.to_vec(), resource_id],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

pub(super) fn to_json<T: serde::Serialize>(value: &T, label: &str) -> Result<String, MuzenError> {
    serde_json::to_string(value)
        .map_err(|error| MuzenError::internal(format!("failed to encode stored {label}: {error}")))
}

pub(super) fn from_json<T: DeserializeOwned>(value: String, label: &str) -> Result<T, MuzenError> {
    serde_json::from_str(&value)
        .map_err(|error| MuzenError::internal(format!("failed to decode stored {label}: {error}")))
}

pub(super) fn sql_error(error: libsql::Error) -> MuzenError {
    MuzenError::internal(format!("SQLite agent store error: {error}"))
}

pub(super) const SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS muzen_agent_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);
INSERT OR IGNORE INTO muzen_agent_meta (id, version) VALUES (1, 1);

CREATE TABLE IF NOT EXISTS muzen_agent_sessions (
    id TEXT PRIMARY KEY,
    record TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS muzen_agent_runs (
    id TEXT PRIMARY KEY,
    record TEXT NOT NULL,
    cancel_sequence INTEGER
);
CREATE TABLE IF NOT EXISTS muzen_agent_messages (
    session_id TEXT NOT NULL REFERENCES muzen_agent_sessions(id),
    ordinal INTEGER NOT NULL,
    message_id TEXT NOT NULL UNIQUE,
    message TEXT NOT NULL,
    PRIMARY KEY (session_id, ordinal)
);
CREATE TABLE IF NOT EXISTS muzen_agent_events (
    run_id TEXT NOT NULL REFERENCES muzen_agent_runs(id),
    sequence INTEGER NOT NULL,
    event TEXT NOT NULL,
    PRIMARY KEY (run_id, sequence)
);
CREATE TABLE IF NOT EXISTS muzen_agent_idempotency (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    digest BLOB NOT NULL,
    resource_id TEXT NOT NULL,
    PRIMARY KEY (scope, key)
);
CREATE TABLE IF NOT EXISTS muzen_agent_sends (
    run_id TEXT NOT NULL REFERENCES muzen_agent_runs(id),
    sequence INTEGER NOT NULL,
    session_id TEXT NOT NULL REFERENCES muzen_agent_sessions(id),
    delivery TEXT NOT NULL,
    record TEXT NOT NULL,
    delivered INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, sequence)
);
CREATE INDEX IF NOT EXISTS muzen_agent_messages_cursor
    ON muzen_agent_messages (session_id, ordinal);
CREATE INDEX IF NOT EXISTS muzen_agent_events_cursor
    ON muzen_agent_events (run_id, sequence);
CREATE INDEX IF NOT EXISTS muzen_agent_sends_pending
    ON muzen_agent_sends (run_id, session_id, delivered, sequence);
"#;
