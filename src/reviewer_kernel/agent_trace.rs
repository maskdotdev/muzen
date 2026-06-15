use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::reviewer_kernel::kernel_types::{RuntimeEvent, RuntimeEventRecord};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTraceReport {
    pub sessions: Vec<AgentTraceSession>,
    pub total_events: usize,
    pub agent_trace_events: usize,
    pub model_turns: usize,
    pub tool_calls_requested: usize,
    pub tool_calls_completed: usize,
    pub tool_calls_denied: usize,
    pub findings_recorded: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTraceSession {
    pub session_id: String,
    pub turns: Vec<AgentTraceTurn>,
    pub session_events: Vec<AgentTraceEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTraceTurn {
    pub turn: u32,
    pub entries: Vec<AgentTraceEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTraceEntry {
    pub seq: u64,
    pub timestamp_utc: String,
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

pub fn build_agent_trace_report(records: &[RuntimeEventRecord]) -> AgentTraceReport {
    let mut totals = AgentTraceReportTotals::default();
    let mut sessions = BTreeMap::<String, SessionBuilder>::new();
    for record in records {
        totals.total_events += 1;
        count_event(&mut totals, &record.event);
        let Some(session_id) = event_session_id(record) else {
            continue;
        };
        let entry = event_entry(record);
        let session = sessions.entry(session_id).or_default();
        match event_turn(record) {
            Some(turn) => session.turns.entry(turn).or_default().push(entry),
            None => session.session_events.push(entry),
        }
    }
    AgentTraceReport {
        sessions: sessions
            .into_iter()
            .map(|(session_id, session)| AgentTraceSession {
                session_id,
                turns: session
                    .turns
                    .into_iter()
                    .map(|(turn, entries)| AgentTraceTurn { turn, entries })
                    .collect(),
                session_events: session.session_events,
            })
            .collect(),
        total_events: totals.total_events,
        agent_trace_events: totals.agent_trace_events,
        model_turns: totals.model_turns,
        tool_calls_requested: totals.tool_calls_requested,
        tool_calls_completed: totals.tool_calls_completed,
        tool_calls_denied: totals.tool_calls_denied,
        findings_recorded: totals.findings_recorded,
    }
}

#[derive(Default)]
struct SessionBuilder {
    turns: BTreeMap<u32, Vec<AgentTraceEntry>>,
    session_events: Vec<AgentTraceEntry>,
}

#[derive(Default)]
struct AgentTraceReportTotals {
    total_events: usize,
    agent_trace_events: usize,
    model_turns: usize,
    tool_calls_requested: usize,
    tool_calls_completed: usize,
    tool_calls_denied: usize,
    findings_recorded: usize,
}

fn count_event(totals: &mut AgentTraceReportTotals, event: &RuntimeEvent) {
    match event {
        RuntimeEvent::AgentTrace {
            trace_kind,
            details,
            ..
        } => {
            totals.agent_trace_events += 1;
            if trace_kind == "tool_calls_requested" {
                totals.tool_calls_requested += requested_tool_call_count(details);
            }
        }
        RuntimeEvent::ModelCompleted { .. } => {
            totals.model_turns += 1;
        }
        RuntimeEvent::ToolCallCompleted { .. } => {
            totals.tool_calls_completed += 1;
        }
        RuntimeEvent::ToolCallDenied { .. } => {
            totals.tool_calls_denied += 1;
        }
        RuntimeEvent::FindingRecorded { .. } => {
            totals.findings_recorded += 1;
        }
        _ => {}
    }
}

fn requested_tool_call_count(details: &Value) -> usize {
    details
        .get("calls")
        .and_then(Value::as_array)
        .filter(|calls| !calls.is_empty())
        .map_or(1, Vec::len)
}

fn event_session_id(record: &RuntimeEventRecord) -> Option<String> {
    record
        .context
        .session_id
        .as_ref()
        .map(|id| id.0.clone())
        .or_else(|| match &record.event {
            RuntimeEvent::AgentTrace { session_id, .. }
            | RuntimeEvent::ModelStarted { session_id, .. }
            | RuntimeEvent::ModelCompleted { session_id, .. }
            | RuntimeEvent::ModelFailed { session_id, .. }
            | RuntimeEvent::ToolBatchStarted { session_id, .. }
            | RuntimeEvent::SessionStarted { session_id }
            | RuntimeEvent::SessionFinished { session_id, .. }
            | RuntimeEvent::FindingRecorded { session_id, .. } => Some(session_id.0.clone()),
            _ => None,
        })
}

fn event_turn(record: &RuntimeEventRecord) -> Option<u32> {
    record
        .context
        .turn_id
        .map(|turn| turn.0)
        .or_else(|| match &record.event {
            RuntimeEvent::AgentTrace { turn_id, .. } => turn_id.map(|turn| turn.0),
            RuntimeEvent::ModelStarted { turn_id, .. }
            | RuntimeEvent::ModelCompleted { turn_id, .. }
            | RuntimeEvent::ModelFailed { turn_id, .. }
            | RuntimeEvent::ToolBatchStarted { turn_id, .. } => Some(turn_id.0),
            _ => None,
        })
}

fn event_entry(record: &RuntimeEventRecord) -> AgentTraceEntry {
    match &record.event {
        RuntimeEvent::AgentTrace {
            trace_kind,
            summary,
            details,
            ..
        } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: trace_kind.clone(),
            summary: summary.clone(),
            details: details.clone(),
        },
        RuntimeEvent::ModelStarted { .. } => simple_entry(record, "model_started"),
        RuntimeEvent::ModelCompleted {
            tool_call_count, ..
        } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "model_completed".to_string(),
            summary: format!("model completed with {tool_call_count} tool call(s)"),
            details: serde_json::json!({ "toolCallCount": tool_call_count }),
        },
        RuntimeEvent::ModelFailed {
            attempt,
            retrying,
            message,
            ..
        } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "model_failed".to_string(),
            summary: message.clone(),
            details: serde_json::json!({
                "attempt": attempt,
                "retrying": retrying,
            }),
        },
        RuntimeEvent::ToolBatchStarted { count, .. } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "tool_batch_started".to_string(),
            summary: format!("started {count} scheduled tool call(s)"),
            details: serde_json::json!({ "count": count }),
        },
        RuntimeEvent::ToolCallCompleted {
            call_id,
            tool_name,
            ok,
            error_code,
            output_bytes,
            cache_status,
            details,
            ..
        } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "tool_call_completed".to_string(),
            summary: format!("{} {} ok={ok}", tool_name.as_str(), call_id.0),
            details: serde_json::json!({
                "callId": call_id.0,
                "toolId": tool_name.as_str(),
                "ok": ok,
                "errorCode": error_code,
                "outputBytes": output_bytes,
                "cacheStatus": cache_status,
                "result": details,
            }),
        },
        RuntimeEvent::ToolCallDenied {
            call_id,
            tool_name,
            error_code,
            reason,
            ..
        } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "tool_call_denied".to_string(),
            summary: reason.clone(),
            details: serde_json::json!({
                "callId": call_id.0,
                "toolId": tool_name.as_str(),
                "errorCode": error_code,
            }),
        },
        RuntimeEvent::ArtifactCreated {
            artifact_id,
            tool_call_id,
            tool_name,
            bytes,
            content_hash,
            summary,
            ..
        } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "artifact_created".to_string(),
            summary: summary
                .clone()
                .unwrap_or_else(|| "artifact created".to_string()),
            details: serde_json::json!({
                "artifactId": artifact_id.0,
                "toolCallId": tool_call_id.0,
                "toolId": tool_name.as_str(),
                "bytes": bytes,
                "contentHash": content_hash,
            }),
        },
        RuntimeEvent::FindingRecorded { finding_id, .. } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "finding_recorded".to_string(),
            summary: format!("finding recorded {finding_id}"),
            details: serde_json::json!({ "findingId": finding_id }),
        },
        RuntimeEvent::SessionStarted { .. } => simple_entry(record, "session_started"),
        RuntimeEvent::SessionFinished { status, .. } => AgentTraceEntry {
            seq: record.seq,
            timestamp_utc: record.timestamp_utc.clone(),
            kind: "session_finished".to_string(),
            summary: status.clone(),
            details: Value::Null,
        },
        _ => simple_entry(record, "runtime_event"),
    }
}

fn simple_entry(record: &RuntimeEventRecord, kind: &str) -> AgentTraceEntry {
    AgentTraceEntry {
        seq: record.seq,
        timestamp_utc: record.timestamp_utc.clone(),
        kind: kind.to_string(),
        summary: kind.to_string(),
        details: Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::reviewer_kernel::kernel_types::{
        CacheStatus, RuntimeEventContext, SessionId, ToolCallId, ToolId, ToolProviderId, TurnId,
    };

    #[test]
    fn groups_agent_trace_records_by_session_and_turn() {
        let session_id = SessionId("session-a".to_string());
        let records = vec![
            record(
                1,
                RuntimeEventContext {
                    session_id: Some(session_id.clone()),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::SessionStarted {
                    session_id: session_id.clone(),
                },
            ),
            record(
                2,
                RuntimeEventContext {
                    session_id: Some(session_id.clone()),
                    turn_id: Some(TurnId(0)),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::AgentTrace {
                    session_id: session_id.clone(),
                    turn_id: Some(TurnId(0)),
                    trace_kind: "model_turn_prepared".to_string(),
                    summary: "prepared".to_string(),
                    details: json!({"exposedTools": [{"modelName": "read"}]}),
                },
            ),
            record(
                3,
                RuntimeEventContext {
                    session_id: Some(session_id.clone()),
                    turn_id: Some(TurnId(0)),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::AgentTrace {
                    session_id: session_id.clone(),
                    turn_id: Some(TurnId(0)),
                    trace_kind: "tool_calls_requested".to_string(),
                    summary: "requested tools".to_string(),
                    details: json!({
                        "calls": [
                            {"callId": "call-1", "toolId": "read_file"},
                            {"callId": "call-2", "toolId": "search_text"}
                        ]
                    }),
                },
            ),
            record(
                4,
                RuntimeEventContext {
                    session_id: Some(session_id.clone()),
                    turn_id: Some(TurnId(0)),
                    tool_call_id: Some(ToolCallId("call-1".to_string())),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::ToolCallCompleted {
                    call_id: ToolCallId("call-1".to_string()),
                    tool_name: ToolId::parse("read_file").unwrap(),
                    provider_id: ToolProviderId::builtin_review(),
                    cache_status: CacheStatus::Miss,
                    output_bytes: 12,
                    ok: true,
                    error_code: None,
                    error_message: None,
                    details: None,
                },
            ),
            record(
                5,
                RuntimeEventContext {
                    session_id: Some(session_id.clone()),
                    tool_call_id: Some(ToolCallId("finding-call".to_string())),
                    finding_id: Some("finding-1".to_string()),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::FindingRecorded {
                    finding_id: "finding-1".to_string(),
                    session_id: session_id.clone(),
                    tool_call_id: ToolCallId("finding-call".to_string()),
                },
            ),
        ];

        let report = build_agent_trace_report(&records);

        assert_eq!(report.total_events, 5);
        assert_eq!(report.agent_trace_events, 2);
        assert_eq!(report.tool_calls_requested, 2);
        assert_eq!(report.tool_calls_completed, 1);
        assert_eq!(report.findings_recorded, 1);
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].session_id, "session-a");
        assert_eq!(report.sessions[0].session_events.len(), 2);
        assert_eq!(report.sessions[0].turns.len(), 1);
        assert_eq!(report.sessions[0].turns[0].turn, 0);
        assert_eq!(report.sessions[0].turns[0].entries.len(), 3);
    }

    fn record(seq: u64, context: RuntimeEventContext, event: RuntimeEvent) -> RuntimeEventRecord {
        RuntimeEventRecord {
            seq,
            timestamp_utc: "2026-06-13T00:00:00Z".to_string(),
            context,
            event,
        }
    }
}
