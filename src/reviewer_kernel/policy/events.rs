use serde_json::{json, Value};

use crate::reviewer_kernel::kernel_types::{
    ArtifactView, RuntimeError, RuntimeEvent, RuntimeEventContext, SessionId, SessionScope,
    ToolErrorCode, ToolResultEnvelope, TurnId,
};
use crate::reviewer_kernel::review_contract::ToolName;
use crate::reviewer_kernel::system::redact_known_secrets;

use super::ReviewerPolicy;
use crate::reviewer_kernel::policy::transcript::{compact_string_array, truncate_chars};
impl ReviewerPolicy {
    pub(crate) fn plan_session_started_runtime_event(
        &self,
        scope: &SessionScope,
    ) -> PlannedRuntimeEvent {
        planned_runtime_event(RuntimeEvent::SessionStarted {
            session_id: scope.id.clone(),
        })
    }

    pub(crate) fn plan_session_finished_runtime_event(
        &self,
        scope: &SessionScope,
        status: &str,
    ) -> PlannedRuntimeEvent {
        planned_runtime_event(RuntimeEvent::SessionFinished {
            session_id: scope.id.clone(),
            status: status.to_string(),
        })
    }

    pub(crate) fn plan_model_started_runtime_event(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
    ) -> PlannedRuntimeEvent {
        planned_runtime_event(RuntimeEvent::ModelStarted {
            session_id: scope.id.clone(),
            turn_id,
        })
    }

    pub(crate) fn plan_agent_trace_event(
        &self,
        scope: &SessionScope,
        turn_id: Option<TurnId>,
        trace_kind: impl Into<String>,
        summary: impl Into<String>,
        details: Value,
    ) -> PlannedRuntimeEvent {
        planned_runtime_event(RuntimeEvent::AgentTrace {
            session_id: scope.id.clone(),
            turn_id,
            trace_kind: trace_kind.into(),
            summary: truncate_chars(&redact_known_secrets(&summary.into(), &[]), 300),
            details,
        })
    }

    pub(crate) fn plan_model_completed_runtime_event(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        tool_call_count: usize,
    ) -> PlannedRuntimeEvent {
        planned_runtime_event(RuntimeEvent::ModelCompleted {
            session_id: scope.id.clone(),
            turn_id,
            tool_call_count,
        })
    }

    pub(crate) fn plan_model_failed_runtime_event(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        attempt: usize,
        retrying: bool,
        error: &RuntimeError,
    ) -> PlannedRuntimeEvent {
        planned_runtime_event(RuntimeEvent::ModelFailed {
            session_id: scope.id.clone(),
            turn_id,
            attempt,
            retrying,
            message: redacted_error_message(error),
        })
    }

    pub(crate) fn plan_tool_batch_started_runtime_event(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        count: usize,
    ) -> Option<PlannedRuntimeEvent> {
        if count == 0 {
            return None;
        }
        Some(planned_runtime_event(RuntimeEvent::ToolBatchStarted {
            session_id: scope.id.clone(),
            turn_id,
            count,
        }))
    }

    pub(crate) fn plan_tool_result_runtime_events(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        result: &ToolResultEnvelope,
        artifact: Option<&ArtifactView>,
        finding_id: Option<&str>,
    ) -> ToolResultRuntimeEventPlan {
        let mut events = Vec::new();
        if let Some(finding_id) = finding_id {
            let event = RuntimeEvent::FindingRecorded {
                finding_id: finding_id.to_string(),
                session_id: scope.id.clone(),
                tool_call_id: result.tool_call_id.clone(),
            };
            events.push(planned_runtime_event(event));
        }
        if let Some(artifact) = artifact {
            events.push(PlannedRuntimeEvent {
                context: result_event_context(
                    &scope.id,
                    turn_id,
                    result,
                    Some(artifact.artifact_id.clone()),
                ),
                event: RuntimeEvent::ArtifactCreated {
                    artifact_id: artifact.artifact_id.clone(),
                    tool_call_id: result.tool_call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    provider_id: result.provider_id.clone(),
                    bytes: artifact.bytes,
                    content_hash: artifact.content_hash.clone(),
                    summary: Some(artifact_event_summary(result)),
                    details: artifact_event_details(result),
                },
            });
        }
        if let Some(error) = result.error.as_ref() {
            if is_policy_denial(error.code) {
                events.push(PlannedRuntimeEvent {
                    context: result_event_context(&scope.id, turn_id, result, None),
                    event: RuntimeEvent::ToolCallDenied {
                        call_id: result.tool_call_id.clone(),
                        tool_name: result.tool_name.clone(),
                        provider_id: result.provider_id.clone(),
                        error_code: error.code,
                        reason: error.message.clone(),
                    },
                });
            }
        }
        events.push(PlannedRuntimeEvent {
            context: result_event_context(&scope.id, turn_id, result, None),
            event: RuntimeEvent::ToolCallCompleted {
                call_id: result.tool_call_id.clone(),
                tool_name: result.tool_name.clone(),
                provider_id: result.provider_id.clone(),
                cache_status: result.cache.status,
                output_bytes: result.limits.output_bytes,
                ok: result.ok,
                error_code: result.error.as_ref().map(|error| error.code),
                error_message: result.error.as_ref().map(|error| error.message.clone()),
                details: tool_call_completed_details(result),
            },
        });
        if result.ok && result.tool_name.as_builtin() == Some(ToolName::SearchText) {
            events.push(PlannedRuntimeEvent {
                context: result_event_context(&scope.id, turn_id, result, None),
                event: RuntimeEvent::SearchBatchCompleted {
                    searched_files: result.limits.searched_files,
                    skipped_files: result.limits.skipped_files,
                    bytes_scanned: result.limits.bytes_scanned,
                    ms: 0,
                },
            });
        }
        ToolResultRuntimeEventPlan { events }
    }
}

pub(crate) struct ToolResultRuntimeEventPlan {
    pub(crate) events: Vec<PlannedRuntimeEvent>,
}

#[derive(Debug)]
pub(crate) struct PlannedRuntimeEvent {
    pub(crate) context: RuntimeEventContext,
    pub(crate) event: RuntimeEvent,
}

fn artifact_event_summary(result: &ToolResultEnvelope) -> String {
    let data = result.data.as_ref();
    let summary = match result.tool_name.as_builtin() {
        Some(ToolName::ReadDiff) => {
            let hash = data
                .and_then(|value| value.get("contentHash"))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            format!("diff artifact contentHash={hash}")
        }
        Some(ToolName::ListChangedFiles) => {
            let files = data
                .and_then(|value| value.get("changedFiles"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("changed files: {files}")
        }
        Some(ToolName::ListFiles) => {
            let files = data
                .and_then(|value| value.get("files"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("listed files: {files}")
        }
        Some(
            ToolName::ReadFile
            | ToolName::ReadFileRange
            | ToolName::ReadBaseFile
            | ToolName::ReadHeadFile,
        ) => {
            let path = data
                .and_then(|value| value.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            format!("read file artifact {path}")
        }
        Some(ToolName::SearchText) => {
            let query = data
                .and_then(|value| value.get("query"))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let matches = data
                .and_then(|value| value.get("returnedMatches"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!("search_text query={query} matches={matches}")
        }
        Some(ToolName::FindRelatedFiles | ToolName::FindTestsForFile) => {
            let path = data
                .and_then(|value| value.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let files = data
                .and_then(|value| value.get("files"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("file relation artifact {path} files={files}")
        }
        Some(ToolName::ListImports) => {
            let path = data
                .and_then(|value| value.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let imports = data
                .and_then(|value| value.get("imports"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("imports artifact {path} imports={imports}")
        }
        _ => format!("{} artifact", result.tool_name.as_str()),
    };
    truncate_chars(&redact_known_secrets(&summary, &[]), 240)
}

fn tool_call_completed_details(result: &ToolResultEnvelope) -> Option<Value> {
    match result.tool_name.as_builtin() {
        Some(
            ToolName::ReadDiff
            | ToolName::ListChangedFiles
            | ToolName::ListFiles
            | ToolName::ReadFile
            | ToolName::ReadFileRange
            | ToolName::ReadBaseFile
            | ToolName::ReadHeadFile,
        )
        | Some(ToolName::SearchText)
        | Some(ToolName::FindRelatedFiles | ToolName::FindTestsForFile)
        | Some(ToolName::ListImports) => artifact_event_details(result),
        _ => None,
    }
}

fn artifact_event_details(result: &ToolResultEnvelope) -> Option<Value> {
    let data = result.data.as_ref()?;
    let details = match result.tool_name.as_builtin() {
        Some(ToolName::ReadDiff) => json!({
            "contentHash": data.get("contentHash").cloned(),
        }),
        Some(ToolName::ListChangedFiles) => json!({
            "changedFiles": compact_string_array(data.get("changedFiles"), 120, 300),
        }),
        Some(ToolName::ListFiles) => json!({
            "files": compact_string_array(data.get("files"), 120, 300),
        }),
        Some(
            ToolName::ReadFile
            | ToolName::ReadFileRange
            | ToolName::ReadBaseFile
            | ToolName::ReadHeadFile,
        ) => json!({
            "path": data.get("path").cloned(),
            "available": data.get("available").cloned(),
            "lineRange": data.get("lineRange").cloned(),
            "message": data
                .get("message")
                .and_then(Value::as_str)
                .map(|value| truncate_chars(value, 400)),
        }),
        Some(ToolName::SearchText) => json!({
            "query": data.get("query").cloned(),
            "searchedFiles": data.get("searchedFiles").cloned(),
            "skippedFiles": data.get("skippedFiles").cloned(),
            "bytesScanned": data.get("bytesScanned").cloned(),
            "returnedMatches": data.get("returnedMatches").cloned(),
            "truncated": data.get("truncated").cloned(),
            "firstMatch": data.get("firstMatch").cloned(),
        }),
        Some(ToolName::FindRelatedFiles | ToolName::FindTestsForFile) => json!({
            "path": data.get("path").cloned(),
            "files": compact_string_array(data.get("files"), 120, 300),
        }),
        Some(ToolName::ListImports) => json!({
            "path": data.get("path").cloned(),
            "imports": compact_string_array(data.get("imports"), 120, 300),
        }),
        _ => return None,
    };
    Some(details)
}

fn redacted_error_message(error: &RuntimeError) -> String {
    redact_known_secrets(&format!("{error:#}"), &[])
}
fn is_policy_denial(code: ToolErrorCode) -> bool {
    matches!(
        code,
        ToolErrorCode::ToolNotAllowed | ToolErrorCode::PathDenied | ToolErrorCode::BudgetExceeded
    )
}

fn planned_runtime_event(event: RuntimeEvent) -> PlannedRuntimeEvent {
    PlannedRuntimeEvent {
        context: RuntimeEventContext::from_event(&event),
        event,
    }
}

fn result_event_context(
    session_id: &SessionId,
    turn_id: TurnId,
    result: &ToolResultEnvelope,
    artifact_id: Option<crate::reviewer_kernel::kernel_types::ArtifactId>,
) -> RuntimeEventContext {
    RuntimeEventContext {
        session_id: Some(session_id.clone()),
        turn_id: Some(turn_id),
        tool_call_id: Some(result.tool_call_id.clone()),
        artifact_id,
        ..RuntimeEventContext::default()
    }
}
