use serde_json::{json, Value};

use crate::contracts::ToolName;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum ToolArgShape {
    Empty,
    Path,
    FileRange,
    SearchQuery,
    RecordFileReview,
    RecordFinding,
    ChallengeFinding,
    Finish,
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct BuiltinToolSpec {
    pub(crate) name: ToolName,
    pub(crate) description: &'static str,
    pub(crate) arg_shape: ToolArgShape,
    pub(crate) cacheable: bool,
}

impl BuiltinToolSpec {
    pub(crate) fn parameters(self) -> Value {
        match self.arg_shape {
            ToolArgShape::Empty => object_schema(json!({}), Vec::new()),
            ToolArgShape::Path => object_schema(
                json!({"path": {"type": "string"}}),
                vec!["path".to_string()],
            ),
            ToolArgShape::FileRange => object_schema(
                json!({
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                }),
                vec![
                    "path".to_string(),
                    "start_line".to_string(),
                    "end_line".to_string(),
                ],
            ),
            ToolArgShape::SearchQuery => object_schema(
                json!({"query": {"type": "string"}}),
                vec!["query".to_string()],
            ),
            ToolArgShape::RecordFileReview => object_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Repo-relative assigned changed file path for this file-review verdict."
                    },
                    "verdict": {
                        "type": "string",
                        "enum": ["clean", "issue_found", "skipped"],
                        "description": "Use issue_found only after a record_finding call has already succeeded in this same session for this same path."
                    },
                    "summary": {
                        "type": "string",
                        "description": "Concrete evidence-backed verdict summary. For clean, explain the mechanism that makes the change safe; for issue_found, summarize the recorded issue."
                    },
                    "finding_id": {
                        "type": ["string", "null"],
                        "description": "Required only for verdict=issue_found, and must be the id returned by a prior successful record_finding call in this same session for the same path. Use null for clean or skipped."
                    },
                    "related_paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Related files inspected for context; empty array when none.",
                        "maxItems": 12
                    }
                }),
                vec![
                    "path".to_string(),
                    "verdict".to_string(),
                    "summary".to_string(),
                    "finding_id".to_string(),
                    "related_paths".to_string(),
                ],
            ),
            ToolArgShape::RecordFinding => object_schema(
                json!({
                    "title": {"type": "string"},
                    "claim": {"type": "string"},
                    "path": {"type": "string"},
                    "start_line": {"type": "integer"},
                    "end_line": {"type": "integer"}
                }),
                vec![
                    "title".to_string(),
                    "claim".to_string(),
                    "path".to_string(),
                    "start_line".to_string(),
                    "end_line".to_string(),
                ],
            ),
            ToolArgShape::ChallengeFinding => object_schema(
                json!({
                    "finding_id": {"type": "string"},
                    "rationale": {"type": "string"}
                }),
                vec!["finding_id".to_string(), "rationale".to_string()],
            ),
            ToolArgShape::Finish => object_schema(
                json!({"reason": {"type": "string"}}),
                vec!["reason".to_string()],
            ),
        }
    }
}

pub(crate) fn review_builtin_specs() -> impl Iterator<Item = BuiltinToolSpec> {
    ToolName::review_read_only_tools()
        .iter()
        .copied()
        .map(builtin_tool_spec)
}

fn builtin_tool_spec(name: ToolName) -> BuiltinToolSpec {
    match name {
        ToolName::ListChangedFiles => BuiltinToolSpec {
            name,
            description: "List files in the review change set.",
            arg_shape: ToolArgShape::Empty,
            cacheable: true,
        },
        ToolName::ReadDiff => BuiltinToolSpec {
            name,
            description: "Read the review diff manifest.",
            arg_shape: ToolArgShape::Empty,
            cacheable: true,
        },
        ToolName::ListFiles => BuiltinToolSpec {
            name,
            description: "List text/code files in the materialized repo.",
            arg_shape: ToolArgShape::Empty,
            cacheable: true,
        },
        ToolName::ReadFile => BuiltinToolSpec {
            name,
            description: "Read a text file by repo-relative path.",
            arg_shape: ToolArgShape::Path,
            cacheable: true,
        },
        ToolName::ReadFileRange => BuiltinToolSpec {
            name,
            description: "Read a focused line range from a text file by repo-relative path.",
            arg_shape: ToolArgShape::FileRange,
            cacheable: true,
        },
        ToolName::ReadBaseFile => BuiltinToolSpec {
            name,
            description:
                "Read a base snapshot file by repo-relative path when a base snapshot is available.",
            arg_shape: ToolArgShape::Path,
            cacheable: true,
        },
        ToolName::ReadHeadFile => BuiltinToolSpec {
            name,
            description: "Read a head/review file by repo-relative path.",
            arg_shape: ToolArgShape::Path,
            cacheable: true,
        },
        ToolName::SearchText => BuiltinToolSpec {
            name,
            description: "Search repository text for literal terms separated by |.",
            arg_shape: ToolArgShape::SearchQuery,
            cacheable: true,
        },
        ToolName::FindRelatedFiles => BuiltinToolSpec {
            name,
            description: "Find files likely related to a repo-relative path.",
            arg_shape: ToolArgShape::Path,
            cacheable: true,
        },
        ToolName::FindTestsForFile => BuiltinToolSpec {
            name,
            description: "Find likely tests for a repo-relative path.",
            arg_shape: ToolArgShape::Path,
            cacheable: true,
        },
        ToolName::ListImports => BuiltinToolSpec {
            name,
            description: "List import-like lines from a repo-relative text file.",
            arg_shape: ToolArgShape::Path,
            cacheable: true,
        },
        ToolName::RecordFileReview => BuiltinToolSpec {
            name,
            description: "Record the review verdict for one assigned changed file. For a concrete bug, call record_finding first; only after that succeeds call exactly one record_file_review with verdict=issue_found for the same path and that finding_id. The finding_id must be for a finding whose primary path is exactly the same path as this verdict; never reuse a related-file finding_id for another file. For clean/skipped verdicts set finding_id to null.",
            arg_shape: ToolArgShape::RecordFileReview,
            cacheable: false,
        },
        ToolName::RecordFinding => BuiltinToolSpec {
            name,
            description: "Record one concrete, evidence-backed bug for the assigned changed file before marking that file issue_found.",
            arg_shape: ToolArgShape::RecordFinding,
            cacheable: false,
        },
        ToolName::ChallengeFinding => BuiltinToolSpec {
            name,
            description: "Challenge a recorded finding with a rationale.",
            arg_shape: ToolArgShape::ChallengeFinding,
            cacheable: false,
        },
        ToolName::Finish => BuiltinToolSpec {
            name,
            description: "Finish the review session.",
            arg_shape: ToolArgShape::Finish,
            cacheable: false,
        },
    }
}

fn object_schema(properties: Value, required: Vec<String>) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}
