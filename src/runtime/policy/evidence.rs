use std::collections::BTreeSet;

use serde_json::Value;

use crate::contracts::ToolName;
use crate::runtime::contracts::{SessionScope, ToolErrorCode, ToolResultEnvelope};

use super::ReviewerPolicy;
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
    pub(crate) fixed_changed_file_scope: bool,
    pub(crate) results: Vec<ToolResultEnvelope>,
}

impl SessionEvidence {
    pub(crate) fn for_scope(scope: &SessionScope) -> Self {
        let changed_files = assigned_changed_files(scope);
        Self {
            fixed_changed_file_scope: !changed_files.is_empty(),
            changed_files,
            ..Self::default()
        }
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
            _ => {}
        }
        if result.artifact_id.is_some() {
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
