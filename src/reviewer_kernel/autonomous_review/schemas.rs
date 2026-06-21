use serde_json::{json, Value};

use crate::reviewer_kernel::kernel_types::ModelResponseFormat;

use super::sessions::{session_kind_name, SessionKind};
use super::tasks::DelegateTaskKind;

pub(super) fn orchestrator_response_format() -> ModelResponseFormat {
    ModelResponseFormat::json_schema(
        "muzen_autonomous_review_result_v1",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["verdict", "summary", "candidates", "notes", "completeness"],
            "properties": {
                "verdict": {"type": "string", "enum": ["issues_found", "clean", "incomplete"]},
                "summary": {"type": "string"},
                "candidates": {"type": "array", "items": candidate_schema()},
                "notes": {"type": "array", "items": {"type": "string"}},
                "completeness": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "reviewedChangedFiles",
                        "reviewedRiskEntries",
                        "unreviewedRiskEntries",
                        "unresolvedQuestions",
                        "incompleteReasons",
                        "ignoredChildCandidates"
                    ],
                    "properties": {
                        "reviewedChangedFiles": {"type": "array", "items": {"type": "string"}},
                        "reviewedRiskEntries": {"type": "array", "items": {"type": "string"}},
                        "unreviewedRiskEntries": {"type": "array", "items": {"type": "string"}},
                        "unresolvedQuestions": {"type": "array", "items": {"type": "string"}},
                        "incompleteReasons": {"type": "array", "items": {"type": "string"}},
                        "ignoredChildCandidates": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }
        }),
    )
}

pub(super) fn child_response_format(kind: DelegateTaskKind) -> ModelResponseFormat {
    ModelResponseFormat::json_schema(
        format!("muzen_{}_packet_v1", kind.tool_name()),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "status",
                "summary",
                "checkedPaths",
                "evidence",
                "openQuestions",
                "suggestedNextSearches",
                "candidateFindings"
            ],
            "properties": {
                "status": {"type": "string", "enum": ["supported", "refuted", "insufficient", "needs_more_evidence"]},
                "summary": {"type": "string"},
                "checkedPaths": {"type": "array", "items": {"type": "string"}},
                "evidence": {"type": "array", "items": evidence_packet_schema()},
                "openQuestions": {"type": "array", "items": {"type": "string"}},
                "suggestedNextSearches": {"type": "array", "items": {"type": "string"}},
                "candidateFindings": {"type": "array", "items": candidate_schema()}
            }
        }),
    )
}

pub(super) fn candidate_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "id",
            "title",
            "claim",
            "negativeOutcome",
            "severity",
            "path",
            "startLine",
            "endLine",
            "behaviorBefore",
            "behaviorAfter",
            "evidenceArtifactIds",
            "relatedPaths"
        ],
        "properties": {
            "id": {"type": "string"},
            "title": {"type": "string"},
            "claim": {"type": "string"},
            "negativeOutcome": {"type": "string"},
            "severity": {"type": ["string", "null"]},
            "path": {"type": "string"},
            "startLine": {"type": ["integer", "null"]},
            "endLine": {"type": ["integer", "null"]},
            "behaviorBefore": {"type": ["string", "null"]},
            "behaviorAfter": {"type": ["string", "null"]},
            "evidenceArtifactIds": {"type": "array", "items": {"type": "string"}},
            "relatedPaths": {"type": "array", "items": {"type": "string"}}
        }
    })
}

fn evidence_packet_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "path",
            "startLine",
            "endLine",
            "snippet",
            "artifactId",
            "whyItMatters"
        ],
        "properties": {
            "path": {"type": ["string", "null"]},
            "startLine": {"type": ["integer", "null"]},
            "endLine": {"type": ["integer", "null"]},
            "snippet": {"type": ["string", "null"]},
            "artifactId": {"type": ["string", "null"]},
            "whyItMatters": {"type": ["string", "null"]}
        }
    })
}

pub(super) fn orchestrator_final_instruction() -> String {
    "Return the final autonomous review result now as strict JSON. Include candidate findings for concrete changed-code bugs and plausible single-invariant changed-code risks that have raw code or diff evidence plus a concrete negative outcome; the validator will decide publishability. Each candidate must describe exactly one failing invariant in claim and exactly one concrete user/system/test failure in negativeOutcome; split unrelated behaviors into separate candidates. Do not bury a directly changed, localized risk in notes only because it needs adversarial confirmation. Correctness/no-issue observations, intended behavior, style-only concerns, and observations without a concrete negative outcome belong in notes or completeness.incompleteReasons, not candidates. Account for every diff risk inventory id in completeness.reviewedRiskEntries or completeness.unreviewedRiskEntries. If a material risk entry remains unreviewed, use verdict=incomplete.".to_string()
}

pub(super) fn child_final_instruction(kind: DelegateTaskKind) -> String {
    if kind == DelegateTaskKind::ValidateFinding {
        return "Return the final validate_finding packet now as strict JSON. Use supported only when raw code/diff evidence establishes one concrete negative changed-code outcome. Use insufficient for no-issue observations, speculative claims, and bundled multi-behavior claims.".to_string();
    }
    format!(
        "Return the final {} packet now as strict JSON. Use supported only when raw code/diff evidence closes the objective.",
        kind.tool_name()
    )
}

pub(super) fn schema_repair_instruction(
    kind: SessionKind,
    attempt: usize,
    max_attempts: usize,
) -> String {
    format!(
        "Your previous final answer did not match the required {} JSON schema. Return corrected strict JSON only. Repair attempt {attempt}/{max_attempts}.",
        session_kind_name(kind)
    )
}

pub(super) fn session_output_valid(kind: SessionKind, output: Option<&str>) -> bool {
    let Some(output) = output else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    match kind {
        SessionKind::Orchestrator => {
            value.get("verdict").and_then(Value::as_str).is_some()
                && value.get("summary").and_then(Value::as_str).is_some()
                && value
                    .get("candidates")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.iter().all(candidate_packet_valid))
                && value.get("notes").and_then(Value::as_array).is_some()
                && value
                    .get("completeness")
                    .and_then(Value::as_object)
                    .is_some_and(completeness_packet_valid)
        }
        SessionKind::Child(_) => {
            value.get("status").and_then(Value::as_str).is_some()
                && value.get("summary").and_then(Value::as_str).is_some()
                && value
                    .get("checkedPaths")
                    .and_then(Value::as_array)
                    .is_some()
                && value.get("evidence").and_then(Value::as_array).is_some()
                && value
                    .get("openQuestions")
                    .and_then(Value::as_array)
                    .is_some()
                && value
                    .get("candidateFindings")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.iter().all(candidate_packet_valid))
        }
    }
}

fn completeness_packet_valid(value: &serde_json::Map<String, Value>) -> bool {
    [
        "reviewedChangedFiles",
        "reviewedRiskEntries",
        "unreviewedRiskEntries",
        "unresolvedQuestions",
        "incompleteReasons",
        "ignoredChildCandidates",
    ]
    .iter()
    .all(|key| value.get(*key).and_then(Value::as_array).is_some())
}

fn candidate_packet_valid(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("id").and_then(Value::as_str).is_some()
        && object.get("title").and_then(Value::as_str).is_some()
        && object.get("claim").and_then(Value::as_str).is_some()
        && object
            .get("negativeOutcome")
            .and_then(Value::as_str)
            .is_some()
        && object.contains_key("severity")
        && object.get("path").and_then(Value::as_str).is_some()
        && object.contains_key("startLine")
        && object.contains_key("endLine")
        && object.contains_key("behaviorBefore")
        && object.contains_key("behaviorAfter")
        && object
            .get("evidenceArtifactIds")
            .and_then(Value::as_array)
            .is_some()
        && object
            .get("relatedPaths")
            .and_then(Value::as_array)
            .is_some()
}
