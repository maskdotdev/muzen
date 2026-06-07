use serde_json::{json, Value};

use crate::contracts::ToolName;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum ToolArgShape {
    Empty,
    Path,
    FileRange,
    SearchQuery,
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
