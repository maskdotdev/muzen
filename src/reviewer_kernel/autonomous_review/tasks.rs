use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeResult};

use super::schemas::candidate_schema;

pub(super) const SEARCH_CODE_TOOL: &str = "search_code";
pub(super) const EXPLORE_CODE_TOOL: &str = "explore_code";
pub(super) const VALIDATE_FINDING_TOOL: &str = "validate_finding";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DelegateTaskKind {
    SearchCode,
    ExploreCode,
    ValidateFinding,
}

impl DelegateTaskKind {
    pub(super) fn tool_name(self) -> &'static str {
        match self {
            Self::SearchCode => SEARCH_CODE_TOOL,
            Self::ExploreCode => EXPLORE_CODE_TOOL,
            Self::ValidateFinding => VALIDATE_FINDING_TOOL,
        }
    }

    pub(super) fn slug(self) -> &'static str {
        match self {
            Self::SearchCode => "search",
            Self::ExploreCode => "explore",
            Self::ValidateFinding => "validate",
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            Self::SearchCode => {
                "Run a bounded deterministic-first repository search for code-review leads. Use for broad discovery; it returns ranked leads and omission counts, not final findings."
            }
            Self::ExploreCode => {
                "Spawn a read-only child review agent to investigate one concrete behavior, hypothesis, caller chain, or evidence question. It returns a structured evidence packet."
            }
            Self::ValidateFinding => {
                "Spawn an adversarial read-only validator for one candidate finding. It tries to refute the claim from raw code and diff evidence."
            }
        }
    }

    pub(super) fn parameters_schema(self) -> Value {
        match self {
            Self::ValidateFinding => json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "objective": {"type": "string"},
                    "prompt": {"type": "string"},
                    "candidate": candidate_schema()
                },
                "required": ["objective", "prompt", "candidate"]
            }),
            _ => json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "objective": {"type": "string"},
                    "prompt": {"type": "string"}
                },
                "required": ["objective", "prompt"]
            }),
        }
    }

    pub(super) fn child_prompt(self) -> &'static str {
        match self {
            Self::SearchCode => {
                "You are Muzen's search_code child. Run broad read-only discovery for the requested review objective. Prefer grep/glob/import/test tools. Return ranked leads and omitted counts. Do not publish findings."
            }
            Self::ExploreCode => {
                "You are Muzen's explore_code child. Investigate the requested behavior chain deeply with read-only tools. Return raw evidence, checked paths, candidate findings if any, and open questions."
            }
            Self::ValidateFinding => {
                "You are Muzen's validate_finding child. Be adversarial. Try to refute the candidate from raw code and diff evidence. Return supported only when the changed-code evidence establishes one concrete negative outcome. Return insufficient for correctness/no-issue observations, speculative claims, or bundled claims that combine unrelated behaviors instead of one failing invariant."
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct DelegateTaskRequest {
    pub(super) objective: String,
    pub(super) prompt: String,
    pub(super) candidate: Option<Value>,
}

pub(super) fn parse_delegate_request(
    kind: DelegateTaskKind,
    args: Value,
) -> RuntimeResult<DelegateTaskRequest> {
    let objective = args
        .get("objective")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RuntimeError::InvalidInput("delegate objective is required".to_string()))?
        .to_string();
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or(&objective)
        .to_string();
    let candidate = args.get("candidate").cloned();
    if kind == DelegateTaskKind::ValidateFinding && candidate.is_none() {
        return Err(RuntimeError::InvalidInput(
            "validate_finding requires candidate".to_string(),
        ));
    }
    Ok(DelegateTaskRequest {
        objective,
        prompt,
        candidate,
    })
}
