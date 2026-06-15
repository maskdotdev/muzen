use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeResult};
pub use crate::reviewer_kernel::kernel_types::{
    RuntimeEvent, RuntimeEventContext, RuntimeEventRecord, RuntimeEventSink as EventSink,
};
use crate::reviewer_kernel::system::{timestamp_utc, SCHEMA_VERSION};

#[derive(Debug, Default)]
pub struct InMemoryEventSink {
    records: Mutex<Vec<RuntimeEventRecord>>,
}

impl InMemoryEventSink {
    pub fn events(&self) -> Vec<RuntimeEvent> {
        self.records()
            .into_iter()
            .map(|record| record.event)
            .collect()
    }

    pub fn records(&self) -> Vec<RuntimeEventRecord> {
        self.records
            .lock()
            .expect("in-memory event sink poisoned")
            .clone()
    }

    pub fn export_jsonl(&self, path: impl AsRef<Path>) -> RuntimeResult<RuntimeEventJsonlManifest> {
        write_event_records_jsonl(path.as_ref(), &self.records(), 0)
    }

    fn record(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let mut records = self.records.lock().expect("in-memory event sink poisoned");
        let seq = records.len() as u64 + 1;
        records.push(RuntimeEventRecord {
            seq,
            timestamp_utc: timestamp_utc(),
            context,
            event,
        });
    }
}

impl EventSink for InMemoryEventSink {
    fn emit(&self, event: RuntimeEvent) {
        let context = RuntimeEventContext::from_event(&event);
        self.record(context, event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        self.record(context, event);
    }
}

#[derive(Debug)]
pub struct BoundedInMemoryEventSink {
    capacity: usize,
    policy: EventBackpressurePolicy,
    next_seq: AtomicU64,
    dropped: AtomicUsize,
    records: Mutex<Vec<RuntimeEventRecord>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum EventBackpressurePolicy {
    DropNewest,
    DropOldest,
}

impl BoundedInMemoryEventSink {
    pub fn new(capacity: usize) -> Self {
        Self::with_policy(capacity, EventBackpressurePolicy::DropNewest)
    }

    pub fn with_policy(capacity: usize, policy: EventBackpressurePolicy) -> Self {
        Self {
            capacity: capacity.max(1),
            policy,
            next_seq: AtomicU64::new(1),
            dropped: AtomicUsize::new(0),
            records: Mutex::new(Vec::new()),
        }
    }

    pub fn records(&self) -> Vec<RuntimeEventRecord> {
        self.records
            .lock()
            .expect("bounded in-memory event sink poisoned")
            .clone()
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn export_jsonl(&self, path: impl AsRef<Path>) -> RuntimeResult<RuntimeEventJsonlManifest> {
        write_event_records_jsonl(path.as_ref(), &self.records(), self.dropped_count())
    }

    fn record(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut records = self
            .records
            .lock()
            .expect("bounded in-memory event sink poisoned");
        if records.len() >= self.capacity {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            match self.policy {
                EventBackpressurePolicy::DropNewest => return,
                EventBackpressurePolicy::DropOldest => {
                    records.remove(0);
                }
            }
        }
        records.push(RuntimeEventRecord {
            seq,
            timestamp_utc: timestamp_utc(),
            context,
            event,
        });
    }
}

impl EventSink for BoundedInMemoryEventSink {
    fn emit(&self, event: RuntimeEvent) {
        let context = RuntimeEventContext::from_event(&event);
        self.record(context, event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        self.record(context, event);
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeEventJsonlManifest {
    pub path: PathBuf,
    pub record_count: usize,
    pub dropped_count: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct RuntimeEventJsonlLoad {
    pub path: PathBuf,
    pub record_count: usize,
    pub records: Vec<RuntimeEventRecord>,
}

pub fn export_event_records_jsonl(
    path: impl AsRef<Path>,
    records: &[RuntimeEventRecord],
) -> RuntimeResult<RuntimeEventJsonlManifest> {
    write_event_records_jsonl(path.as_ref(), records, 0)
}

pub fn load_event_records_jsonl(path: impl AsRef<Path>) -> RuntimeResult<RuntimeEventJsonlLoad> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to read event log: {error}"))
    })?;
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: RuntimeEventJsonlRecord = serde_json::from_str(line).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "invalid event log record at line {}: {error}",
                index + 1
            ))
        })?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported event log schemaVersion {} at line {}",
                record.schema_version,
                index + 1
            )));
        }
        let context = record.context.ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "missing event log context at line {} for schemaVersion {}",
                index + 1,
                SCHEMA_VERSION
            ))
        })?;
        records.push(RuntimeEventRecord {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc,
            context,
            event: record.event,
        });
    }
    Ok(RuntimeEventJsonlLoad {
        path: path.to_path_buf(),
        record_count: records.len(),
        records,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeEventJsonlRecord {
    schema_version: String,
    seq: u64,
    timestamp_utc: String,
    context: Option<RuntimeEventContext>,
    event: RuntimeEvent,
}

fn write_event_records_jsonl(
    path: &Path,
    records: &[RuntimeEventRecord],
    dropped_count: usize,
) -> RuntimeResult<RuntimeEventJsonlManifest> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to create event log directory: {error}"))
        })?;
    }
    let mut file = std::fs::File::create(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to create event log: {error}"))
    })?;
    let mut bytes = 0usize;
    for record in records {
        let line = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": SCHEMA_VERSION,
            "seq": record.seq,
            "timestampUtc": record.timestamp_utc,
            "context": &record.context,
            "event": &record.event,
        }))
        .map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to serialize event log: {error}"))
        })?;
        file.write_all(&line).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write event log: {error}"))
        })?;
        file.write_all(b"\n").map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write event log: {error}"))
        })?;
        bytes += line.len() + 1;
    }
    file.flush().map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to flush event log: {error}"))
    })?;
    Ok(RuntimeEventJsonlManifest {
        path: path.to_path_buf(),
        record_count: records.len(),
        dropped_count,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::reviewer_kernel::kernel_types::{SessionId, SessionScope, TurnId};
    use crate::reviewer_kernel::review_contract::{AgentBudget, Role};

    #[test]
    fn agent_trace_events_round_trip_through_runtime_jsonl() {
        let sink = InMemoryEventSink::default();
        let scope = SessionScope::review_read_only(
            SessionId("trace-session".to_string()),
            Role::Generalist,
            "trace test",
            AgentBudget {
                max_turns: 1,
                max_tool_calls: 1,
                max_prompt_tokens: 1024,
                max_output_tokens: 128,
                budget_source:
                    crate::reviewer_kernel::review_contract::BudgetSource::PlannedDefault,
            },
        );
        sink.emit(RuntimeEvent::AgentTrace {
            session_id: scope.id.clone(),
            turn_id: Some(TurnId(7)),
            trace_kind: "model_turn_prepared".to_string(),
            summary: "prepared model turn".to_string(),
            details: json!({
                "transcriptItems": 2,
                "exposedTools": [{"modelName": "read"}],
            }),
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runtime-events.jsonl");
        let manifest = sink.export_jsonl(&path).expect("export jsonl");
        assert_eq!(manifest.record_count, 1);

        let loaded = load_event_records_jsonl(&path).expect("load jsonl");
        assert_eq!(loaded.record_count, 1);
        let record = loaded.records.first().expect("record");
        assert_eq!(record.context.session_id, Some(scope.id));
        assert_eq!(record.context.turn_id, Some(TurnId(7)));
        match &record.event {
            RuntimeEvent::AgentTrace {
                trace_kind,
                summary,
                details,
                ..
            } => {
                assert_eq!(trace_kind, "model_turn_prepared");
                assert_eq!(summary, "prepared model turn");
                assert_eq!(details["transcriptItems"], 2);
                assert_eq!(details["exposedTools"][0]["modelName"], "read");
            }
            event => panic!("expected agent trace, got {event:?}"),
        }
    }
}
