use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::contracts::ToolName;
use crate::runtime::contracts::*;
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::policy::{ReviewerPolicy, SessionEvidence};
use crate::runtime::tools::ToolEngine;

pub(crate) struct ToolBatchRunner<'a> {
    policy: &'a ReviewerPolicy,
    tools: &'a ToolEngine,
    events: &'a RuntimeEventDispatcher,
}

#[derive(Debug, Clone)]
struct RequestedToolCallTrace {
    tool_id: ToolId,
    argument_bytes: usize,
    argument_hash: String,
    argument_summary: Value,
}

#[derive(Debug, Clone)]
struct AcceptedToolCallRepair {
    call_id: ToolCallId,
    index: usize,
    original_tool_id: ToolId,
    canonical_call: ModelToolCall,
    error_code: ToolErrorCode,
    repair_kinds: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct RejectedToolCallRepair {
    call_id: ToolCallId,
    index: usize,
    tool_id: ToolId,
    error_code: ToolErrorCode,
    reason: &'static str,
    repair_kinds: Vec<&'static str>,
}

impl<'a> ToolBatchRunner<'a> {
    pub(crate) fn new(
        policy: &'a ReviewerPolicy,
        tools: &'a ToolEngine,
        events: &'a RuntimeEventDispatcher,
    ) -> Self {
        Self {
            policy,
            tools,
            events,
        }
    }

    pub(crate) async fn execute(
        &self,
        scope: SessionScope,
        turn_id: TurnId,
        calls: Vec<ModelToolCall>,
        evidence: &SessionEvidence,
        remaining_tool_calls: usize,
        cancel: CancellationToken,
    ) -> Vec<ToolResultEnvelope> {
        let requested_calls = calls
            .iter()
            .map(|call| {
                (
                    call.call_id.clone(),
                    RequestedToolCallTrace {
                        tool_id: call.name.clone(),
                        argument_bytes: call.raw_arguments.len(),
                        argument_hash: blake3::hash(call.raw_arguments.as_bytes())
                            .to_hex()
                            .to_string(),
                        argument_summary: call.redacted_argument_summary(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        self.events.emit_planned_runtime(self.policy.plan_agent_trace_event(
            &scope,
            Some(turn_id),
            "tool_calls_requested",
            format!("model requested {} tool call(s)", calls.len()),
            json!({
                "remainingToolCalls": remaining_tool_calls,
                "calls": calls.iter().map(|call| json!({
                    "callId": call.call_id.0,
                    "index": call.index,
                    "toolName": call.name.as_str(),
                    "argumentBytes": call.raw_arguments.len(),
                    "argumentHash": blake3::hash(call.raw_arguments.as_bytes()).to_hex().to_string(),
                    "argumentSummary": call.redacted_argument_summary(),
                })).collect::<Vec<_>>()
            }),
        ));
        let (calls, accepted_repairs, rejected_repairs) = self.repair_tool_calls(calls);
        for repair in &accepted_repairs {
            self.emit_tool_call_repair_trace(
                &scope,
                turn_id,
                repair.call_id.clone(),
                repair.index,
                repair.canonical_call.name.clone(),
                repair.error_code,
                "tool call was canonicalized before policy planning",
                requested_calls.get(&repair.call_id),
                Some(repair),
                Some(repair.repair_kinds.as_slice()),
                true,
                true,
            );
        }
        for repair in &rejected_repairs {
            self.emit_tool_call_repair_trace(
                &scope,
                turn_id,
                repair.call_id.clone(),
                repair.index,
                repair.tool_id.clone(),
                repair.error_code,
                repair.reason,
                requested_calls.get(&repair.call_id),
                None,
                Some(repair.repair_kinds.as_slice()),
                true,
                false,
            );
        }
        let plan = self
            .policy
            .plan_tool_batch(calls, evidence, remaining_tool_calls);
        let policy_denied_call_ids = plan
            .denied_calls
            .iter()
            .map(|denied| denied.call_id.clone())
            .collect::<HashSet<_>>();
        let rejected_repair_call_ids = rejected_repairs
            .iter()
            .map(|repair| repair.call_id.clone())
            .collect::<HashSet<_>>();
        for denied in &plan.denied_calls {
            self.emit_tool_call_repair_trace(
                &scope,
                turn_id,
                denied.call_id.clone(),
                denied.index,
                denied.tool_id.clone(),
                denied.denial.code,
                denied.denial.message.as_str(),
                requested_calls.get(&denied.call_id),
                None,
                None,
                false,
                false,
            );
        }
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                &scope,
                Some(turn_id),
                "tool_batch_planned",
                format!(
                    "tool policy scheduled {} call(s), denied {} call(s)",
                    plan.scheduled_count,
                    plan.denied_calls.len()
                ),
                json!({
                    "scheduledCount": plan.scheduled_count,
                    "deniedCount": plan.denied_calls.len(),
                    "allowed": plan.allowed_calls.iter().map(|call| json!({
                        "callId": call.call_id.0,
                        "index": call.index,
                        "toolId": call.name.as_str(),
                    })).collect::<Vec<_>>(),
                    "denied": plan.denied_calls.iter().map(|call| json!({
                        "callId": call.call_id.0,
                        "index": call.index,
                        "toolId": call.tool_id.as_str(),
                        "code": call.denial.code,
                        "retryable": call.denial.retryable,
                        "reason": call.denial.message,
                    })).collect::<Vec<_>>(),
                }),
            ));
        if let Some(planned) =
            self.policy
                .plan_tool_batch_started_runtime_event(&scope, turn_id, plan.scheduled_count)
        {
            self.events.emit_planned_runtime(planned);
        }
        let mut indexed_results = Vec::new();
        for denied in plan.denied_calls {
            let result = self.tools.error_result(
                denied.call_id,
                denied.tool_id,
                denied.denial.code,
                &denied.denial.message,
                denied.denial.retryable,
            );
            self.tools
                .record_tool_metrics(std::slice::from_ref(&result));
            indexed_results.push((denied.index, result));
        }

        if !plan.allowed_calls.is_empty() {
            let allowed_indices = plan
                .allowed_calls
                .iter()
                .map(|call| call.index)
                .collect::<Vec<_>>();
            let allowed_results = self
                .tools
                .execute_batch(scope.clone(), turn_id, plan.allowed_calls, cancel)
                .await;
            for (index, result) in allowed_indices.into_iter().zip(allowed_results) {
                indexed_results.push((index, result));
            }
        }
        indexed_results.sort_by_key(|(index, _)| *index);
        for (index, result) in &indexed_results {
            if policy_denied_call_ids.contains(&result.tool_call_id) {
                continue;
            }
            if rejected_repair_call_ids.contains(&result.tool_call_id) {
                continue;
            }
            if let Some(error) = result
                .error
                .as_ref()
                .filter(|error| trace_tool_call_repair(error.code))
            {
                self.emit_tool_call_repair_trace(
                    &scope,
                    turn_id,
                    result.tool_call_id.clone(),
                    *index,
                    result.tool_name.clone(),
                    error.code,
                    error.message.as_str(),
                    requested_calls.get(&result.tool_call_id),
                    None,
                    None,
                    false,
                    false,
                );
            }
        }
        indexed_results
            .into_iter()
            .map(|(_, result)| result)
            .collect()
    }

    fn repair_tool_calls(
        &self,
        calls: Vec<ModelToolCall>,
    ) -> (
        Vec<ModelToolCall>,
        Vec<AcceptedToolCallRepair>,
        Vec<RejectedToolCallRepair>,
    ) {
        let mut repaired_calls = Vec::with_capacity(calls.len());
        let mut accepted_repairs = Vec::new();
        let mut rejected_repairs = Vec::new();
        for call in calls {
            let mut canonical = call.clone();
            let original_tool_id = call.name.clone();
            let mut repair_kinds = Vec::new();
            let mut rejected_repair_kinds = Vec::new();

            if self.tools.registry.definition(&canonical.name).is_none() {
                if let Some(tool_id) = self.tools.registry.tool_id_for_model_alias(&canonical.name)
                {
                    if tool_id != canonical.name {
                        canonical.name = tool_id;
                        repair_kinds.push("tool_alias");
                    }
                }
            }

            if let Some(builtin) = canonical.name.as_builtin() {
                match repair_builtin_arguments(builtin, &canonical.raw_arguments) {
                    Some(BuiltinArgumentRepair::Accepted {
                        arguments,
                        repair_kind,
                    }) => {
                        if arguments != canonical.raw_arguments {
                            canonical.raw_arguments = arguments;
                            repair_kinds.push(repair_kind);
                        }
                    }
                    Some(BuiltinArgumentRepair::Rejected { repair_kind }) => {
                        rejected_repair_kinds.push(repair_kind);
                    }
                    None => {}
                }
            }

            if !repair_kinds.is_empty() {
                let error_code = if repair_kinds.iter().any(|kind| *kind != "tool_alias") {
                    ToolErrorCode::InvalidArgs
                } else {
                    ToolErrorCode::UnknownTool
                };
                accepted_repairs.push(AcceptedToolCallRepair {
                    call_id: canonical.call_id.clone(),
                    index: canonical.index,
                    original_tool_id: original_tool_id.clone(),
                    canonical_call: canonical.clone(),
                    error_code,
                    repair_kinds,
                });
            }
            if !rejected_repair_kinds.is_empty() {
                rejected_repairs.push(RejectedToolCallRepair {
                    call_id: canonical.call_id.clone(),
                    index: canonical.index,
                    tool_id: canonical.name.clone(),
                    error_code: ToolErrorCode::InvalidArgs,
                    reason: "tool call arguments matched a repair shape but could not be safely canonicalized",
                    repair_kinds: rejected_repair_kinds,
                });
            }
            repaired_calls.push(canonical);
        }
        (repaired_calls, accepted_repairs, rejected_repairs)
    }

    fn emit_tool_call_repair_trace(
        &self,
        scope: &SessionScope,
        turn_id: TurnId,
        call_id: ToolCallId,
        index: usize,
        tool_id: ToolId,
        error_code: ToolErrorCode,
        reason: &str,
        requested_call: Option<&RequestedToolCallTrace>,
        accepted_repair: Option<&AcceptedToolCallRepair>,
        repair_kinds: Option<&[&'static str]>,
        repair_attempted: bool,
        repair_accepted: bool,
    ) {
        let (argument_bytes, argument_hash, argument_summary, requested_tool_id) = requested_call
            .map(|call| {
                (
                    call.argument_bytes,
                    call.argument_hash.clone(),
                    call.argument_summary.clone(),
                    call.tool_id.clone(),
                )
            })
            .unwrap_or_else(|| (0, String::new(), json!(null), tool_id.clone()));
        let original_tool_id = accepted_repair
            .map(|repair| repair.original_tool_id.clone())
            .unwrap_or(requested_tool_id);
        let canonical = accepted_repair.map(|repair| {
            json!({
                "canonicalToolId": repair.canonical_call.name.as_str(),
                "canonicalArgumentBytes": repair.canonical_call.raw_arguments.len(),
                "canonicalArgumentHash": blake3::hash(repair.canonical_call.raw_arguments.as_bytes()).to_hex().to_string(),
                "canonicalArgumentSummary": repair.canonical_call.redacted_argument_summary(),
                "repairKinds": repair.repair_kinds,
            })
        });
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                scope,
                Some(turn_id),
                "tool_call_repair",
                if repair_accepted {
                    format!("tool call {} was repaired: {reason}", call_id.0)
                } else {
                    format!("tool call {} was not repaired: {reason}", call_id.0)
                },
                json!({
                    "callId": call_id.0,
                    "index": index,
                    "toolId": tool_id.as_str(),
                    "originalToolId": original_tool_id.as_str(),
                    "errorCode": error_code,
                    "reason": reason,
                    "repairAttempted": repair_attempted,
                    "repairAccepted": repair_accepted,
                    "repairKinds": repair_kinds.unwrap_or(&[]),
                    "argumentBytes": argument_bytes,
                    "argumentHash": argument_hash,
                    "argumentSummary": argument_summary,
                    "acceptedRepair": canonical,
                }),
            ));
    }
}

enum BuiltinArgumentRepair {
    Accepted {
        arguments: String,
        repair_kind: &'static str,
    },
    Rejected {
        repair_kind: &'static str,
    },
}

fn repair_builtin_arguments(tool: ToolName, raw: &str) -> Option<BuiltinArgumentRepair> {
    match tool {
        ToolName::ListChangedFiles | ToolName::ReadDiff | ToolName::ListFiles => {
            repair_empty_arguments(raw).map(|arguments| BuiltinArgumentRepair::Accepted {
                arguments,
                repair_kind: "empty_args",
            })
        }
        ToolName::ReadFile
        | ToolName::ReadBaseFile
        | ToolName::ReadHeadFile
        | ToolName::FindRelatedFiles
        | ToolName::FindTestsForFile
        | ToolName::ListImports => repair_path_arguments(raw, "path_args"),
        ToolName::ReadFileRange => repair_range_arguments(raw),
        ToolName::SearchText => repair_search_arguments(raw, "search_args"),
    }
}

fn repair_empty_arguments(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some("{}".to_string());
    }
    let parsed = serde_json::from_str::<Value>(trimmed).ok()?;
    if parsed.is_null() || parsed.as_array().is_some_and(Vec::is_empty) {
        Some("{}".to_string())
    } else {
        None
    }
}

fn repair_path_arguments(raw: &str, repair_kind: &'static str) -> Option<BuiltinArgumentRepair> {
    let parsed = serde_json::from_str::<Value>(raw).ok()?;
    let path = match parsed {
        Value::String(path) => match normalize_path_string(&path) {
            Some(path) => path,
            None => return Some(BuiltinArgumentRepair::Rejected { repair_kind }),
        },
        Value::Object(ref object) => {
            if !has_any_key(object, &["path", "file", "filepath", "filename"]) {
                return None;
            }
            match path_field(object) {
                Some(path) => path,
                None => return Some(BuiltinArgumentRepair::Rejected { repair_kind }),
            }
        }
        _ => return None,
    };
    Some(BuiltinArgumentRepair::Accepted {
        arguments: json_string(json!({ "path": path })),
        repair_kind,
    })
}

fn repair_search_arguments(raw: &str, repair_kind: &'static str) -> Option<BuiltinArgumentRepair> {
    let parsed = serde_json::from_str::<Value>(raw).ok()?;
    let query = match parsed {
        Value::String(query) => match non_empty_trimmed_string(&query) {
            Some(query) => query,
            None => return Some(BuiltinArgumentRepair::Rejected { repair_kind }),
        },
        Value::Object(ref object) => {
            if !has_any_key(object, &["query", "pattern", "text"]) {
                return None;
            }
            match string_field(object, &["query", "pattern", "text"]) {
                Some(query) => query,
                None => return Some(BuiltinArgumentRepair::Rejected { repair_kind }),
            }
        }
        _ => return None,
    };
    Some(BuiltinArgumentRepair::Accepted {
        arguments: json_string(json!({ "query": query })),
        repair_kind,
    })
}

fn repair_range_arguments(raw: &str) -> Option<BuiltinArgumentRepair> {
    let repair_kind = "range_args";
    let Value::Object(object) = serde_json::from_str::<Value>(raw).ok()? else {
        return None;
    };
    if !has_any_key(
        &object,
        &[
            "path",
            "file",
            "filepath",
            "filename",
            "line",
            "start_line",
            "startLine",
            "start",
            "end_line",
            "endLine",
            "end",
        ],
    ) {
        return None;
    }
    let Some(path) = path_field(&object) else {
        return Some(BuiltinArgumentRepair::Rejected { repair_kind });
    };
    let line = usize_field(&object, &["line"]);
    let Some(mut start_line) = usize_field(&object, &["start_line", "startLine", "start"]).or(line)
    else {
        return Some(BuiltinArgumentRepair::Rejected { repair_kind });
    };
    let Some(mut end_line) = usize_field(&object, &["end_line", "endLine", "end"]).or(line) else {
        return Some(BuiltinArgumentRepair::Rejected { repair_kind });
    };
    start_line = start_line.max(1);
    end_line = end_line.max(1);
    if start_line > end_line {
        std::mem::swap(&mut start_line, &mut end_line);
    }
    Some(BuiltinArgumentRepair::Accepted {
        arguments: json_string(json!({
            "path": path,
            "start_line": start_line,
            "end_line": end_line
        })),
        repair_kind,
    })
}

fn path_field(object: &Map<String, Value>) -> Option<String> {
    string_field(object, &["path", "file", "filepath", "filename"])
        .and_then(|path| normalize_path_string(&path))
}

fn string_field(object: &Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
        .and_then(non_empty_trimmed_string)
}

fn has_any_key(object: &Map<String, Value>, names: &[&str]) -> bool {
    names.iter().any(|name| object.contains_key(*name))
}

fn usize_field(object: &Map<String, Value>, names: &[&str]) -> Option<usize> {
    names.iter().find_map(|name| {
        let value = object.get(*name)?;
        if let Some(value) = value.as_u64() {
            return usize::try_from(value).ok();
        }
        if let Some(value) = value.as_i64() {
            return usize::try_from(value).ok();
        }
        value.as_str()?.trim().parse::<usize>().ok()
    })
}

fn normalize_path_string(input: &str) -> Option<String> {
    let mut path = non_empty_trimmed_string(input)?;
    while let Some(stripped) = path.strip_prefix("./") {
        path = stripped.to_string();
    }
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn non_empty_trimmed_string(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn json_string(value: Value) -> String {
    serde_json::to_string(&value).expect("canonical repair arguments serialize")
}

fn trace_tool_call_repair(error_code: ToolErrorCode) -> bool {
    matches!(
        error_code,
        ToolErrorCode::InvalidArgs
            | ToolErrorCode::UnknownTool
            | ToolErrorCode::ToolNotAllowed
            | ToolErrorCode::PathDenied
            | ToolErrorCode::BudgetExceeded
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::contracts::{
        AgentBudget, ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus,
        PathPolicyV1, RenameDetection, Role, SnapshotMode, ToolName,
    };
    use crate::runtime::repo::RepoSnapshot;

    #[derive(Default)]
    struct RecordingRuntimeSink {
        records: Mutex<Vec<(RuntimeEventContext, RuntimeEvent)>>,
    }

    impl RuntimeEventSink for RecordingRuntimeSink {
        fn emit(&self, event: RuntimeEvent) {
            self.emit_with_context(RuntimeEventContext::from_event(&event), event);
        }

        fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
            self.records
                .lock()
                .expect("sink lock")
                .push((context, event));
        }
    }

    #[tokio::test]
    async fn tool_batch_runner_applies_policy_denials_and_preserves_model_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("README.md"), "needle\n").expect("write readme");
        let change = test_change_with_file("README.md");
        let snapshot = RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64, 20), &change)
            .expect("snapshot");
        let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
        let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
        let policy = ReviewerPolicy::new();
        let runtime_sink = Arc::new(RecordingRuntimeSink::default());
        let sink: Arc<dyn RuntimeEventSink> = runtime_sink.clone();
        let dispatcher = RuntimeEventDispatcher::new(Some(sink));
        let runner = ToolBatchRunner::new(&policy, &tools, &dispatcher);
        let scope = test_scope("tool-batch-session");

        let results = runner
            .execute(
                scope.clone(),
                TurnId(3),
                vec![
                    model_call("diff", 0, ToolName::ReadDiff, "{}"),
                    model_call("files", 1, ToolName::ListFiles, "{}"),
                    model_call("changed", 2, ToolName::ListChangedFiles, "{}"),
                ],
                &SessionEvidence::default(),
                2,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].tool_call_id, ToolCallId("diff".to_string()));
        assert!(results[0].ok);
        assert_eq!(results[1].tool_call_id, ToolCallId("files".to_string()));
        assert!(results[1].ok);
        assert_eq!(results[2].tool_call_id, ToolCallId("changed".to_string()));
        assert_eq!(
            results[2].error.as_ref().expect("denial").code,
            ToolErrorCode::BudgetExceeded
        );
        let records = runtime_sink.records.lock().expect("sink lock");
        assert!(records.iter().any(|(context, event)| {
            context.session_id.as_ref() == Some(&scope.id)
                && context.turn_id == Some(TurnId(3))
                && matches!(event, RuntimeEvent::ToolBatchStarted { count: 2, .. })
        }));
        let denied_repair_trace = records
            .iter()
            .find_map(|(_, event)| match event {
                RuntimeEvent::AgentTrace {
                    trace_kind,
                    details,
                    ..
                } if trace_kind == "tool_call_repair" && details["callId"] == "changed" => {
                    Some(details)
                }
                _ => None,
            })
            .expect("budget-denied no-repair trace");
        assert_eq!(denied_repair_trace["errorCode"], "budget_exceeded");
        assert_eq!(denied_repair_trace["repairAttempted"], false);
        assert_eq!(denied_repair_trace["repairAccepted"], false);
    }

    #[tokio::test]
    async fn tool_batch_runner_traces_unrepaired_invalid_tool_calls() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("README.md"), "needle\n").expect("write readme");
        let change = test_change_with_file("README.md");
        let snapshot = RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64, 20), &change)
            .expect("snapshot");
        let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
        let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
        let policy = ReviewerPolicy::new();
        let runtime_sink = Arc::new(RecordingRuntimeSink::default());
        let sink: Arc<dyn RuntimeEventSink> = runtime_sink.clone();
        let dispatcher = RuntimeEventDispatcher::new(Some(sink));
        let runner = ToolBatchRunner::new(&policy, &tools, &dispatcher);
        let scope = test_scope("tool-repair-trace");

        let results = runner
            .execute(
                scope.clone(),
                TurnId(7),
                vec![
                    ModelToolCall {
                        call_id: ToolCallId("bad-json".to_string()),
                        index: 0,
                        name: ToolId::from(ToolName::ReadFile),
                        raw_arguments: "not-json".to_string(),
                    },
                    ModelToolCall {
                        call_id: ToolCallId("unknown-tool".to_string()),
                        index: 1,
                        name: ToolId::parse("missing_tool").unwrap(),
                        raw_arguments: "{}".to_string(),
                    },
                    ModelToolCall {
                        call_id: ToolCallId("bad-path-shape".to_string()),
                        index: 2,
                        name: ToolId::from(ToolName::ReadFile),
                        raw_arguments: r#"{"file":123}"#.to_string(),
                    },
                ],
                &SessionEvidence::default(),
                4,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].error.as_ref().expect("bad json").code,
            ToolErrorCode::InvalidArgs
        );
        assert_eq!(
            results[1].error.as_ref().expect("unknown").code,
            ToolErrorCode::UnknownTool
        );
        assert_eq!(
            results[2].error.as_ref().expect("bad path shape").code,
            ToolErrorCode::InvalidArgs
        );
        let records = runtime_sink.records.lock().expect("sink lock");
        let repair_traces = records
            .iter()
            .filter_map(|(_, event)| match event {
                RuntimeEvent::AgentTrace {
                    trace_kind,
                    details,
                    ..
                } if trace_kind == "tool_call_repair" => Some(details),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(repair_traces.len(), 3);
        assert!(repair_traces.iter().any(|details| {
            details["callId"] == "bad-json"
                && details["errorCode"] == "invalid_args"
                && details["repairAttempted"] == false
                && details["repairAccepted"] == false
        }));
        assert!(repair_traces.iter().any(|details| {
            details["callId"] == "unknown-tool"
                && details["errorCode"] == "unknown_tool"
                && details["repairAttempted"] == false
                && details["repairAccepted"] == false
        }));
        let rejected_path_trace = repair_traces
            .iter()
            .find(|details| details["callId"] == "bad-path-shape")
            .expect("rejected path repair trace");
        assert_eq!(rejected_path_trace["errorCode"], "invalid_args");
        assert_eq!(rejected_path_trace["repairAttempted"], true);
        assert_eq!(rejected_path_trace["repairAccepted"], false);
        assert_eq!(rejected_path_trace["repairKinds"][0], "path_args");
    }

    #[tokio::test]
    async fn tool_batch_runner_repairs_aliases_and_loose_arguments_before_policy() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("README.md"), "first\nneedle\nthird\n")
            .expect("write readme");
        let change = test_change_with_file("README.md");
        let snapshot = RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64, 20), &change)
            .expect("snapshot");
        let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
        let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
        let policy = ReviewerPolicy::new();
        let runtime_sink = Arc::new(RecordingRuntimeSink::default());
        let sink: Arc<dyn RuntimeEventSink> = runtime_sink.clone();
        let dispatcher = RuntimeEventDispatcher::new(Some(sink));
        let runner = ToolBatchRunner::new(&policy, &tools, &dispatcher);
        let scope = test_scope("tool-repair-accepted");

        let results = runner
            .execute(
                scope,
                TurnId(9),
                vec![
                    ModelToolCall {
                        call_id: ToolCallId("read-alias".to_string()),
                        index: 0,
                        name: ToolId::parse("read").unwrap(),
                        raw_arguments: "\"./README.md\"".to_string(),
                    },
                    ModelToolCall {
                        call_id: ToolCallId("grep-alias".to_string()),
                        index: 1,
                        name: ToolId::parse("grep").unwrap(),
                        raw_arguments: "\"needle\"".to_string(),
                    },
                    ModelToolCall {
                        call_id: ToolCallId("range-args".to_string()),
                        index: 2,
                        name: ToolId::from(ToolName::ReadFileRange),
                        raw_arguments: r#"{"file":"README.md","startLine":2,"endLine":1}"#
                            .to_string(),
                    },
                ],
                &SessionEvidence::default(),
                6,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|result| result.ok));
        assert_eq!(results[0].tool_name, ToolId::from(ToolName::ReadFile));
        assert_eq!(results[1].tool_name, ToolId::from(ToolName::SearchText));
        assert_eq!(results[2].tool_name, ToolId::from(ToolName::ReadFileRange));

        let records = runtime_sink.records.lock().expect("sink lock");
        let repair_traces = records
            .iter()
            .filter_map(|(_, event)| match event {
                RuntimeEvent::AgentTrace {
                    trace_kind,
                    details,
                    ..
                } if trace_kind == "tool_call_repair" => Some(details),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(repair_traces.len(), 3);

        let read_trace = repair_traces
            .iter()
            .find(|details| details["callId"] == "read-alias")
            .expect("read repair trace");
        assert_eq!(read_trace["originalToolId"], "read");
        assert_eq!(read_trace["toolId"], "read_file");
        assert_eq!(read_trace["repairAttempted"], true);
        assert_eq!(read_trace["repairAccepted"], true);
        assert_eq!(
            read_trace["acceptedRepair"]["canonicalArgumentSummary"]["path"],
            "README.md"
        );
        assert_repair_kind(read_trace, "tool_alias");
        assert_repair_kind(read_trace, "path_args");

        let grep_trace = repair_traces
            .iter()
            .find(|details| details["callId"] == "grep-alias")
            .expect("grep repair trace");
        assert_eq!(grep_trace["originalToolId"], "grep");
        assert_eq!(grep_trace["toolId"], "search_text");
        assert_eq!(
            grep_trace["acceptedRepair"]["canonicalArgumentSummary"]["query"],
            "needle"
        );
        assert_repair_kind(grep_trace, "tool_alias");
        assert_repair_kind(grep_trace, "search_args");

        let range_trace = repair_traces
            .iter()
            .find(|details| details["callId"] == "range-args")
            .expect("range repair trace");
        assert_eq!(range_trace["originalToolId"], "read_file_range");
        assert_eq!(range_trace["toolId"], "read_file_range");
        assert_eq!(
            range_trace["acceptedRepair"]["canonicalArgumentSummary"]["startLine"],
            1
        );
        assert_eq!(
            range_trace["acceptedRepair"]["canonicalArgumentSummary"]["endLine"],
            2
        );
        assert_repair_kind(range_trace, "range_args");
    }

    fn model_call(id: &str, index: usize, tool: ToolName, raw_arguments: &str) -> ModelToolCall {
        ModelToolCall {
            call_id: ToolCallId(id.to_string()),
            index,
            name: ToolId::from(tool),
            raw_arguments: raw_arguments.to_string(),
        }
    }

    fn assert_repair_kind(details: &Value, expected: &str) {
        let kinds = details["acceptedRepair"]["repairKinds"]
            .as_array()
            .expect("repair kinds");
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "missing repair kind {expected}: {details}"
        );
    }

    fn test_scope(id: &str) -> SessionScope {
        SessionScope::review_read_only(
            SessionId(id.to_string()),
            Role::Generalist,
            "tool batch runner test",
            AgentBudget {
                max_turns: 4,
                max_tool_calls: 8,
                max_prompt_tokens: 32_000,
                max_output_tokens: 512,
                budget_source: crate::contracts::BudgetSource::PlannedDefault,
            },
        )
    }

    fn test_change_with_file(path: &str) -> ChangeScopeV1 {
        ChangeScopeV1 {
            kind: ChangeKind::LocalDiff,
            change_id: "test".to_string(),
            source_ref: "head".to_string(),
            target_ref: "base".to_string(),
            base_revision_id: "base".to_string(),
            head_revision_id: "head".to_string(),
            merge_base_revision_id: None,
            changed_files_manifest_ref: None,
            diff_manifest_ref: None,
            inline_diff: None,
            snapshot_mode: SnapshotMode::WorktreeHead,
            rename_detection: RenameDetection::None,
            changed_files: vec![ChangedFileEntryV1 {
                status: ChangedFileStatus::Modified,
                old_path: Some(PathBuf::from(path)),
                new_path: Some(PathBuf::from(path)),
                old_content_hash: None,
                new_content_hash: None,
                is_binary: false,
                is_generated: false,
            }],
        }
    }
}
