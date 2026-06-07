use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::contracts::{
    ArtifactId, RuntimeError, RuntimeEvent, RuntimeEventContext, RuntimeResult, SnapshotId,
    ToolErrorCode,
};

use crate::util::timestamp_utc;

#[async_trait]
pub trait ReviewEventSink: Send + Sync {
    fn emit_review_event(&self, record: ReviewEventRecord);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewEventRecord {
    pub seq: u64,
    pub timestamp_utc: String,
    pub run_id: Option<String>,
    pub snapshot_id: Option<SnapshotId>,
    pub session_id: Option<String>,
    pub turn: Option<u32>,
    pub tool_call_id: Option<String>,
    pub artifact_id: Option<ArtifactId>,
    pub finding_id: Option<String>,
    pub event: ReviewEvent,
}

impl ReviewEventRecord {
    pub(crate) fn from_runtime(
        seq: u64,
        context: RuntimeEventContext,
        event: &RuntimeEvent,
    ) -> Self {
        Self {
            seq,
            timestamp_utc: timestamp_utc(),
            run_id: context.run_id,
            snapshot_id: context.snapshot_id,
            session_id: context.session_id.map(|id| id.0),
            turn: context.turn_id.map(|turn| turn.0),
            tool_call_id: context.tool_call_id.map(|id| id.0),
            artifact_id: context.artifact_id,
            finding_id: context.finding_id,
            event: ReviewEvent::from_runtime(event),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ReviewEvent {
    RunStarted {
        snapshot_id: SnapshotId,
    },
    SnapshotStarted {
        snapshot_id: SnapshotId,
    },
    ContextIndexStarted {
        snapshot_id: SnapshotId,
    },
    ContextIndexCompleted {
        snapshot_id: SnapshotId,
        index_id: String,
        evidence_count: usize,
        indexed_files: usize,
        skipped_files: usize,
        ms: u64,
    },
    ContextPackStarted {
        session_id: Option<String>,
        purpose: String,
    },
    ContextPackCompleted {
        pack_id: String,
        session_id: Option<String>,
        purpose: String,
        evidence_count: usize,
        omitted_count: usize,
        used_tokens: usize,
        sufficiency: String,
        ms: u64,
    },
    ContextQueryCompleted {
        session_id: Option<String>,
        query_kind: String,
        result_count: usize,
        artifact_id: Option<ArtifactId>,
        ms: u64,
    },
    RepoManifestCompleted {
        files: usize,
        skipped: usize,
        bytes: u64,
        ms: u64,
    },
    SessionStarted {
        session_id: String,
    },
    ModelStarted {
        session_id: String,
        turn: u32,
    },
    ModelCompleted {
        session_id: String,
        turn: u32,
        tool_call_count: usize,
    },
    ModelFailed {
        session_id: String,
        turn: u32,
        attempt: usize,
        retrying: bool,
        message: String,
    },
    ToolBatchStarted {
        session_id: String,
        turn: u32,
        count: usize,
    },
    ToolCallCompleted {
        call_id: String,
        tool_id: String,
        ok: bool,
        error_code: Option<ToolErrorCode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    ToolCallDenied {
        call_id: String,
        tool_id: String,
        error_code: ToolErrorCode,
        reason: String,
    },
    ArtifactCreated {
        artifact_id: ArtifactId,
        tool_call_id: String,
        tool_id: String,
        bytes: usize,
        content_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    FindingRecorded {
        finding_id: String,
        session_id: String,
        tool_call_id: String,
    },
    SearchBatchCompleted {
        searched_files: usize,
        skipped_files: usize,
        bytes_scanned: usize,
        ms: u64,
    },
    SessionFinished {
        session_id: String,
        status: String,
    },
    SnapshotFinished {
        snapshot_id: SnapshotId,
        sessions: usize,
        completed_sessions: usize,
    },
    RunFinished {
        status: String,
    },
}

impl ReviewEvent {
    fn from_runtime(event: &RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::JobStarted { snapshot_id } => Self::RunStarted {
                snapshot_id: snapshot_id.clone(),
            },
            RuntimeEvent::SnapshotStarted { snapshot_id } => Self::SnapshotStarted {
                snapshot_id: snapshot_id.clone(),
            },
            RuntimeEvent::ContextIndexStarted { snapshot_id } => Self::ContextIndexStarted {
                snapshot_id: snapshot_id.clone(),
            },
            RuntimeEvent::ContextIndexCompleted {
                snapshot_id,
                index_id,
                evidence_count,
                indexed_files,
                skipped_files,
                ms,
            } => Self::ContextIndexCompleted {
                snapshot_id: snapshot_id.clone(),
                index_id: index_id.clone(),
                evidence_count: *evidence_count,
                indexed_files: *indexed_files,
                skipped_files: *skipped_files,
                ms: *ms,
            },
            RuntimeEvent::ContextPackStarted {
                session_id,
                purpose,
            } => Self::ContextPackStarted {
                session_id: session_id.as_ref().map(|id| id.0.clone()),
                purpose: purpose.clone(),
            },
            RuntimeEvent::ContextPackCompleted {
                pack_id,
                session_id,
                purpose,
                evidence_count,
                omitted_count,
                used_tokens,
                sufficiency,
                ms,
            } => Self::ContextPackCompleted {
                pack_id: pack_id.clone(),
                session_id: session_id.as_ref().map(|id| id.0.clone()),
                purpose: purpose.clone(),
                evidence_count: *evidence_count,
                omitted_count: *omitted_count,
                used_tokens: *used_tokens,
                sufficiency: sufficiency.clone(),
                ms: *ms,
            },
            RuntimeEvent::ContextQueryCompleted {
                session_id,
                query_kind,
                result_count,
                artifact_id,
                ms,
            } => Self::ContextQueryCompleted {
                session_id: session_id.as_ref().map(|id| id.0.clone()),
                query_kind: query_kind.clone(),
                result_count: *result_count,
                artifact_id: artifact_id.clone(),
                ms: *ms,
            },
            RuntimeEvent::RepoManifestCompleted {
                files,
                skipped,
                bytes,
                ms,
            } => Self::RepoManifestCompleted {
                files: *files,
                skipped: *skipped,
                bytes: *bytes,
                ms: *ms,
            },
            RuntimeEvent::SessionStarted { session_id } => Self::SessionStarted {
                session_id: session_id.0.clone(),
            },
            RuntimeEvent::ModelStarted {
                session_id,
                turn_id,
            } => Self::ModelStarted {
                session_id: session_id.0.clone(),
                turn: turn_id.0,
            },
            RuntimeEvent::ModelCompleted {
                session_id,
                turn_id,
                tool_call_count,
            } => Self::ModelCompleted {
                session_id: session_id.0.clone(),
                turn: turn_id.0,
                tool_call_count: *tool_call_count,
            },
            RuntimeEvent::ModelFailed {
                session_id,
                turn_id,
                attempt,
                retrying,
                message,
            } => Self::ModelFailed {
                session_id: session_id.0.clone(),
                turn: turn_id.0,
                attempt: *attempt,
                retrying: *retrying,
                message: message.clone(),
            },
            RuntimeEvent::ToolBatchStarted {
                session_id,
                turn_id,
                count,
            } => Self::ToolBatchStarted {
                session_id: session_id.0.clone(),
                turn: turn_id.0,
                count: *count,
            },
            RuntimeEvent::ToolCallCompleted {
                call_id,
                tool_name,
                ok,
                error_code,
                error_message,
                details,
                ..
            } => Self::ToolCallCompleted {
                call_id: call_id.0.clone(),
                tool_id: tool_name.as_str().to_string(),
                ok: *ok,
                error_code: *error_code,
                error_message: error_message.clone(),
                details: details.clone(),
            },
            RuntimeEvent::ToolCallDenied {
                call_id,
                tool_name,
                error_code,
                reason,
                ..
            } => Self::ToolCallDenied {
                call_id: call_id.0.clone(),
                tool_id: tool_name.as_str().to_string(),
                error_code: *error_code,
                reason: reason.clone(),
            },
            RuntimeEvent::ArtifactCreated {
                artifact_id,
                tool_call_id,
                tool_name,
                bytes,
                content_hash,
                summary,
                details,
                ..
            } => Self::ArtifactCreated {
                artifact_id: artifact_id.clone(),
                tool_call_id: tool_call_id.0.clone(),
                tool_id: tool_name.as_str().to_string(),
                bytes: *bytes,
                content_hash: content_hash.clone(),
                summary: summary.clone(),
                details: details.clone(),
            },
            RuntimeEvent::FindingRecorded {
                finding_id,
                session_id,
                tool_call_id,
            } => Self::FindingRecorded {
                finding_id: finding_id.clone(),
                session_id: session_id.0.clone(),
                tool_call_id: tool_call_id.0.clone(),
            },
            RuntimeEvent::SearchBatchCompleted {
                searched_files,
                skipped_files,
                bytes_scanned,
                ms,
            } => Self::SearchBatchCompleted {
                searched_files: *searched_files,
                skipped_files: *skipped_files,
                bytes_scanned: *bytes_scanned,
                ms: *ms,
            },
            RuntimeEvent::SessionFinished { session_id, status } => Self::SessionFinished {
                session_id: session_id.0.clone(),
                status: status.clone(),
            },
            RuntimeEvent::SnapshotFinished {
                snapshot_id,
                sessions,
                completed_sessions,
            } => Self::SnapshotFinished {
                snapshot_id: snapshot_id.clone(),
                sessions: *sessions,
                completed_sessions: *completed_sessions,
            },
            RuntimeEvent::JobFinished { status } => Self::RunFinished {
                status: status.clone(),
            },
        }
    }
}

#[derive(Default)]
pub struct InMemoryReviewEventSink {
    records: Mutex<Vec<ReviewEventRecord>>,
}

impl std::fmt::Debug for InMemoryReviewEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryReviewEventSink")
            .field("records", &self.records().len())
            .finish()
    }
}

impl InMemoryReviewEventSink {
    pub fn records(&self) -> Vec<ReviewEventRecord> {
        self.records
            .lock()
            .expect("review event sink poisoned")
            .clone()
    }

    pub fn events(&self) -> Vec<ReviewEvent> {
        self.records()
            .into_iter()
            .map(|record| record.event)
            .collect()
    }
}

impl ReviewEventSink for InMemoryReviewEventSink {
    fn emit_review_event(&self, record: ReviewEventRecord) {
        self.records
            .lock()
            .expect("review event sink poisoned")
            .push(record);
    }
}

pub const REVIEW_EVENT_LOG_SCHEMA_VERSION: &str = "heimdaal.review-events.v1";

#[derive(Debug, Clone)]
pub struct ReviewEventJsonlManifest {
    pub path: PathBuf,
    pub schema_version: String,
    pub record_count: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct ReviewEventJsonlLoad {
    pub path: PathBuf,
    pub schema_version: String,
    pub record_count: usize,
    pub records: Vec<ReviewEventRecord>,
}

pub fn export_review_event_records_jsonl(
    path: impl AsRef<Path>,
    records: &[ReviewEventRecord],
) -> RuntimeResult<ReviewEventJsonlManifest> {
    write_review_event_records_jsonl(path.as_ref(), records)
}

pub fn load_review_event_records_jsonl(
    path: impl AsRef<Path>,
) -> RuntimeResult<ReviewEventJsonlLoad> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to read review event log: {error}"))
    })?;
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: ReviewEventJsonlRecord = serde_json::from_str(line).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "invalid review event log record at line {}: {error}",
                index + 1
            ))
        })?;
        if record.schema_version != REVIEW_EVENT_LOG_SCHEMA_VERSION {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported review event log schemaVersion {} at line {}",
                record.schema_version,
                index + 1
            )));
        }
        records.push(record.record);
    }
    Ok(ReviewEventJsonlLoad {
        path: path.to_path_buf(),
        schema_version: REVIEW_EVENT_LOG_SCHEMA_VERSION.to_string(),
        record_count: records.len(),
        records,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BorrowedReviewEventJsonlRecord<'a> {
    schema_version: &'static str,
    #[serde(flatten)]
    record: &'a ReviewEventRecord,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewEventJsonlRecord {
    schema_version: String,
    #[serde(flatten)]
    record: ReviewEventRecord,
}

fn write_review_event_records_jsonl(
    path: &Path,
    records: &[ReviewEventRecord],
) -> RuntimeResult<ReviewEventJsonlManifest> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::RepoUnavailable(format!(
                "failed to create review event log directory: {error}"
            ))
        })?;
    }
    let mut file = std::fs::File::create(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to create review event log: {error}"))
    })?;
    let mut bytes = 0usize;
    for record in records {
        let line = serde_json::to_vec(&BorrowedReviewEventJsonlRecord {
            schema_version: REVIEW_EVENT_LOG_SCHEMA_VERSION,
            record,
        })
        .map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to serialize review event log: {error}"))
        })?;
        file.write_all(&line).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write review event log: {error}"))
        })?;
        file.write_all(b"\n").map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write review event log: {error}"))
        })?;
        bytes += line.len() + 1;
    }
    file.flush().map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to flush review event log: {error}"))
    })?;
    Ok(ReviewEventJsonlManifest {
        path: path.to_path_buf(),
        schema_version: REVIEW_EVENT_LOG_SCHEMA_VERSION.to_string(),
        record_count: records.len(),
        bytes,
    })
}
