use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    ArtifactId, CapabilitySet, EvidenceId, FileId, RepoPath, ScopeKey, SessionId, SnapshotId,
    ToolCallId, ToolId, ToolProviderId, TurnId,
};
use crate::reviewer_kernel::review_contract::{TokenUsage, ToolName};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationItem {
    System {
        content: String,
    },
    User {
        content: String,
    },
    AssistantText {
        content: String,
    },
    AssistantToolCalls {
        calls: Vec<ModelToolCall>,
    },
    ToolResult {
        call_id: ToolCallId,
        name: ToolId,
        content: Box<ToolResultEnvelope>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelToolCall {
    pub call_id: ToolCallId,
    pub index: usize,
    pub name: ToolId,
    pub raw_arguments: String,
}

impl ModelToolCall {
    pub(crate) fn redacted_argument_summary(&self) -> Value {
        redacted_tool_argument_summary(&self.name, &self.raw_arguments)
    }
}

fn redacted_tool_argument_summary(tool_id: &ToolId, raw_arguments: &str) -> Value {
    let Ok(arguments) = serde_json::from_str::<Value>(raw_arguments) else {
        return serde_json::json!({ "parseable": false });
    };
    match tool_id.as_builtin() {
        Some(ToolName::ListChangedFiles | ToolName::ReadDiff | ToolName::ListFiles) => {
            serde_json::json!({ "parseable": true })
        }
        Some(
            ToolName::ReadFile
            | ToolName::ReadBaseFile
            | ToolName::ReadHeadFile
            | ToolName::FindRelatedFiles
            | ToolName::FindTestsForFile
            | ToolName::ListImports,
        ) => serde_json::json!({
            "parseable": true,
            "path": arguments
                .get("path")
                .and_then(Value::as_str)
                .map(compact_trace_string),
        }),
        Some(ToolName::ReadFileRange) => serde_json::json!({
            "parseable": true,
            "path": arguments
                .get("path")
                .and_then(Value::as_str)
                .map(compact_trace_string),
            "startLine": arguments.get("start_line").or_else(|| arguments.get("startLine")).cloned(),
            "endLine": arguments.get("end_line").or_else(|| arguments.get("endLine")).cloned(),
        }),
        Some(ToolName::SearchText) => serde_json::json!({
            "parseable": true,
            "query": arguments
                .get("query")
                .and_then(Value::as_str)
                .map(compact_trace_string),
        }),
        None => {
            let keys = arguments
                .as_object()
                .map(|object| object.keys().take(20).cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            serde_json::json!({
                "parseable": true,
                "keys": keys,
            })
        }
    }
}

fn compact_trace_string(value: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= MAX_CHARS {
            output.push_str("...");
            return output;
        }
        output.push(ch);
    }
    output
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelTurn {
    Text {
        content: String,
        usage: TokenUsage,
    },
    ToolCalls {
        calls: Vec<ModelToolCall>,
        usage: TokenUsage,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ToolInvocation {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) call_id: ToolCallId,
    pub(crate) tool_id: ToolId,
    pub(crate) builtin_name: Option<ToolName>,
    pub(crate) input_bytes: usize,
    pub(crate) args: ToolArgs,
    pub(crate) capabilities: CapabilitySet,
    pub(crate) scope_key: ScopeKey,
    pub(crate) assigned_changed_files: Vec<RepoPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArgs {
    Empty,
    ReadFile {
        path: RepoPath,
    },
    ReadFileRange {
        path: RepoPath,
        start_line: usize,
        end_line: usize,
    },
    SearchText {
        query: String,
    },
    Raw(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEnvelope {
    pub ok: bool,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolId,
    pub provider_id: ToolProviderId,
    pub snapshot_id: SnapshotId,
    pub artifact_id: Option<ArtifactId>,
    pub cache: CacheInfo,
    pub limits: LimitInfo,
    pub data: Option<Value>,
    pub error: Option<ToolErrorInfo>,
}

impl ToolResultEnvelope {
    pub(crate) fn for_call(
        &self,
        call_id: ToolCallId,
        tool_name: ToolId,
        cache_status: CacheStatus,
    ) -> Self {
        let mut cloned = self.clone();
        cloned.tool_call_id = call_id;
        cloned.tool_name = tool_name;
        cloned.cache.status = cache_status;
        cloned
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheInfo {
    pub status: CacheStatus,
    pub key_hash: Option<String>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Hit,
    Miss,
    Deduped,
    NotCacheable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitInfo {
    pub truncated: bool,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub latency_ms: u64,
    pub queue_wait_ms: u64,
    pub searched_files: usize,
    pub skipped_files: usize,
    pub bytes_scanned: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolErrorInfo {
    pub code: ToolErrorCode,
    pub message: String,
    pub retryable: bool,
    pub partial: bool,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    InvalidArgs,
    UnknownTool,
    ToolNotAllowed,
    PathDenied,
    NotFound,
    NotText,
    TooLarge,
    TooManyMatches,
    SnapshotStale,
    Timeout,
    Cancelled,
    BudgetExceeded,
    QueueFull,
    RepoUnavailable,
    RedactionFailed,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub evidence_id: EvidenceId,
    pub snapshot_id: SnapshotId,
    pub file_id: Option<FileId>,
    pub path: Option<RepoPath>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub snippet_hash: String,
    pub artifact_id: ArtifactId,
}
