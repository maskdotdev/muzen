use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::json;

use crate::reviewer_kernel::events::{ReviewEvent, ReviewEventRecord};
use crate::reviewer_kernel::runtime_events::{
    EventSink as RuntimeEventSink, RuntimeEvent, RuntimeEventContext, RuntimeEventRecord,
};
use crate::reviewer_kernel::system::timestamp_utc;

use super::transport::RunnerCallbackTransport;

pub(crate) struct StreamingRunnerEventSink {
    transport: Arc<dyn RunnerCallbackTransport>,
    next_seq: AtomicU64,
}

impl StreamingRunnerEventSink {
    pub(crate) fn new(transport: Arc<dyn RunnerCallbackTransport>) -> Self {
        Self {
            transport,
            next_seq: AtomicU64::new(1),
        }
    }
}

impl RuntimeEventSink for StreamingRunnerEventSink {
    fn emit(&self, event: RuntimeEvent) {
        self.emit_with_context(RuntimeEventContext::from_event(&event), event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let runtime_record = RuntimeEventRecord {
            seq,
            timestamp_utc: timestamp_utc(),
            context: context.clone(),
            event: event.clone(),
        };
        let _ = self
            .transport
            .notify("event.runtime", json!(runtime_record));
        let review_record = ReviewEventRecord {
            seq,
            timestamp_utc: timestamp_utc(),
            run_id: context.run_id,
            snapshot_id: context.snapshot_id,
            session_id: context.session_id.map(|id| id.0),
            turn: context.turn_id.map(|turn| turn.0),
            tool_call_id: context.tool_call_id.map(|id| id.0),
            artifact_id: context.artifact_id,
            finding_id: context.finding_id,
            event: review_event_from_runtime(&event),
        };
        let _ = self.transport.notify("event.review", json!(review_record));
    }
}

fn review_event_from_runtime(event: &RuntimeEvent) -> ReviewEvent {
    match event {
        RuntimeEvent::JobStarted { snapshot_id } => ReviewEvent::RunStarted {
            snapshot_id: snapshot_id.clone(),
        },
        RuntimeEvent::SnapshotStarted { snapshot_id } => ReviewEvent::SnapshotStarted {
            snapshot_id: snapshot_id.clone(),
        },
        RuntimeEvent::ContextIndexStarted { snapshot_id } => ReviewEvent::ContextIndexStarted {
            snapshot_id: snapshot_id.clone(),
        },
        RuntimeEvent::ContextIndexCompleted {
            snapshot_id,
            index_id,
            evidence_count,
            indexed_files,
            skipped_files,
            ms,
        } => ReviewEvent::ContextIndexCompleted {
            snapshot_id: snapshot_id.clone(),
            index_id: index_id.clone(),
            evidence_count: *evidence_count,
            indexed_files: *indexed_files,
            skipped_files: *skipped_files,
            ms: *ms,
        },
        RuntimeEvent::ContextIndexFailed {
            snapshot_id,
            message,
        } => ReviewEvent::ContextIndexFailed {
            snapshot_id: snapshot_id.clone(),
            message: message.clone(),
        },
        RuntimeEvent::ContextPackStarted {
            session_id,
            purpose,
        } => ReviewEvent::ContextPackStarted {
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
        } => ReviewEvent::ContextPackCompleted {
            pack_id: pack_id.clone(),
            session_id: session_id.as_ref().map(|id| id.0.clone()),
            purpose: purpose.clone(),
            evidence_count: *evidence_count,
            omitted_count: *omitted_count,
            used_tokens: *used_tokens,
            sufficiency: sufficiency.clone(),
            ms: *ms,
        },
        RuntimeEvent::ContextPackFailed {
            session_id,
            purpose,
            message,
            ms,
        } => ReviewEvent::ContextPackFailed {
            session_id: session_id.as_ref().map(|id| id.0.clone()),
            purpose: purpose.clone(),
            message: message.clone(),
            ms: *ms,
        },
        RuntimeEvent::ContextQueryCompleted {
            session_id,
            query_kind,
            result_count,
            artifact_id,
            ms,
        } => ReviewEvent::ContextQueryCompleted {
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
        } => ReviewEvent::RepoManifestCompleted {
            files: *files,
            skipped: *skipped,
            bytes: *bytes,
            ms: *ms,
        },
        RuntimeEvent::SessionStarted { session_id } => ReviewEvent::SessionStarted {
            session_id: session_id.0.clone(),
        },
        RuntimeEvent::ModelStarted {
            session_id,
            turn_id,
        } => ReviewEvent::ModelStarted {
            session_id: session_id.0.clone(),
            turn: turn_id.0,
        },
        RuntimeEvent::AgentTrace {
            session_id,
            turn_id,
            trace_kind,
            summary,
            details,
        } => ReviewEvent::AgentTrace {
            session_id: session_id.0.clone(),
            turn: turn_id.map(|turn| turn.0),
            trace_kind: trace_kind.clone(),
            summary: summary.clone(),
            details: details.clone(),
        },
        RuntimeEvent::ModelCompleted {
            session_id,
            turn_id,
            tool_call_count,
        } => ReviewEvent::ModelCompleted {
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
        } => ReviewEvent::ModelFailed {
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
        } => ReviewEvent::ToolBatchStarted {
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
        } => ReviewEvent::ToolCallCompleted {
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
        } => ReviewEvent::ToolCallDenied {
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
        } => ReviewEvent::ArtifactCreated {
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
        } => ReviewEvent::FindingRecorded {
            finding_id: finding_id.clone(),
            session_id: session_id.0.clone(),
            tool_call_id: tool_call_id.0.clone(),
        },
        RuntimeEvent::SearchBatchCompleted {
            searched_files,
            skipped_files,
            bytes_scanned,
            ms,
        } => ReviewEvent::SearchBatchCompleted {
            searched_files: *searched_files,
            skipped_files: *skipped_files,
            bytes_scanned: *bytes_scanned,
            ms: *ms,
        },
        RuntimeEvent::SessionFinished { session_id, status } => ReviewEvent::SessionFinished {
            session_id: session_id.0.clone(),
            status: status.clone(),
        },
        RuntimeEvent::SnapshotFinished {
            snapshot_id,
            sessions,
            completed_sessions,
        } => ReviewEvent::SnapshotFinished {
            snapshot_id: snapshot_id.clone(),
            sessions: *sessions,
            completed_sessions: *completed_sessions,
        },
        RuntimeEvent::JobFinished { status } => ReviewEvent::RunFinished {
            status: status.clone(),
        },
    }
}
