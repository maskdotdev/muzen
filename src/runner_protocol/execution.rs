use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::context_engine::{ContextEngineMode, SnapshotContextEngine};
use crate::reviewer_kernel::events::{ReviewEventRecord, ReviewEventSink};
use crate::reviewer_kernel::kernel::Run;
use crate::reviewer_kernel::report::{ReviewRunSummary, RunReport};
use crate::reviewer_kernel::runtime_events::EventSink as RuntimeEventSink;

use super::adapters::StreamingRunnerEventSink;
use super::planning::plan_run_start;
use super::stored::RunnerStoredRun;
use super::transport::RunnerCallbackTransport;
use super::types::{
    RunHeartbeatConfigParams, RunHeartbeatParams, RunHeartbeatResult, RunStartParams,
    RunnerFileReview, RunnerFinding, RunnerFindingEvidence, RunnerFindingLocation,
    RunnerReviewQualityDiagnostics, RunnerRunResult, RunnerRunSummary, RunnerSnapshotSummary,
};
use super::wiring::RunnerWiring;
use super::RUNNER_PROTOCOL_VERSION;

pub(crate) struct ExecutedRun {
    pub(crate) result: RunnerRunResult,
    pub(crate) events: Vec<ReviewEventRecord>,
    pub(crate) stored: RunnerStoredRun,
}

pub(crate) fn execute_run_start(
    params: RunStartParams,
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
    cancel: CancellationToken,
) -> Result<ExecutedRun> {
    if let Some(protocol_version) = &params.protocol_version {
        if protocol_version != RUNNER_PROTOCOL_VERSION {
            anyhow::bail!("unsupported protocolVersion {protocol_version}");
        }
    }
    let heartbeat = start_heartbeat(
        params.run_id.as_deref().unwrap_or("muzen-run"),
        params.heartbeat.as_ref(),
        transport.clone(),
        cancel.clone(),
    )?;
    let model = params.model.clone();
    let tools = params.tools.clone();
    let context_engine = params.context_engine.clone();
    let plan = plan_run_start(params, transport.as_ref())?;
    let model = model.ok_or_else(|| {
        anyhow::anyhow!("run requires a model; pass a callback or hosted provider model")
    })?;
    let event_sink = Arc::new(RecordingReviewEventSink::default());
    let streaming_sink = transport.as_ref().map(|transport| {
        Arc::new(StreamingRunnerEventSink::new(transport.clone())) as Arc<dyn RuntimeEventSink>
    });
    let wiring = RunnerWiring::new(&plan.run_id, &tools, transport.clone())?;
    let mut builder = wiring.wire_model(
        Run::builder(plan.spec),
        &plan.run_id,
        &model,
        plan.max_active_sessions,
        transport.clone(),
        #[cfg(test)]
        plan.target_path,
    )?;
    if let Some(config) = context_engine {
        if config.mode != ContextEngineMode::Disabled {
            builder = builder.context_engine(Arc::new(SnapshotContextEngine::new(config)));
        }
    }
    let run = if let Some(streaming_sink) = streaming_sink {
        builder.event_sink(streaming_sink).build()
    } else {
        builder.review_event_sink(event_sink.clone()).build()
    }
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build runner tokio runtime")?;
    let report = runtime.block_on(run.execute_with_cancel(cancel));
    heartbeat.stop();
    let result = runner_result_from_report(&report, plan.metadata);
    let stored = RunnerStoredRun::from_report(&report, result.clone());
    Ok(ExecutedRun {
        result,
        events: event_sink.records(),
        stored,
    })
}

struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
}

impl HeartbeatGuard {
    fn noop() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(true)),
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn start_heartbeat(
    run_id: &str,
    config: Option<&RunHeartbeatConfigParams>,
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
    cancel: CancellationToken,
) -> Result<HeartbeatGuard> {
    let Some(config) = config else {
        return Ok(HeartbeatGuard::noop());
    };
    if !config.callback {
        return Ok(HeartbeatGuard::noop());
    }
    let transport = transport
        .ok_or_else(|| anyhow::anyhow!("heartbeat callback requires interactive stdio"))?;
    let interval = Duration::from_millis(config.interval_ms.unwrap_or(30_000).max(1));
    let lease_seconds = config.lease_seconds;
    let run_id = run_id.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    thread::spawn(move || {
        let started = Instant::now();
        let mut sequence = 0u64;
        while !thread_stop.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            thread::sleep(interval);
            if thread_stop.load(Ordering::SeqCst) || cancel.is_cancelled() {
                break;
            }
            sequence += 1;
            let params = RunHeartbeatParams {
                protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
                run_id: run_id.clone(),
                sequence,
                elapsed_ms: started.elapsed().as_micros().div_ceil(1000) as u64,
                lease_seconds,
            };
            let should_continue = transport
                .request("run.heartbeat", json!(params))
                .ok()
                .and_then(|value| serde_json::from_value::<RunHeartbeatResult>(value).ok())
                .map(|result| result.continue_run)
                .unwrap_or(false);
            if !should_continue {
                cancel.cancel();
                break;
            }
        }
    });
    Ok(HeartbeatGuard { stop })
}

fn runner_result_from_report(
    report: &RunReport,
    metadata: BTreeMap<String, Value>,
) -> RunnerRunResult {
    let mut summary = runner_summary_from_review(&report.summary);
    let snapshots = report
        .snapshot_manifests()
        .into_iter()
        .map(|manifest| RunnerSnapshotSummary {
            snapshot_id: manifest.snapshot_id.0,
            files: manifest.files.len(),
            changed_files: manifest.changed_files.len(),
            captured_files: manifest
                .files
                .iter()
                .filter(|file| {
                    matches!(
                        file.capture_status,
                        crate::reviewer_kernel::kernel_types::SnapshotCaptureStatus::Captured
                    )
                })
                .count(),
            captured_bytes: manifest.captured_text_bytes as u64,
        })
        .collect();
    let findings = dedupe_runner_findings(
        report
            .findings()
            .into_iter()
            .map(|finding| RunnerFinding {
                id: finding.id,
                title: finding.title,
                claim: finding.claim,
                evidence_count: finding.evidence_count,
                publishable: finding.publishable,
                severity: Some(finding.severity),
                confidence: Some(finding.confidence),
                validation_status: Some(finding.validation_status),
                challenge_status: Some(finding.challenge_status),
                evidence: finding
                    .evidence
                    .into_iter()
                    .map(|evidence| RunnerFindingEvidence {
                        evidence_id: evidence.evidence_id,
                        artifact_id: evidence.artifact_id.0,
                        kind: evidence.kind,
                        content_hash: evidence.content_hash,
                        producing_tool_call_id: evidence.producing_tool_call_id.0,
                    })
                    .collect(),
                discovered_by: finding.discovered_by,
                validated_by: finding.validated_by,
                challenged_by: finding.challenged_by,
                location: finding.location.map(|location| RunnerFindingLocation {
                    path: location.path,
                    revision: None,
                    start_line: location.start_line,
                    end_line: location.end_line,
                    start_column: None,
                    end_column: None,
                    side: None,
                    provider_anchor: None,
                }),
                related_paths: finding.related_paths,
            })
            .collect(),
    );
    summary.findings = findings.len();
    summary.publishable_findings = findings
        .iter()
        .filter(|finding| finding.publishable)
        .count();
    let file_reviews = report
        .file_reviews()
        .into_iter()
        .map(|review| RunnerFileReview {
            path: review.path,
            verdict: review.verdict,
            coverage: runner_coverage(review.coverage).to_string(),
            review_verdict: runner_review_verdict(review.review_verdict).to_string(),
            summary: review.summary,
            related_paths: review.related_paths,
            evidence_artifact_ids: review.evidence_artifact_ids,
            evidence_count: review.evidence_count,
            session_id: review.session_id,
            unit_id: review.unit_id,
        })
        .collect();
    RunnerRunResult {
        protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
        run_id: report.run_id.clone(),
        status: summary_status(&summary),
        summary,
        file_reviews,
        findings,
        snapshots,
        metadata,
    }
}

fn dedupe_runner_findings(findings: Vec<RunnerFinding>) -> Vec<RunnerFinding> {
    let mut kept: Vec<RunnerFinding> = Vec::new();
    for finding in findings {
        if let Some(existing) = kept
            .iter_mut()
            .find(|existing| is_duplicate_finding(existing, &finding))
        {
            merge_runner_finding(existing, finding);
            continue;
        }
        kept.push(finding);
    }
    kept
}

/// Independent review passes (units, packs, synthesis) frequently rediscover
/// the same bug with different phrasing, so duplicates are detected by anchor
/// overlap plus content similarity instead of exact text equality.
fn is_duplicate_finding(left: &RunnerFinding, right: &RunnerFinding) -> bool {
    let left_text = normalize_finding_text(&format!("{} {}", left.title, left.claim));
    let right_text = normalize_finding_text(&format!("{} {}", right.title, right.claim));
    if left_text == right_text {
        return true;
    }
    let (Some(left_location), Some(right_location)) = (&left.location, &right.location) else {
        return false;
    };
    if left_location.path != right_location.path {
        return false;
    }
    let overlap = finding_token_overlap(&left_text, &right_text);
    if line_ranges_near(left_location, right_location, 3) {
        return overlap >= 0.4;
    }
    // Models sometimes anchor the same bug to a different spot in the file
    // (e.g. the enclosing function signature), so a strong content match
    // still counts as a duplicate within one file.
    overlap >= 0.55
}

fn line_ranges_near(
    left: &RunnerFindingLocation,
    right: &RunnerFindingLocation,
    slack: usize,
) -> bool {
    let (Some(left_start), Some(right_start)) = (left.start_line, right.start_line) else {
        return false;
    };
    let left_end = left.end_line.unwrap_or(left_start);
    let right_end = right.end_line.unwrap_or(right_start);
    left_start <= right_end.saturating_add(slack) && right_start <= left_end.saturating_add(slack)
}

fn finding_token_overlap(left: &str, right: &str) -> f64 {
    let left_tokens = left.split_whitespace().collect::<BTreeSet<_>>();
    let right_tokens = right.split_whitespace().collect::<BTreeSet<_>>();
    let smaller = left_tokens.len().min(right_tokens.len());
    if smaller == 0 {
        return 0.0;
    }
    let shared = left_tokens.intersection(&right_tokens).count();
    shared as f64 / smaller as f64
}

fn normalize_finding_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn merge_runner_finding(existing: &mut RunnerFinding, duplicate: RunnerFinding) {
    existing.confidence = match (existing.confidence, duplicate.confidence) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (None, right) => right,
        (left, None) => left,
    };
    existing.publishable |= duplicate.publishable;
    append_unique(&mut existing.discovered_by, duplicate.discovered_by);
    append_unique(&mut existing.validated_by, duplicate.validated_by);
    append_unique(&mut existing.challenged_by, duplicate.challenged_by);

    let mut seen_evidence = existing
        .evidence
        .iter()
        .map(|evidence| evidence.evidence_id.clone())
        .collect::<BTreeSet<_>>();
    for evidence in duplicate.evidence {
        if seen_evidence.insert(evidence.evidence_id.clone()) {
            existing.evidence.push(evidence);
        }
    }
    existing.evidence_count = existing.evidence.len();

    if let Some(path) = duplicate
        .location
        .as_ref()
        .map(|location| location.path.clone())
    {
        append_related_location(&mut existing.claim, &path);
    }
}

fn append_unique(values: &mut Vec<String>, additions: Vec<String>) {
    let mut seen = values.iter().cloned().collect::<BTreeSet<_>>();
    for value in additions {
        if seen.insert(value.clone()) {
            values.push(value);
        }
    }
}

fn append_related_location(claim: &mut String, path: &str) {
    if claim.contains(path) {
        return;
    }
    if claim.contains("Also observed in:") {
        claim.push_str(", ");
        claim.push_str(path);
    } else {
        claim.push_str(" Also observed in: ");
        claim.push_str(path);
    }
}

fn runner_coverage(
    coverage: crate::reviewer_kernel::review_contract::ReviewCoverage,
) -> &'static str {
    match coverage {
        crate::reviewer_kernel::review_contract::ReviewCoverage::Full => "full",
        crate::reviewer_kernel::review_contract::ReviewCoverage::Standard => "standard",
        crate::reviewer_kernel::review_contract::ReviewCoverage::Sampled => "sampled",
        crate::reviewer_kernel::review_contract::ReviewCoverage::Insufficient => "insufficient",
    }
}

fn runner_review_verdict(
    verdict: crate::reviewer_kernel::review_contract::ReviewVerdict,
) -> &'static str {
    match verdict {
        crate::reviewer_kernel::review_contract::ReviewVerdict::Clean => "clean",
        crate::reviewer_kernel::review_contract::ReviewVerdict::IssueFound => "issue_found",
        crate::reviewer_kernel::review_contract::ReviewVerdict::NeedsReview => "needs_review",
    }
}

fn runner_summary_from_review(summary: &ReviewRunSummary) -> RunnerRunSummary {
    RunnerRunSummary {
        sessions: summary.sessions,
        completed_sessions: summary.completed_sessions,
        model_calls: summary.model_calls,
        tool_calls: summary.tool_calls,
        findings: summary.findings,
        publishable_findings: summary.publishable_findings,
        elapsed_ms: summary.elapsed_ms,
        input_tokens: summary.input_tokens,
        output_tokens: summary.output_tokens,
        total_tokens: summary.total_tokens,
        cached_input_tokens: summary.cached_input_tokens,
        artifacts: summary.artifacts,
        artifact_bytes: summary.artifact_bytes,
        snapshot_count: summary.snapshot_count,
        quality_diagnostics: RunnerReviewQualityDiagnostics {
            contract_risk_units: summary.quality_diagnostics.contract_risk_units,
            contract_seed_count: summary.quality_diagnostics.contract_seed_count,
            contract_pack_count: summary.quality_diagnostics.contract_pack_count,
            omitted_contract_pack_candidates: summary
                .quality_diagnostics
                .omitted_contract_pack_candidates
                .clone(),
            selected_contract_packs: summary.quality_diagnostics.selected_contract_packs.clone(),
            contract_evidence_failures: summary.quality_diagnostics.contract_evidence_failures,
            coverage_counts: summary.quality_diagnostics.coverage_counts.clone(),
            coverage_counts_by_lens: summary.quality_diagnostics.coverage_counts_by_lens.clone(),
            high_risk_files_below_target: summary
                .quality_diagnostics
                .high_risk_files_below_target
                .clone(),
            challenge_status_counts: summary.quality_diagnostics.challenge_status_counts.clone(),
            sessions_run: summary.quality_diagnostics.sessions_run,
            budgets_used: summary.quality_diagnostics.budgets_used.clone(),
            explicit_caller_cap_sessions: summary.quality_diagnostics.explicit_caller_cap_sessions,
            candidate_findings: summary.quality_diagnostics.candidate_findings,
            rescued_candidates: summary.quality_diagnostics.rescued_candidates,
            rejected_candidates: summary.quality_diagnostics.rejected_candidates,
            rejection_reasons: summary.quality_diagnostics.rejection_reasons.clone(),
        },
    }
}

fn summary_status(summary: &RunnerRunSummary) -> String {
    if summary.completed_sessions == summary.sessions {
        "completed".to_string()
    } else {
        "partial".to_string()
    }
}

#[derive(Default)]
struct RecordingReviewEventSink {
    records: std::sync::Mutex<Vec<ReviewEventRecord>>,
}

impl RecordingReviewEventSink {
    fn records(&self) -> Vec<ReviewEventRecord> {
        self.records
            .lock()
            .expect("review event sink poisoned")
            .clone()
    }
}

impl ReviewEventSink for RecordingReviewEventSink {
    fn emit_review_event(&self, record: ReviewEventRecord) {
        self.records
            .lock()
            .expect("review event sink poisoned")
            .push(record);
    }
}

#[cfg(test)]
mod tests;
