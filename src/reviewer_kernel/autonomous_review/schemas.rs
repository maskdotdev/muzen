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
                "status": {"type": "string", "enum": ["supported", "review_concern", "refuted", "insufficient", "needs_more_evidence"]},
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
    "Return the final autonomous review result now as strict JSON. Include candidate findings for concrete changed-code bugs and plausible single-invariant changed-code risks that have raw code or diff evidence plus a concrete negative outcome; the validator will decide publishability. Each candidate must describe exactly one failing invariant in claim and exactly one concrete user/system/test failure in negativeOutcome; split unrelated behaviors into separate candidates. When an unsafe Optional.get()/unwrap()/expect() depends on a raw List.class/Map.class, unchecked cast, or erased collection shape on the same changed data path, keep it as one evidence-complete candidate that names both the unchecked collection source and the unsafe unwrap/absence outcome. Do not include negative-evidence disclaimers such as \"I did not find...\" in a candidate claim; put refuted adjacent concerns in notes or completeness instead. Do not bury a directly changed, localized risk in notes only because it needs adversarial confirmation. Correctness/no-issue observations, intended behavior, style-only concerns, and observations without a concrete negative outcome belong in notes or completeness.incompleteReasons, not candidates. Account for every diff risk inventory id in completeness.reviewedRiskEntries or completeness.unreviewedRiskEntries. If a material risk entry remains unreviewed, use verdict=incomplete.".to_string()
}

pub(super) fn child_final_instruction(kind: DelegateTaskKind) -> String {
    if kind == DelegateTaskKind::ValidateFinding {
        return "Return the final validate_finding packet now as strict JSON. Use supported only when raw code/diff evidence establishes the exact candidate claim and its concrete negative changed-code outcome. If evidence supports only a sibling, inverse, broader, narrower, or adjacent issue, return refuted or insufficient for this candidate rather than rewriting it into the neighboring issue. Use review_concern only for diagnostic notes where changed-code evidence establishes an actionable concern but the exact runtime/test failure is not proven; review_concern is not publishable, so prefer insufficient when a targeted follow-up search could prove or refute it. Candidate findings must state the supported issue only; do not include negative-evidence disclaimers such as \"I did not find...\" in a candidate claim. For localized resource language/script candidates, if the candidate points at a changed localized resource line and raw current-line evidence shows a value in the wrong language or script for that locale, support the localized mismatch without requiring a base-file before value; publication separately verifies the line is changed. For Optional.get()/unwrap candidates, treat a changed producer, changed lookup order, changed data source, or changed absence condition as changed behavior even when the consumer unwrap existed before; support it if the changed path can now return empty without a dominating presence check. For Optional.get()/unwrap candidates, inspect the changed producer and one reachable caller/precondition before supporting; if raw List.class/Map.class, unchecked casts, or erased collection-shape reconstruction is on the same changed domain value path, return one candidate that names both the unchecked collection source and the unsafe unwrap/absence outcome, and do not support a narrower optional-only candidate. For persisted identity candidates, support when raw evidence shows a changed reconstruction/creation path fails to copy the authoritative stored id/identity and a later remove, update, lookup, audit, or callback consumes that missing or wrong identity; exact replay through every storage backend is not required when the changed contract itself passes a blank/missing identity into a later id-based operation. For persisted identity candidates, a search hit naming a factory, constructor, accessor, update, remove, lookup, audit, or callback method is not sufficient evidence by itself; when such a hit identifies a relevant method or consumer inside the available repository, read the method/consumer body before finalizing supported, refuted, or insufficient. For documentation-contract candidates, support only when public API documentation, Javadocs, schema text, examples, or generated docs contradict executable behavior or built-in implementations in a way that can mislead callers or implementers; inline TODO/comment cleanup observations belong in notes unless they prove a concrete changed-code failure beyond the comment itself. Use refuted for no-issue observations, style-only concerns, speculative claims without changed-code evidence, and claims contradicted by raw evidence. Use insufficient for missing evidence or bundled multi-behavior claims.".to_string();
    }
    format!(
        "Return the final {} packet now as strict JSON. Use supported only when raw code/diff evidence closes the objective and candidateFindings contains every concrete changed-code issue found for this objective. Do not include negative-evidence disclaimers such as \"I did not find...\" in candidate claims; put refuted adjacent concerns in evidence, openQuestions, or summary. Do not mark status=supported with empty candidateFindings. If the evidence refutes the objective or proves no publishable candidate, use refuted with empty candidateFindings. If evidence is incomplete, use insufficient or needs_more_evidence with empty candidateFindings.",
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
