use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(test)]
use crate::reviewer_kernel::kernel_types::SnapshotId;
use crate::reviewer_kernel::kernel_types::{
    ArtifactId, ConcurrentCounters, ConcurrentRunReport, ModelMetricsSnapshot,
    ReviewQualityDiagnostics, RuntimeEvent, RuntimeEventContext, RuntimeEventSink,
    SessionCompletionDiagnostic, ToolCallId, ToolMetricKey, ToolMetricsSnapshot,
    ToolProviderHealthSnapshot, ToolProviderHealthState,
};
use crate::reviewer_kernel::review_contract::{FileReviewV1, FindingV1};

use crate::reviewer_kernel::events::*;
use crate::reviewer_kernel::snapshots::*;
use crate::reviewer_kernel::tool_engine::ConcurrentArtifactStore as ArtifactStore;
pub(crate) struct ReviewEventSinkAdapter {
    inner: Arc<dyn ReviewEventSink>,
    next_seq: AtomicU64,
}

impl ReviewEventSinkAdapter {
    pub(crate) fn new(inner: Arc<dyn ReviewEventSink>) -> Self {
        Self {
            inner,
            next_seq: AtomicU64::new(1),
        }
    }
}

impl RuntimeEventSink for ReviewEventSinkAdapter {
    fn emit(&self, event: RuntimeEvent) {
        self.emit_with_context(RuntimeEventContext::from_event(&event), event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        self.inner
            .emit_review_event(ReviewEventRecord::from_runtime(seq, context, &event));
    }
}

pub(crate) fn merge_run_summaries(mut summaries: Vec<ConcurrentRunReport>) -> ConcurrentRunReport {
    let mut merged = summaries.remove(0);
    for summary in summaries {
        merged.sessions += summary.sessions;
        merged.completed_sessions += summary.completed_sessions;
        merged.model_calls += summary.model_calls;
        merged.tool_calls += summary.tool_calls;
        merged.tool_counts.add(summary.tool_counts);
        merged.findings += summary.findings;
        merged.publishable_findings += summary.publishable_findings;
        merged.elapsed_ms += summary.elapsed_ms;
        merged.input_tokens += summary.input_tokens;
        merged.cached_input_tokens += summary.cached_input_tokens;
        merged.output_tokens += summary.output_tokens;
        merged.total_tokens += summary.total_tokens;
        merged.artifacts += summary.artifacts;
        merged.artifact_bytes += summary.artifact_bytes;
        merge_counters(&mut merged.counters, summary.counters);
        merge_tool_metrics(&mut merged.tool_metrics, summary.tool_metrics);
        merged.provider_health =
            merge_provider_health(merged.provider_health, summary.provider_health);
        merged.snapshot_metrics.extend(summary.snapshot_metrics);
        merge_model_metrics(&mut merged.model_metrics, summary.model_metrics);
        merged.quality_diagnostics.add(summary.quality_diagnostics);
        merged
            .completion_diagnostics
            .extend(summary.completion_diagnostics);
    }
    merged
        .completion_diagnostics
        .sort_by(|left, right| left.session_id.cmp(&right.session_id));
    merged.snapshot_metrics.sort_by(|left, right| {
        left.snapshot_id
            .0
            .cmp(&right.snapshot_id.0)
            .then(left.sessions.cmp(&right.sessions))
    });
    merged
}

fn merge_counters(left: &mut ConcurrentCounters, right: ConcurrentCounters) {
    left.search_scans += right.search_scans;
    left.search_dedupe_waiters += right.search_dedupe_waiters;
    left.search_cache_hits += right.search_cache_hits;
    left.read_cache_hits += right.read_cache_hits;
    left.read_file_reads += right.read_file_reads;
    left.tool_errors += right.tool_errors;
    left.artifact_cache_hits += right.artifact_cache_hits;
}

fn merge_tool_metrics(
    left: &mut BTreeMap<ToolMetricKey, ToolMetricsSnapshot>,
    right: BTreeMap<ToolMetricKey, ToolMetricsSnapshot>,
) {
    for (key, metrics) in right {
        let entry = left.entry(key).or_default();
        entry.calls += metrics.calls;
        entry.successes += metrics.successes;
        entry.errors += metrics.errors;
        entry.cache_hits += metrics.cache_hits;
        entry.deduped += metrics.deduped;
        entry.timeouts += metrics.timeouts;
        entry.cancellations += metrics.cancellations;
        entry.artifacts += metrics.artifacts;
        entry.input_bytes += metrics.input_bytes;
        entry.output_bytes += metrics.output_bytes;
        entry.latency_ms += metrics.latency_ms;
        entry.max_latency_ms = entry.max_latency_ms.max(metrics.max_latency_ms);
        entry.queue_wait_ms += metrics.queue_wait_ms;
        entry.max_queue_wait_ms = entry.max_queue_wait_ms.max(metrics.max_queue_wait_ms);
    }
}

fn merge_model_metrics(left: &mut ModelMetricsSnapshot, right: ModelMetricsSnapshot) {
    left.calls += right.calls;
    left.successes += right.successes;
    left.errors += right.errors;
    left.retries += right.retries;
    left.costed_calls += right.costed_calls;
    left.unpriced_calls += right.unpriced_calls;
    left.latency_ms += right.latency_ms;
    left.max_latency_ms = left.max_latency_ms.max(right.max_latency_ms);
    left.limiter_wait_ms += right.limiter_wait_ms;
    left.max_limiter_wait_ms = left.max_limiter_wait_ms.max(right.max_limiter_wait_ms);
    left.estimated_input_cost_micro_usd += right.estimated_input_cost_micro_usd;
    left.estimated_output_cost_micro_usd += right.estimated_output_cost_micro_usd;
    left.estimated_total_cost_micro_usd += right.estimated_total_cost_micro_usd;
    left.input_tokens += right.input_tokens;
    left.cached_input_tokens += right.cached_input_tokens;
    left.output_tokens += right.output_tokens;
    left.total_tokens += right.total_tokens;
}

fn merge_provider_health(
    left: Vec<ToolProviderHealthSnapshot>,
    right: Vec<ToolProviderHealthSnapshot>,
) -> Vec<ToolProviderHealthSnapshot> {
    let mut by_provider = BTreeMap::new();
    for snapshot in left.into_iter().chain(right) {
        let entry =
            by_provider
                .entry(snapshot.provider_id.clone())
                .or_insert(ToolProviderHealthSnapshot {
                    provider_id: snapshot.provider_id.clone(),
                    state: ToolProviderHealthState::Healthy,
                    calls: 0,
                    errors: 0,
                    timeouts: 0,
                    cancellations: 0,
                    consecutive_errors: 0,
                });
        entry.calls += snapshot.calls;
        entry.errors += snapshot.errors;
        entry.timeouts += snapshot.timeouts;
        entry.cancellations += snapshot.cancellations;
        entry.consecutive_errors = entry.consecutive_errors.max(snapshot.consecutive_errors);
        entry.state = if entry.consecutive_errors >= 3 {
            ToolProviderHealthState::Unhealthy
        } else if entry.errors > 0 {
            ToolProviderHealthState::Degraded
        } else {
            ToolProviderHealthState::Healthy
        };
    }
    by_provider.into_values().collect()
}

#[derive(Debug, Clone)]
pub struct ReviewRunSummary {
    pub sessions: usize,
    pub completed_sessions: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub findings: usize,
    pub publishable_findings: usize,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub artifacts: usize,
    pub artifact_bytes: usize,
    pub snapshot_count: usize,
    pub completion_diagnostics: Vec<SessionCompletionDiagnostic>,
    pub quality_diagnostics: ReviewQualityDiagnostics,
}

impl ReviewRunSummary {
    pub(crate) fn from_metrics(metrics: &ConcurrentRunReport) -> Self {
        Self {
            sessions: metrics.sessions,
            completed_sessions: metrics.completed_sessions,
            model_calls: metrics.model_calls,
            tool_calls: metrics.tool_calls,
            findings: metrics.findings,
            publishable_findings: metrics.publishable_findings,
            elapsed_ms: metrics.elapsed_ms,
            input_tokens: metrics.input_tokens,
            cached_input_tokens: metrics.cached_input_tokens,
            output_tokens: metrics.output_tokens,
            total_tokens: metrics.total_tokens,
            artifacts: metrics.artifacts,
            artifact_bytes: metrics.artifact_bytes,
            snapshot_count: metrics.snapshot_metrics.len(),
            completion_diagnostics: metrics.completion_diagnostics.clone(),
            quality_diagnostics: metrics.quality_diagnostics.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunReport {
    pub run_id: String,
    #[cfg(test)]
    pub snapshot: SnapshotHandle,
    #[cfg(test)]
    pub snapshots: Vec<SnapshotHandle>,
    pub summary: ReviewRunSummary,
    #[cfg(test)]
    pub metrics: ConcurrentRunReport,
    pub artifacts: Arc<ArtifactStore>,
    pub(crate) snapshot_readers: Vec<SnapshotReader>,
    pub(crate) findings: Vec<FindingV1>,
    pub(crate) file_reviews: Vec<FileReviewV1>,
}

impl RunReport {
    pub fn snapshot_readers(&self) -> Vec<SnapshotReader> {
        self.snapshot_readers.clone()
    }

    #[cfg(test)]
    pub fn snapshot_reader(&self, snapshot_id: &SnapshotId) -> Option<SnapshotReader> {
        self.snapshot_readers
            .iter()
            .find(|reader| reader.snapshot_id() == snapshot_id)
            .cloned()
    }

    pub fn snapshot_summaries(&self) -> Vec<SnapshotSummary> {
        self.snapshot_readers
            .iter()
            .map(SnapshotReader::summary)
            .collect()
    }

    #[cfg(test)]
    pub fn snapshot_manifests(&self) -> Vec<SnapshotManifest> {
        self.snapshot_readers
            .iter()
            .map(SnapshotReader::manifest)
            .collect()
    }

    pub fn findings(&self) -> Vec<FindingView> {
        self.findings
            .iter()
            .map(FindingView::from_finding)
            .collect()
    }

    pub fn file_reviews(&self) -> Vec<FileReviewV1> {
        self.file_reviews.clone()
    }
}

#[derive(Debug, Clone)]
pub struct FindingView {
    pub id: String,
    pub title: String,
    pub claim: String,
    pub evidence_count: usize,
    pub publishable: bool,
    pub severity: String,
    pub confidence: f32,
    pub validation_status: String,
    pub evidence: Vec<EvidenceView>,
    pub location: Option<FindingLocationView>,
    pub related_paths: Vec<String>,
    pub discovered_by: Vec<String>,
    pub validated_by: Vec<String>,
    pub challenged_by: Vec<String>,
    pub challenge_status: String,
}

#[derive(Debug, Clone)]
pub struct FindingLocationView {
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

impl FindingView {
    fn from_finding(finding: &FindingV1) -> Self {
        let evidence = finding
            .evidence
            .iter()
            .map(EvidenceView::from_evidence)
            .collect::<Vec<_>>();
        let mut validated_by = evidence
            .iter()
            .map(|evidence| evidence.producing_tool_call_id.0.clone())
            .collect::<Vec<_>>();
        validated_by.sort();
        validated_by.dedup();
        Self {
            id: finding.id.clone(),
            title: finding.title.clone(),
            claim: finding.claim.clone(),
            evidence_count: finding.evidence.len(),
            publishable: matches!(
                finding.publishability,
                crate::reviewer_kernel::review_contract::FindingPublishability::Publishable
            ),
            severity: finding_severity_name(finding.severity).to_string(),
            confidence: finding.confidence,
            validation_status: validation_status_name(finding.validation_status).to_string(),
            evidence,
            location: finding_location_view(finding),
            related_paths: finding_related_paths(finding),
            discovered_by: finding.discovered_by.clone(),
            validated_by,
            challenged_by: finding.challenged_by.clone(),
            challenge_status: challenge_status_name(finding.challenge_status).to_string(),
        }
    }
}

fn challenge_status_name(
    status: crate::reviewer_kernel::review_contract::ChallengeStatus,
) -> &'static str {
    match status {
        crate::reviewer_kernel::review_contract::ChallengeStatus::Confirmed => "confirmed",
        crate::reviewer_kernel::review_contract::ChallengeStatus::Refuted => "refuted",
        crate::reviewer_kernel::review_contract::ChallengeStatus::Insufficient => "insufficient",
        crate::reviewer_kernel::review_contract::ChallengeStatus::NotRun => "not_run",
        crate::reviewer_kernel::review_contract::ChallengeStatus::Incomplete => "incomplete",
    }
}

fn evidence_kind_name(kind: crate::reviewer_kernel::review_contract::ArtifactKind) -> &'static str {
    match kind {
        crate::reviewer_kernel::review_contract::ArtifactKind::ToolSummary => "tool_summary",
    }
}

fn finding_severity_name(
    severity: crate::reviewer_kernel::review_contract::FindingSeverity,
) -> &'static str {
    match severity {
        crate::reviewer_kernel::review_contract::FindingSeverity::Blocker => "blocker",
        crate::reviewer_kernel::review_contract::FindingSeverity::High => "high",
        crate::reviewer_kernel::review_contract::FindingSeverity::Medium => "medium",
        crate::reviewer_kernel::review_contract::FindingSeverity::Low => "low",
        crate::reviewer_kernel::review_contract::FindingSeverity::Nit => "nit",
    }
}

fn validation_status_name(
    status: crate::reviewer_kernel::review_contract::ValidationStatus,
) -> &'static str {
    match status {
        crate::reviewer_kernel::review_contract::ValidationStatus::Challenged => "challenged",
        crate::reviewer_kernel::review_contract::ValidationStatus::Validated => "validated",
    }
}

fn finding_related_paths(finding: &FindingV1) -> Vec<String> {
    finding
        .file_refs
        .iter()
        .skip(1)
        .map(|location| match location {
            crate::reviewer_kernel::review_contract::EvidenceLocationV1::SinglePath { path } => {
                path.clone()
            }
        })
        .collect()
}

fn finding_location_view(finding: &FindingV1) -> Option<FindingLocationView> {
    if let Some(location) = finding.file_refs.first() {
        let path = match location {
            crate::reviewer_kernel::review_contract::EvidenceLocationV1::SinglePath { path } => {
                path.clone()
            }
        };
        let line_range = finding.location_line_range;
        return Some(FindingLocationView {
            path,
            start_line: line_range.map(|range| range.start_line),
            end_line: line_range.map(|range| range.end_line),
        });
    }
    finding
        .evidence
        .iter()
        .map(|evidence| {
            let path = match &evidence.location {
                crate::reviewer_kernel::review_contract::EvidenceLocationV1::SinglePath {
                    path,
                } => path.clone(),
            };
            FindingLocationView {
                path,
                start_line: evidence.line_range.map(|range| range.start_line),
                end_line: evidence.line_range.map(|range| range.end_line),
            }
        })
        .next()
}

#[derive(Debug, Clone)]
pub struct EvidenceView {
    pub evidence_id: String,
    pub artifact_id: ArtifactId,
    pub kind: String,
    pub content_hash: String,
    pub producing_tool_call_id: ToolCallId,
}

impl EvidenceView {
    fn from_evidence(evidence: &crate::reviewer_kernel::review_contract::EvidenceRefV1) -> Self {
        Self {
            evidence_id: evidence.evidence_id.clone(),
            artifact_id: ArtifactId(evidence.artifact_id.clone()),
            kind: evidence_kind_name(evidence.kind).to_string(),
            content_hash: evidence.content_hash.clone(),
            producing_tool_call_id: ToolCallId(evidence.producing_tool_call_id.clone()),
        }
    }
}
