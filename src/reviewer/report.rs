use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::contracts::ToolCounts;
use crate::runtime::contracts::{
    AgentSessionOutput, ArtifactId, ArtifactView, ConcurrentCounters, ConcurrentRunReport,
    ModelMetricsSnapshot, RuntimeError, RuntimeEvent, RuntimeEventContext, RuntimeEventSink,
    RuntimeResult, SnapshotId, ToolCallId, ToolMetricKey, ToolMetricsSnapshot,
    ToolProviderHealthSnapshot, ToolProviderHealthState,
};

use crate::contracts::{FileReviewV1, FindingV1, ReviewOutcomeV1};

use crate::reviewer::adapters::metrics;
use crate::reviewer::adapters::{capabilities, tool_adapters};
use crate::reviewer::artifacts::*;
use crate::reviewer::events::*;
use crate::reviewer::snapshots::*;
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
    merged.benchmark_failures = planned_runtime_benchmark_failures(&merged);
    merged.benchmark_valid = merged.benchmark_failures.is_empty();
    merged
}

fn planned_runtime_benchmark_failures(report: &ConcurrentRunReport) -> Vec<String> {
    let mut failures = Vec::new();
    if report.sessions > 0 && report.completed_sessions == 0 {
        failures.push("no planned review units completed".to_string());
    }
    if report.sessions > 0 && report.model_calls == 0 {
        failures.push("no model calls recorded".to_string());
    }
    failures
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
    left.estimated_input_cost_micro_usd += right.estimated_input_cost_micro_usd;
    left.estimated_output_cost_micro_usd += right.estimated_output_cost_micro_usd;
    left.estimated_total_cost_micro_usd += right.estimated_total_cost_micro_usd;
    left.input_tokens += right.input_tokens;
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
    pub status: String,
    pub sessions: usize,
    pub completed_sessions: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub tool_counts: ToolCounts,
    pub findings: usize,
    pub publishable_findings: usize,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub artifacts: usize,
    pub artifact_bytes: usize,
    pub snapshot_count: usize,
    pub benchmark_valid: bool,
    pub benchmark_failure_count: usize,
}

impl ReviewRunSummary {
    pub(crate) fn from_metrics(metrics: &ConcurrentRunReport) -> Self {
        Self {
            status: if metrics.completed_sessions == metrics.sessions {
                "completed".to_string()
            } else {
                "partial".to_string()
            },
            sessions: metrics.sessions,
            completed_sessions: metrics.completed_sessions,
            model_calls: metrics.model_calls,
            tool_calls: metrics.tool_calls,
            tool_counts: metrics.tool_counts,
            findings: metrics.findings,
            publishable_findings: metrics.publishable_findings,
            elapsed_ms: metrics.elapsed_ms,
            input_tokens: metrics.input_tokens,
            output_tokens: metrics.output_tokens,
            total_tokens: metrics.total_tokens,
            artifacts: metrics.artifacts,
            artifact_bytes: metrics.artifact_bytes,
            snapshot_count: metrics.snapshot_metrics.len(),
            benchmark_valid: metrics.benchmark_valid,
            benchmark_failure_count: metrics.benchmark_failures.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunReport {
    pub run_id: String,
    pub snapshot: SnapshotHandle,
    pub snapshots: Vec<SnapshotHandle>,
    pub summary: ReviewRunSummary,
    pub metrics: metrics::ConcurrentRunReport,
    pub artifacts: Arc<tool_adapters::ArtifactStore>,
    /// Per-session final outputs; populated by direct-session runs only.
    pub session_outputs: Vec<AgentSessionOutput>,
    pub(crate) snapshot_readers: Vec<SnapshotReader>,
    pub(crate) findings: Vec<FindingV1>,
    pub(crate) file_reviews: Vec<FileReviewV1>,
}

impl RunReport {
    pub fn snapshot_readers(&self) -> Vec<SnapshotReader> {
        self.snapshot_readers.clone()
    }

    pub fn snapshot_reader(&self, snapshot_id: &SnapshotId) -> Option<SnapshotReader> {
        self.snapshot_readers
            .iter()
            .find(|reader| reader.snapshot_id() == snapshot_id)
            .cloned()
    }

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

    pub fn finding_evidence_artifacts(
        &self,
        finding_id: &str,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<Vec<EvidenceArtifactView>> {
        let Some(finding) = self
            .findings
            .iter()
            .find(|finding| finding.id == finding_id)
        else {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown finding id {finding_id}"
            )));
        };
        let artifacts = finding
            .evidence
            .iter()
            .filter(|evidence| policy.allows_artifact(&ArtifactId(evidence.artifact_id.clone())))
            .map(|evidence| {
                let artifact_id = ArtifactId(evidence.artifact_id.clone());
                let artifact = if policy.include_raw() {
                    self.artifacts.get_raw(&artifact_id)
                } else {
                    self.artifacts.get(&artifact_id)
                }
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "missing evidence artifact {}",
                        evidence.artifact_id
                    ))
                })?;
                Ok(EvidenceArtifactView {
                    evidence: EvidenceView::from_evidence(evidence),
                    artifact,
                })
            })
            .collect::<RuntimeResult<Vec<_>>>()?;
        policy.validate_retention(
            artifacts.len(),
            artifacts
                .iter()
                .map(|evidence_artifact| evidence_artifact.artifact.bytes)
                .sum(),
        )?;
        Ok(artifacts)
    }

    pub fn redacted_artifacts(&self) -> ReviewArtifacts<'_> {
        ReviewArtifacts::new(self, ArtifactExportPolicy::redacted_all())
    }

    pub fn raw_artifacts(
        &self,
        capabilities: &capabilities::CapabilitySet,
    ) -> RuntimeResult<ReviewArtifacts<'_>> {
        Ok(ReviewArtifacts::new(
            self,
            ArtifactExportPolicy::raw(capabilities)?,
        ))
    }

    pub fn export_artifacts(
        &self,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactExportManifest> {
        self.artifacts.export_with_policy(policy)
    }

    pub fn persist_artifacts(
        &self,
        object_store: &dyn ArtifactObjectStore,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactPersistenceManifest> {
        self.artifacts.persist_with_policy(object_store, policy)
    }

    pub fn export_artifact_bundle(
        &self,
        root: impl AsRef<Path>,
        policy: ArtifactExportPolicy,
    ) -> RuntimeResult<ArtifactBundleManifest> {
        self.artifacts.export_bundle(root.as_ref(), policy)
    }
}

#[derive(Debug, Clone)]
pub struct ReviewArtifacts<'a> {
    report: &'a RunReport,
    policy: ArtifactExportPolicy,
}

impl<'a> ReviewArtifacts<'a> {
    fn new(report: &'a RunReport, policy: ArtifactExportPolicy) -> Self {
        Self { report, policy }
    }

    pub fn only_artifacts<I, S>(mut self, artifact_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.policy = self.policy.with_artifact_ids(artifact_ids);
        self
    }

    pub fn with_retention_policy(mut self, retention: ArtifactRetentionPolicy) -> Self {
        self.policy = self.policy.with_retention_policy(retention);
        self
    }

    pub fn export(&self) -> RuntimeResult<ArtifactExportManifest> {
        self.report.export_artifacts(self.policy.clone())
    }

    pub fn finding_evidence(&self, finding_id: &str) -> RuntimeResult<Vec<EvidenceArtifactView>> {
        self.report
            .finding_evidence_artifacts(finding_id, self.policy.clone())
    }

    pub fn persist_to(
        &self,
        object_store: &dyn ArtifactObjectStore,
    ) -> RuntimeResult<ArtifactPersistenceManifest> {
        self.report
            .persist_artifacts(object_store, self.policy.clone())
    }

    pub fn bundle_at(&self, root: impl AsRef<Path>) -> RuntimeResult<ArtifactBundleManifest> {
        self.report
            .export_artifact_bundle(root, self.policy.clone())
    }

    pub fn policy(&self) -> &ArtifactExportPolicy {
        &self.policy
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
    pub discovered_by: Vec<String>,
    pub validated_by: Vec<String>,
    pub challenged_by: Vec<String>,
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
                crate::contracts::FindingPublishability::Publishable
            ),
            severity: finding_severity_name(finding.severity).to_string(),
            confidence: finding.confidence,
            validation_status: validation_status_name(finding.validation_status).to_string(),
            evidence,
            location: finding_location_view(finding),
            discovered_by: finding.discovered_by.clone(),
            validated_by,
            challenged_by: finding.challenged_by.clone(),
        }
    }
}

fn finding_location_view(finding: &FindingV1) -> Option<FindingLocationView> {
    if let Some(location) = finding.file_refs.first() {
        let path = match location {
            crate::contracts::EvidenceLocationV1::SinglePath { path } => path.clone(),
            crate::contracts::EvidenceLocationV1::Rename { new_path, .. } => new_path.clone(),
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
                crate::contracts::EvidenceLocationV1::SinglePath { path } => path.clone(),
                crate::contracts::EvidenceLocationV1::Rename { new_path, .. } => new_path.clone(),
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
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id.0
    }

    pub fn producing_tool_call_id(&self) -> &str {
        &self.producing_tool_call_id.0
    }

    fn from_evidence(evidence: &crate::contracts::EvidenceRefV1) -> Self {
        Self {
            evidence_id: evidence.evidence_id.clone(),
            artifact_id: ArtifactId(evidence.artifact_id.clone()),
            kind: evidence_kind_name(evidence.kind).to_string(),
            content_hash: evidence.content_hash.clone(),
            producing_tool_call_id: ToolCallId(evidence.producing_tool_call_id.clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvidenceArtifactView {
    pub evidence: EvidenceView,
    pub artifact: ArtifactView,
}

impl EvidenceArtifactView {
    pub fn artifact_id(&self) -> &str {
        self.artifact.artifact_id()
    }
}

impl ArtifactView {
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id.0
    }
}

#[derive(Debug, Clone)]
pub struct RunHandle {
    pub run_id: String,
}

pub(crate) fn concurrent_review_outcome(
    report: &ConcurrentRunReport,
    findings: usize,
) -> ReviewOutcomeV1 {
    if report.completed_sessions < report.sessions {
        ReviewOutcomeV1::FailedPartial
    } else if findings > 0 {
        ReviewOutcomeV1::CompletedWithFindings
    } else {
        ReviewOutcomeV1::CompletedNoFindings
    }
}
