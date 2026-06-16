use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::reviewer_kernel::kernel_types::{stable_id, ArtifactId, SessionId};
use crate::reviewer_kernel::review_contract::{
    ArtifactKind, ChallengeStatus, EvidenceLocationV1, EvidenceRefV1, EvidenceRevision,
    FileReviewV1, FindingPublishability, FindingSeverity, FindingV1, LineRangeV1,
    RedactionMetadataV1, RedactionState, ReportStatus, ReviewCoverage, ReviewVerdict,
    ValidationStatus,
};
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::workspace::RepoSnapshot;

use super::diff_risk::truncate_chars;
use super::finding_filters::{autonomous_candidate_rejection_reason, changed_line_ranges_by_path};
use super::tasks::DelegateTaskKind;
use super::ORCHESTRATOR_SESSION_ID;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CandidateFinding {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) claim: String,
    #[serde(default)]
    pub(super) severity: Option<String>,
    pub(super) path: String,
    #[serde(default)]
    pub(super) start_line: Option<usize>,
    #[serde(default)]
    pub(super) end_line: Option<usize>,
    #[serde(default)]
    pub(super) behavior_before: Option<String>,
    #[serde(default)]
    pub(super) behavior_after: Option<String>,
    #[serde(default)]
    pub(super) evidence_artifact_ids: Vec<String>,
    #[serde(default)]
    pub(super) related_paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ParsedOrchestratorOutput {
    pub(super) verdict: String,
    pub(super) summary: String,
    pub(super) candidates: Vec<CandidateFinding>,
    pub(super) notes: Vec<String>,
    pub(super) completeness: Value,
}

pub(super) fn parse_orchestrator_output(output: Option<&str>) -> ParsedOrchestratorOutput {
    let value = output
        .and_then(|output| serde_json::from_str::<Value>(output).ok())
        .unwrap_or_else(|| json!({}));
    let candidates = value
        .get("candidates")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<CandidateFinding>(item.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let notes = value
        .get("notes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ParsedOrchestratorOutput {
        verdict: value
            .get("verdict")
            .and_then(Value::as_str)
            .unwrap_or(if candidates.is_empty() {
                "incomplete"
            } else {
                "issues_found"
            })
            .to_string(),
        summary: value
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("autonomous review completed")
            .to_string(),
        candidates,
        notes,
        completeness: value
            .get("completeness")
            .cloned()
            .unwrap_or_else(|| json!({})),
    }
}

pub(super) fn parse_child_packet(kind: DelegateTaskKind, output: Option<&str>) -> Value {
    output
        .and_then(|output| serde_json::from_str::<Value>(output).ok())
        .unwrap_or_else(|| {
            json!({
                "status": "insufficient",
                "summary": format!("{} child did not return a valid packet", kind.tool_name()),
                "checkedPaths": [],
                "evidence": [],
                "openQuestions": ["child output was missing or malformed"],
                "suggestedNextSearches": [],
                "candidateFindings": []
            })
        })
}

pub(super) fn compact_child_packet(
    kind: DelegateTaskKind,
    session_id: &SessionId,
    packet: &Value,
    artifact_id: &ArtifactId,
) -> Value {
    json!({
        "taskType": kind.tool_name(),
        "sessionId": session_id.0,
        "status": packet.get("status").cloned().unwrap_or_else(|| json!("insufficient")),
        "summary": packet.get("summary").cloned().unwrap_or_else(|| json!("")),
        "checkedPaths": compact_string_array(packet.get("checkedPaths"), 40),
        "candidateCount": packet.get("candidateFindings").and_then(Value::as_array).map_or(0, Vec::len),
        "openQuestionCount": packet.get("openQuestions").and_then(Value::as_array).map_or(0, Vec::len),
        "artifactId": artifact_id.0,
    })
}

fn compact_string_array(value: Option<&Value>, max_items: usize) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .take(max_items)
                .map(|item| truncate_chars(item, 240))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub(super) struct ValidationPacket {
    pub(super) candidate_id: String,
    pub(super) status: String,
    pub(super) summary: String,
    pub(super) artifact_id: Option<ArtifactId>,
    pub(super) child_session_id: Option<String>,
}

pub(super) struct FindingBuildOutcome {
    pub(super) findings: Vec<FindingV1>,
    pub(super) rejection_reasons: BTreeMap<String, usize>,
}

pub(super) fn build_findings(
    tools: &ToolEngine,
    snapshot: &RepoSnapshot,
    review_revision_id: &str,
    candidates: &[CandidateFinding],
    validations: &[ValidationPacket],
) -> FindingBuildOutcome {
    let validation_by_candidate = validations
        .iter()
        .map(|validation| (validation.candidate_id.as_str(), validation))
        .collect::<HashMap<_, _>>();
    let changed_paths = changed_paths_for_snapshot(snapshot);
    let changed_ranges = changed_line_ranges_by_path(&snapshot.diff.content);
    let mut seen_candidate_keys = BTreeSet::new();
    let mut findings = Vec::new();
    let mut rejection_reasons = BTreeMap::new();
    for candidate in candidates {
        let key = stable_id(&[&candidate.path, &candidate.claim, &candidate.title]);
        if !seen_candidate_keys.insert(key) {
            record_candidate_rejection(&mut rejection_reasons, "duplicate_candidate");
            continue;
        }
        let Some(validation) = validation_by_candidate.get(candidate.id.as_str()) else {
            record_candidate_rejection(&mut rejection_reasons, "missing_validation");
            continue;
        };
        if !validation.status.trim().eq_ignore_ascii_case("supported") {
            record_candidate_rejection(
                &mut rejection_reasons,
                validation_rejection_reason(&validation.status),
            );
            continue;
        }
        if let Some(reason) =
            autonomous_candidate_rejection_reason(candidate, &changed_paths, &changed_ranges)
        {
            record_candidate_rejection(&mut rejection_reasons, reason);
            continue;
        }
        let validation_has_summary = !validation.summary.trim().is_empty();
        findings.push(candidate_to_finding(
            tools,
            review_revision_id,
            candidate,
            validation,
            validation_has_summary,
        ));
    }
    FindingBuildOutcome {
        findings,
        rejection_reasons,
    }
}

fn record_candidate_rejection(
    rejection_reasons: &mut BTreeMap<String, usize>,
    reason: impl Into<String>,
) {
    *rejection_reasons.entry(reason.into()).or_insert(0) += 1;
}

fn validation_rejection_reason(status: &str) -> String {
    let normalized = status.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "refuted" => "validator_refuted".to_string(),
        "insufficient" => "validator_insufficient".to_string(),
        "needs_more_evidence" => "validator_needs_more_evidence".to_string(),
        "" => "validator_missing_status".to_string(),
        _ => format!("validator_{normalized}"),
    }
}

fn changed_paths_for_snapshot(snapshot: &RepoSnapshot) -> BTreeSet<String> {
    snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| file.rel_path.display())
        .collect()
}

fn candidate_to_finding(
    tools: &ToolEngine,
    review_revision_id: &str,
    candidate: &CandidateFinding,
    validation: &ValidationPacket,
    validation_has_summary: bool,
) -> FindingV1 {
    let artifact_ids = candidate
        .evidence_artifact_ids
        .iter()
        .filter_map(|id| {
            let artifact_id = ArtifactId(id.clone());
            tools
                .artifact(&artifact_id)
                .map(|artifact| (artifact_id, artifact))
        })
        .collect::<Vec<_>>();
    let fallback_artifact = validation.artifact_id.as_ref().and_then(|artifact_id| {
        tools
            .artifact(artifact_id)
            .map(|artifact| (artifact_id.clone(), artifact))
    });
    let evidence = if artifact_ids.is_empty() {
        fallback_artifact.into_iter().collect::<Vec<_>>()
    } else {
        artifact_ids
    }
    .into_iter()
    .enumerate()
    .map(|(index, (artifact_id, artifact))| EvidenceRefV1 {
        evidence_id: format!("ev_{}", stable_id(&[&candidate.id, &index.to_string()])),
        artifact_id: artifact_id.0.clone(),
        kind: ArtifactKind::ToolSummary,
        revision: EvidenceRevision::Review,
        revision_id: review_revision_id.to_string(),
        location: EvidenceLocationV1::SinglePath {
            path: candidate.path.clone(),
        },
        line_range: candidate
            .start_line
            .zip(candidate.end_line)
            .map(|(start_line, end_line)| LineRangeV1 {
                start_line,
                end_line,
            }),
        content_hash: artifact.content_hash,
        redaction: RedactionMetadataV1 {
            redaction_state: RedactionState::Partial,
            redaction_policy_id: "runtime-redactor-v1".to_string(),
            contains_repo_content: true,
            contains_prompt_content: false,
            contains_model_output: true,
            contains_secret_material: false,
        },
        producing_tool_call_id: validation
            .child_session_id
            .clone()
            .unwrap_or_else(|| "mandatory-validation".to_string()),
    })
    .collect::<Vec<_>>();
    FindingV1 {
        id: if candidate.id.trim().is_empty() {
            format!(
                "finding_{}",
                stable_id(&[&candidate.path, &candidate.claim])
            )
        } else {
            candidate.id.clone()
        },
        title: candidate.title.clone(),
        claim: candidate.claim.clone(),
        severity: parse_severity(candidate.severity.as_deref()),
        confidence: if validation_has_summary { 0.85 } else { 0.8 },
        validation_status: ValidationStatus::Validated,
        report_status: ReportStatus::Included,
        publishability: FindingPublishability::Publishable,
        challenge_status: ChallengeStatus::Confirmed,
        evidence,
        file_refs: std::iter::once(candidate.path.clone())
            .chain(candidate.related_paths.iter().cloned())
            .map(|path| EvidenceLocationV1::SinglePath { path })
            .collect(),
        location_line_range: candidate.start_line.zip(candidate.end_line).map(
            |(start_line, end_line)| LineRangeV1 {
                start_line,
                end_line,
            },
        ),
        discovered_by: vec![ORCHESTRATOR_SESSION_ID.to_string()],
        challenged_by: validation
            .child_session_id
            .clone()
            .map(|id| vec![id])
            .unwrap_or_default(),
    }
}

fn parse_severity(value: Option<&str>) -> FindingSeverity {
    match value
        .unwrap_or("medium")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "blocker" => FindingSeverity::Blocker,
        "high" => FindingSeverity::High,
        "low" => FindingSeverity::Low,
        "nit" => FindingSeverity::Nit,
        _ => FindingSeverity::Medium,
    }
}

pub(super) fn build_file_reviews(
    snapshot: &RepoSnapshot,
    parsed: &ParsedOrchestratorOutput,
    findings: &[FindingV1],
    output: &str,
) -> Vec<FileReviewV1> {
    let completeness_incomplete = parsed
        .completeness
        .get("incompleteReasons")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    let unreviewed_risks = parsed
        .completeness
        .get("unreviewedRiskEntries")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    let unpublished_candidates = parsed.candidates.len() > findings.len();
    let incomplete = parsed.verdict == "incomplete"
        || completeness_incomplete
        || unreviewed_risks
        || unpublished_candidates;
    let finding_paths = findings
        .iter()
        .flat_map(|finding| finding.file_refs.iter())
        .filter_map(|location| match location {
            EvidenceLocationV1::SinglePath { path } => Some(path.clone()),
        })
        .collect::<std::collections::BTreeSet<_>>();
    snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| {
            let path = file.rel_path.display();
            let issue_found = finding_paths.contains(&path);
            let review_verdict = if issue_found {
                ReviewVerdict::IssueFound
            } else if incomplete {
                ReviewVerdict::NeedsReview
            } else {
                ReviewVerdict::Clean
            };
            FileReviewV1 {
                path: path.clone(),
                verdict: match review_verdict {
                    ReviewVerdict::IssueFound => "issue_found",
                    ReviewVerdict::NeedsReview => "needs_review",
                    ReviewVerdict::Clean => "clean",
                }
                .to_string(),
                coverage: if incomplete {
                    ReviewCoverage::Insufficient
                } else {
                    ReviewCoverage::Standard
                },
                review_verdict,
                summary: truncate_chars(
                    if parsed.summary.is_empty() {
                        output
                    } else {
                        &parsed.summary
                    },
                    500,
                ),
                related_paths: Vec::new(),
                evidence_artifact_ids: Vec::new(),
                evidence_count: 0,
                session_id: ORCHESTRATOR_SESSION_ID.to_string(),
                unit_id: "autonomous-review".to_string(),
            }
        })
        .collect()
}
