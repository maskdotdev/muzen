use std::sync::Arc;

use anyhow::Result;

use crate::contracts::ReviewRunJobV1;
use crate::events::EventEmitter;
use crate::runtime::contracts::{ComparisonReport, ConcurrentRunReport};

pub(crate) fn optimization_failures(
    baseline: &ConcurrentRunReport,
    concurrent: &ConcurrentRunReport,
    speedup: f64,
) -> Vec<String> {
    let mut failures = Vec::new();
    if concurrent.counters.search_scans > baseline.counters.search_scans {
        failures.push(format!(
            "concurrent search scanned more batches than baseline: {} > {}",
            concurrent.counters.search_scans, baseline.counters.search_scans
        ));
    }
    if concurrent.elapsed_ms > baseline.elapsed_ms.saturating_mul(4).max(1) {
        failures.push(format!(
            "concurrent runtime exceeded 4x baseline wall time: {}ms vs {}ms",
            concurrent.elapsed_ms, baseline.elapsed_ms
        ));
    }
    if speedup.is_finite() && speedup < 0.25 {
        failures.push(format!("measured speedup below floor: {speedup:.2}x"));
    }
    failures
}

pub(crate) fn run_job_concurrent(job: ReviewRunJobV1) -> Result<ConcurrentRunReport> {
    run_job_concurrent_with_events(job, None)
}

pub(crate) fn run_job_concurrent_with_events(
    job: ReviewRunJobV1,
    emitter: Option<Arc<EventEmitter>>,
) -> Result<ConcurrentRunReport> {
    crate::reviewer::legacy::run_review_job_with_events(job, emitter)
}

#[allow(dead_code)]
fn _keep_comparison_report_linked(_: Option<ComparisonReport>) {}
