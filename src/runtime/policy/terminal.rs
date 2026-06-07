use crate::contracts::{ToolCounts, ToolName};
use crate::runtime::contracts::{
    SessionScope, SessionTerminalDiagnostic, ToolErrorCode, ToolResultEnvelope,
};
use crate::util::redact_known_secrets;

use super::{ReviewerPolicy, SessionEvidence};
impl ReviewerPolicy {
    pub(crate) fn observe_terminal_batch(
        &self,
        terminal: &mut SessionTerminal,
        results: &[ToolResultEnvelope],
    ) -> bool {
        terminal.observe_batch(results)
    }

    pub(crate) fn observe_terminal_error(
        &self,
        terminal: &mut SessionTerminal,
        result: &ToolResultEnvelope,
    ) {
        terminal.observe_error(result);
    }

    pub(crate) fn should_fail_after_terminal_errors(&self, terminal: &SessionTerminal) -> bool {
        terminal.denied_tool_errors >= 2
    }
    pub(crate) fn session_state(
        &self,
        completed: bool,
        terminal_seen: bool,
        cancelled: bool,
        failed: bool,
    ) -> &'static str {
        if cancelled {
            "cancelled"
        } else if completed {
            "done"
        } else if failed || terminal_seen {
            "failed"
        } else {
            "budget_exhausted"
        }
    }

    pub(crate) fn session_terminal_diagnostic(
        &self,
        scope: &SessionScope,
        completed: bool,
        evidence: &SessionEvidence,
        terminal: &SessionTerminal,
        model_calls: usize,
        tool_counts: ToolCounts,
    ) -> SessionTerminalDiagnostic {
        SessionTerminalDiagnostic {
            session_id: scope.id.0.clone(),
            completed,
            terminal_tool: terminal.tool(),
            terminal_summary: terminal.summary(),
            saw_diff: evidence.saw_diff(),
            saw_file: evidence.saw_file(),
            saw_search: evidence.saw_search(),
            model_calls,
            tool_counts,
        }
    }

    pub(crate) fn empty_session_terminal_diagnostic(
        &self,
        scope: &SessionScope,
        completed: bool,
        terminal_summary: Option<String>,
    ) -> SessionTerminalDiagnostic {
        SessionTerminalDiagnostic {
            session_id: scope.id.0.clone(),
            completed,
            terminal_tool: None,
            terminal_summary,
            saw_diff: false,
            saw_file: false,
            saw_search: false,
            model_calls: 0,
            tool_counts: ToolCounts::default(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct SessionTerminal {
    seen: bool,
    tool: Option<String>,
    summary: Option<String>,
    denied_tool_errors: usize,
}

impl SessionTerminal {
    fn observe_batch(&mut self, results: &[ToolResultEnvelope]) -> bool {
        let terminal = results.iter().any(is_successful_terminal);
        if let Some(result) = results.iter().find(|result| is_successful_terminal(result)) {
            self.tool = Some(result.tool_name.as_str().to_string());
            self.summary = terminal_result_summary(result);
        }
        self.seen |= terminal;
        terminal
    }

    fn observe_error(&mut self, result: &ToolResultEnvelope) {
        if !result.ok
            && matches!(
                result.error.as_ref().map(|error| error.code),
                Some(ToolErrorCode::ToolNotAllowed)
            )
            && !result.error.as_ref().is_some_and(|error| error.retryable)
        {
            self.denied_tool_errors += 1;
        }
    }

    pub(crate) fn seen(&self) -> bool {
        self.seen
    }

    pub(crate) fn tool(&self) -> Option<String> {
        self.tool.clone()
    }

    pub(crate) fn summary(&self) -> Option<String> {
        self.summary.clone()
    }
}
fn is_successful_terminal(result: &ToolResultEnvelope) -> bool {
    result.tool_name.as_builtin() == Some(ToolName::Finish) && result.ok
}

fn terminal_result_summary(result: &ToolResultEnvelope) -> Option<String> {
    let data = result.data.as_ref()?;
    let raw = match result.tool_name.as_builtin() {
        Some(ToolName::RecordFinding) => data
            .get("title")
            .and_then(serde_json::Value::as_str)
            .or_else(|| data.get("claim").and_then(serde_json::Value::as_str)),
        Some(ToolName::Finish) => data.get("reason").and_then(serde_json::Value::as_str),
        _ => None,
    }?;
    Some(truncate_summary(&redact_known_secrets(raw, &[]), 240))
}

pub(crate) fn truncate_summary(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str(" [truncated]");
    }
    output
}
