use std::collections::BTreeMap;

use serde::Serialize;

use super::{ArtifactId, SnapshotId, ToolMetricKey, ToolProviderId};
use crate::reviewer_kernel::review_contract::ToolCounts;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConcurrentCounters {
    pub search_scans: usize,
    pub search_dedupe_waiters: usize,
    pub search_cache_hits: usize,
    pub read_cache_hits: usize,
    pub read_file_reads: usize,
    pub tool_errors: usize,
    pub artifact_cache_hits: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolMetricsSnapshot {
    pub calls: usize,
    pub successes: usize,
    pub errors: usize,
    pub cache_hits: usize,
    pub deduped: usize,
    pub timeouts: usize,
    pub cancellations: usize,
    pub artifacts: usize,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub latency_ms: u64,
    pub max_latency_ms: u64,
    pub queue_wait_ms: u64,
    pub max_queue_wait_ms: u64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProviderHealthState {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProviderHealthSnapshot {
    pub provider_id: ToolProviderId,
    pub state: ToolProviderHealthState,
    pub calls: usize,
    pub errors: usize,
    pub timeouts: usize,
    pub cancellations: usize,
    pub consecutive_errors: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetricsSnapshot {
    pub calls: usize,
    pub successes: usize,
    pub errors: usize,
    pub retries: usize,
    pub costed_calls: usize,
    pub unpriced_calls: usize,
    pub latency_ms: u64,
    pub max_latency_ms: u64,
    pub limiter_wait_ms: u64,
    pub max_limiter_wait_ms: u64,
    pub estimated_input_cost_micro_usd: u64,
    pub estimated_output_cost_micro_usd: u64,
    pub estimated_total_cost_micro_usd: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
}

#[derive(Debug, Default, Copy, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostEstimate {
    pub input_cost_micro_usd: u64,
    pub output_cost_micro_usd: u64,
    pub total_cost_micro_usd: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactView {
    pub artifact_id: ArtifactId,
    pub bytes: usize,
    pub content_hash: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompletionDiagnostic {
    pub session_id: String,
    pub completed: bool,
    pub completion_kind: Option<String>,
    pub completion_summary: Option<String>,
    pub saw_diff: bool,
    pub saw_file: bool,
    pub saw_search: bool,
    pub model_calls: usize,
    pub tool_counts: ToolCounts,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMetricsSnapshot {
    pub snapshot_id: SnapshotId,
    pub sessions: usize,
    pub completed_sessions: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub artifacts: usize,
    pub artifact_bytes: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewQualityDiagnostics {
    pub contract_risk_units: usize,
    pub contract_seed_count: usize,
    pub contract_pack_count: usize,
    pub omitted_contract_pack_candidates: Vec<String>,
    pub selected_contract_packs: Vec<String>,
    pub contract_evidence_failures: usize,
    pub coverage_counts: BTreeMap<String, usize>,
    pub coverage_counts_by_lens: BTreeMap<String, BTreeMap<String, usize>>,
    pub high_risk_files_below_target: Vec<String>,
    pub challenge_status_counts: BTreeMap<String, usize>,
    pub sessions_run: usize,
    pub budgets_used: BTreeMap<String, usize>,
    pub explicit_caller_cap_sessions: usize,
    pub candidate_findings: usize,
    pub rescued_candidates: usize,
    pub rejected_candidates: usize,
    pub rejection_reasons: BTreeMap<String, usize>,
}

impl ReviewQualityDiagnostics {
    pub fn add(&mut self, other: Self) {
        self.contract_risk_units += other.contract_risk_units;
        self.contract_seed_count += other.contract_seed_count;
        self.contract_pack_count += other.contract_pack_count;
        self.omitted_contract_pack_candidates
            .extend(other.omitted_contract_pack_candidates);
        self.selected_contract_packs
            .extend(other.selected_contract_packs);
        self.contract_evidence_failures += other.contract_evidence_failures;
        merge_counts(&mut self.coverage_counts, other.coverage_counts);
        for (lens, counts) in other.coverage_counts_by_lens {
            merge_counts(
                self.coverage_counts_by_lens.entry(lens).or_default(),
                counts,
            );
        }
        self.high_risk_files_below_target
            .extend(other.high_risk_files_below_target);
        merge_counts(
            &mut self.challenge_status_counts,
            other.challenge_status_counts,
        );
        self.sessions_run += other.sessions_run;
        merge_counts(&mut self.budgets_used, other.budgets_used);
        self.explicit_caller_cap_sessions += other.explicit_caller_cap_sessions;
        self.candidate_findings += other.candidate_findings;
        self.rescued_candidates += other.rescued_candidates;
        self.rejected_candidates += other.rejected_candidates;
        for (reason, count) in other.rejection_reasons {
            *self.rejection_reasons.entry(reason).or_insert(0) += count;
        }
    }
}

impl Default for ReviewQualityDiagnostics {
    fn default() -> Self {
        Self {
            contract_risk_units: 0,
            contract_seed_count: 0,
            contract_pack_count: 0,
            omitted_contract_pack_candidates: Vec::new(),
            selected_contract_packs: Vec::new(),
            contract_evidence_failures: 0,
            coverage_counts: BTreeMap::new(),
            coverage_counts_by_lens: BTreeMap::new(),
            high_risk_files_below_target: Vec::new(),
            challenge_status_counts: BTreeMap::new(),
            sessions_run: 0,
            budgets_used: BTreeMap::new(),
            explicit_caller_cap_sessions: 0,
            candidate_findings: 0,
            rescued_candidates: 0,
            rejected_candidates: 0,
            rejection_reasons: BTreeMap::new(),
        }
    }
}

fn merge_counts(target: &mut BTreeMap<String, usize>, source: BTreeMap<String, usize>) {
    for (key, count) in source {
        *target.entry(key).or_insert(0) += count;
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConcurrentRunReport {
    pub runtime: &'static str,
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
    pub cached_input_tokens: u64,
    pub artifacts: usize,
    pub artifact_bytes: usize,
    pub counters: ConcurrentCounters,
    pub tool_metrics: BTreeMap<ToolMetricKey, ToolMetricsSnapshot>,
    pub provider_health: Vec<ToolProviderHealthSnapshot>,
    pub snapshot_metrics: Vec<SnapshotMetricsSnapshot>,
    pub model_metrics: ModelMetricsSnapshot,
    pub completion_diagnostics: Vec<SessionCompletionDiagnostic>,
    pub quality_diagnostics: ReviewQualityDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComparisonReport {
    pub sessions: usize,
    pub sync: ConcurrentRunReport,
    pub concurrent: ConcurrentRunReport,
    pub speedup: f64,
    pub search_scan_reduction: f64,
    pub optimization_valid: bool,
    pub optimization_failures: Vec<String>,
}
