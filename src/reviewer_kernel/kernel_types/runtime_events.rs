use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    ArtifactId, CacheStatus, SessionId, SnapshotId, ToolCallId, ToolErrorCode, ToolId,
    ToolProviderId, TurnId,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RuntimeEvent {
    JobStarted {
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
    ContextIndexFailed {
        snapshot_id: SnapshotId,
        message: String,
    },
    ContextPackStarted {
        session_id: Option<SessionId>,
        purpose: String,
    },
    ContextPackCompleted {
        pack_id: String,
        session_id: Option<SessionId>,
        purpose: String,
        evidence_count: usize,
        omitted_count: usize,
        used_tokens: usize,
        sufficiency: String,
        ms: u64,
    },
    ContextPackFailed {
        session_id: Option<SessionId>,
        purpose: String,
        message: String,
        ms: u64,
    },
    ContextQueryCompleted {
        session_id: Option<SessionId>,
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
        session_id: SessionId,
    },
    ModelStarted {
        session_id: SessionId,
        turn_id: TurnId,
    },
    AgentTrace {
        session_id: SessionId,
        turn_id: Option<TurnId>,
        trace_kind: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        details: Value,
    },
    ModelCompleted {
        session_id: SessionId,
        turn_id: TurnId,
        tool_call_count: usize,
    },
    ModelFailed {
        session_id: SessionId,
        turn_id: TurnId,
        attempt: usize,
        retrying: bool,
        message: String,
    },
    ToolBatchStarted {
        session_id: SessionId,
        turn_id: TurnId,
        count: usize,
    },
    ToolCallCompleted {
        call_id: ToolCallId,
        tool_name: ToolId,
        provider_id: ToolProviderId,
        cache_status: CacheStatus,
        output_bytes: usize,
        ok: bool,
        error_code: Option<ToolErrorCode>,
        error_message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    ToolCallDenied {
        call_id: ToolCallId,
        tool_name: ToolId,
        provider_id: ToolProviderId,
        error_code: ToolErrorCode,
        reason: String,
    },
    ArtifactCreated {
        artifact_id: ArtifactId,
        tool_call_id: ToolCallId,
        tool_name: ToolId,
        provider_id: ToolProviderId,
        bytes: usize,
        content_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    FindingRecorded {
        finding_id: String,
        session_id: SessionId,
        tool_call_id: ToolCallId,
    },
    SearchBatchCompleted {
        searched_files: usize,
        skipped_files: usize,
        bytes_scanned: usize,
        ms: u64,
    },
    SessionFinished {
        session_id: SessionId,
        status: String,
    },
    SnapshotFinished {
        snapshot_id: SnapshotId,
        sessions: usize,
        completed_sessions: usize,
    },
    JobFinished {
        status: String,
    },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEventContext {
    pub run_id: Option<String>,
    pub snapshot_id: Option<SnapshotId>,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub tool_call_id: Option<ToolCallId>,
    pub artifact_id: Option<ArtifactId>,
    pub finding_id: Option<String>,
}

impl RuntimeEventContext {
    pub fn from_event(event: &RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::JobStarted { snapshot_id }
            | RuntimeEvent::SnapshotStarted { snapshot_id }
            | RuntimeEvent::ContextIndexStarted { snapshot_id }
            | RuntimeEvent::ContextIndexCompleted { snapshot_id, .. }
            | RuntimeEvent::ContextIndexFailed { snapshot_id, .. }
            | RuntimeEvent::SnapshotFinished { snapshot_id, .. } => Self {
                snapshot_id: Some(snapshot_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ContextPackStarted { session_id, .. }
            | RuntimeEvent::ContextPackCompleted { session_id, .. }
            | RuntimeEvent::ContextPackFailed { session_id, .. }
            | RuntimeEvent::ContextQueryCompleted { session_id, .. } => Self {
                session_id: session_id.clone(),
                ..Self::default()
            },
            RuntimeEvent::RepoManifestCompleted { .. } | RuntimeEvent::JobFinished { .. } => {
                Self::default()
            }
            RuntimeEvent::SessionStarted { session_id }
            | RuntimeEvent::SessionFinished { session_id, .. } => Self {
                session_id: Some(session_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ModelStarted {
                session_id,
                turn_id,
            }
            | RuntimeEvent::AgentTrace {
                session_id,
                turn_id: Some(turn_id),
                ..
            }
            | RuntimeEvent::ModelCompleted {
                session_id,
                turn_id,
                ..
            }
            | RuntimeEvent::ModelFailed {
                session_id,
                turn_id,
                ..
            }
            | RuntimeEvent::ToolBatchStarted {
                session_id,
                turn_id,
                ..
            } => Self {
                session_id: Some(session_id.clone()),
                turn_id: Some(*turn_id),
                ..Self::default()
            },
            RuntimeEvent::AgentTrace {
                session_id,
                turn_id: None,
                ..
            } => Self {
                session_id: Some(session_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ToolCallCompleted { call_id, .. }
            | RuntimeEvent::ToolCallDenied { call_id, .. } => Self {
                tool_call_id: Some(call_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::ArtifactCreated {
                artifact_id,
                tool_call_id,
                ..
            } => Self {
                tool_call_id: Some(tool_call_id.clone()),
                artifact_id: Some(artifact_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::FindingRecorded {
                finding_id,
                session_id,
                tool_call_id,
            } => Self {
                session_id: Some(session_id.clone()),
                tool_call_id: Some(tool_call_id.clone()),
                finding_id: Some(finding_id.clone()),
                ..Self::default()
            },
            RuntimeEvent::SearchBatchCompleted { .. } => Self::default(),
        }
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_default_snapshot_id(mut self, snapshot_id: SnapshotId) -> Self {
        if self.snapshot_id.is_none() {
            self.snapshot_id = Some(snapshot_id);
        }
        self
    }
}

pub trait RuntimeEventSink: Send + Sync {
    fn emit(&self, event: RuntimeEvent);

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let _ = context;
        self.emit(event);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEventRecord {
    pub seq: u64,
    pub timestamp_utc: String,
    pub context: RuntimeEventContext,
    pub event: RuntimeEvent,
}
