use std::collections::BTreeSet;

use serde_json::Value;

use crate::contracts::ToolName;
use crate::runtime::contracts::{ModelToolCall, SessionScope, ToolErrorCode, ToolResultEnvelope};

use super::{ReviewerPolicy, ToolPolicyDenial};
impl ReviewerPolicy {
    pub(crate) fn observe_evidence_result(
        &self,
        evidence: &mut SessionEvidence,
        result: &ToolResultEnvelope,
    ) {
        evidence.observe(result);
    }
}

#[derive(Debug, Default)]
pub(crate) struct SessionEvidence {
    pub(crate) saw_diff: bool,
    pub(crate) saw_file: bool,
    pub(crate) saw_search: bool,
    pub(crate) changed_files: BTreeSet<String>,
    pub(crate) read_files: BTreeSet<String>,
    pub(crate) reviewed_files: BTreeSet<String>,
    pub(crate) fixed_changed_file_scope: bool,
    pub(crate) results: Vec<ToolResultEnvelope>,
}

impl SessionEvidence {
    const SMALL_CHANGED_FILE_SCOPE: usize = 24;

    pub(crate) fn for_scope(scope: &SessionScope) -> Self {
        let changed_files = assigned_changed_files(scope);
        Self {
            fixed_changed_file_scope: !changed_files.is_empty(),
            changed_files,
            ..Self::default()
        }
    }

    pub(crate) fn ready(&self) -> bool {
        self.saw_diff && self.saw_file && self.saw_search
    }

    pub(crate) fn ready_to_finish(&self) -> bool {
        self.ready() && self.changed_file_coverage_ready()
    }

    pub(crate) fn file_review_scope_denial(
        &self,
        call: &ModelToolCall,
    ) -> Option<ToolPolicyDenial> {
        if call.name.as_builtin() != Some(ToolName::RecordFileReview)
            || !self.fixed_changed_file_scope
            || self.changed_files.is_empty()
        {
            return None;
        }
        let value = serde_json::from_str::<Value>(&call.raw_arguments).ok()?;
        let path = value.get("path")?.as_str()?.trim();
        if path.is_empty() || self.changed_files.contains(path) {
            return None;
        }
        Some(ToolPolicyDenial {
            code: ToolErrorCode::ToolNotAllowed,
            message: format!(
                "record_file_review is limited to this session's assigned changed file(s): {}; do not record verdicts for related files inspected for context",
                self.changed_files
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            retryable: true,
        })
    }

    pub(crate) fn finding_scope_denial(&self, call: &ModelToolCall) -> Option<ToolPolicyDenial> {
        if call.name.as_builtin() != Some(ToolName::RecordFinding)
            || !self.fixed_changed_file_scope
            || self.changed_files.is_empty()
        {
            return None;
        }
        let value = serde_json::from_str::<Value>(&call.raw_arguments).ok()?;
        let path = value.get("path")?.as_str()?.trim();
        if path.is_empty() || self.changed_files.contains(path) {
            return None;
        }
        Some(ToolPolicyDenial {
            code: ToolErrorCode::ToolNotAllowed,
            message: format!(
                "record_finding is limited to this session's assigned changed file(s): {}; use related files only as evidence, and let their own batch record findings for them",
                self.changed_files
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            retryable: true,
        })
    }

    pub(crate) fn finish_coverage_denial_message(&self) -> String {
        let missing_read = self.missing_read_files(8);
        let missing_review = self.missing_review_files(8);
        let mut message = "finish requires reading and recording a file-review verdict for every listed changed file when the changed-file scope is small".to_string();
        if !missing_read.is_empty() {
            message.push_str("; missing reads: ");
            message.push_str(&missing_read.join(", "));
        }
        if !missing_review.is_empty() {
            message.push_str("; missing file reviews: ");
            message.push_str(&missing_review.join(", "));
        }
        message
    }

    pub(crate) fn missing_read_files(&self, limit: usize) -> Vec<String> {
        self.changed_files
            .iter()
            .filter(|path| !self.read_files.contains(*path))
            .take(limit)
            .cloned()
            .collect()
    }

    pub(crate) fn missing_review_files(&self, limit: usize) -> Vec<String> {
        self.changed_files
            .iter()
            .filter(|path| !self.reviewed_files.contains(*path))
            .take(limit)
            .cloned()
            .collect()
    }

    fn changed_file_coverage_ready(&self) -> bool {
        if self.changed_files.is_empty()
            || self.changed_files.len() > Self::SMALL_CHANGED_FILE_SCOPE
        {
            return true;
        }
        self.changed_files
            .iter()
            .all(|path| self.read_files.contains(path) && self.reviewed_files.contains(path))
    }

    pub(crate) fn results(&self) -> &[ToolResultEnvelope] {
        &self.results
    }

    #[cfg(test)]
    pub(crate) fn saw_search(&self) -> bool {
        self.saw_search
    }

    pub(crate) fn observe(&mut self, result: &ToolResultEnvelope) {
        if !result.ok {
            self.observe_failed_read_attempt(result);
            return;
        }
        match result.tool_name.as_builtin() {
            Some(ToolName::ReadDiff) => self.saw_diff = true,
            Some(ToolName::ReadFile | ToolName::ReadFileRange | ToolName::ReadHeadFile) => {
                self.saw_file = true;
                if let Some(path) = result_data_path(result) {
                    self.read_files.insert(path);
                }
            }
            Some(ToolName::SearchText) => self.saw_search = true,
            Some(ToolName::ListChangedFiles) => {
                if !self.fixed_changed_file_scope {
                    self.changed_files.extend(result_changed_files(result));
                }
            }
            Some(ToolName::RecordFileReview) => {
                if let Some(path) = result_data_path(result) {
                    self.reviewed_files.insert(path);
                }
            }
            _ => {}
        }
        if result.artifact_id.is_some()
            && !matches!(
                result.tool_name.as_builtin(),
                Some(ToolName::RecordFinding | ToolName::ChallengeFinding | ToolName::Finish)
            )
        {
            self.results.push(result.clone());
        }
    }

    fn observe_failed_read_attempt(&mut self, result: &ToolResultEnvelope) {
        if !matches!(
            result.tool_name.as_builtin(),
            Some(ToolName::ReadFile | ToolName::ReadFileRange | ToolName::ReadHeadFile)
        ) {
            return;
        }
        if !matches!(
            result.error.as_ref().map(|error| error.code),
            Some(
                ToolErrorCode::NotText
                    | ToolErrorCode::TooLarge
                    | ToolErrorCode::PathDenied
                    | ToolErrorCode::NotFound
            )
        ) {
            return;
        }
        let Some(path) = result_data_path(result) else {
            return;
        };
        if self.fixed_changed_file_scope && !self.changed_files.contains(&path) {
            return;
        }
        self.saw_file = true;
        self.read_files.insert(path);
    }
}

pub(crate) fn assigned_changed_files(scope: &SessionScope) -> BTreeSet<String> {
    scope
        .instructions
        .iter()
        .filter(|instruction| instruction.trusted && instruction.kind == "changed_file_batch")
        .flat_map(|instruction| changed_files_from_batch_instruction(&instruction.text))
        .collect()
}

fn changed_files_from_batch_instruction(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (prefix, path) = trimmed.split_once(". ")?;
            prefix.parse::<usize>().ok()?;
            let path = path.trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

fn result_data_path(result: &ToolResultEnvelope) -> Option<String> {
    result
        .data
        .as_ref()
        .and_then(|data| data.get("path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

fn result_changed_files(result: &ToolResultEnvelope) -> Vec<String> {
    let Some(files) = result
        .data
        .as_ref()
        .and_then(|data| data.get("changedFiles"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(Value::as_str)
        .filter_map(normalize_changed_file_entry)
        .collect()
}

fn normalize_changed_file_entry(entry: &str) -> Option<String> {
    let trimmed = entry.trim();
    let path = [
        "Added ",
        "Modified ",
        "Deleted ",
        "Renamed ",
        "Copied ",
        "TypeChanged ",
    ]
    .into_iter()
    .find_map(|prefix| trimmed.strip_prefix(prefix))
    .unwrap_or(trimmed)
    .trim();
    (!path.is_empty()).then(|| path.to_string())
}
