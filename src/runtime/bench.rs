use anyhow::Result;

use crate::contracts::ReviewRunJobV1;
use std::sync::Arc;

use crate::runtime::contracts::{ConcurrentRunReport, RuntimeEventSink};

pub(crate) fn run_job_concurrent(job: ReviewRunJobV1) -> Result<ConcurrentRunReport> {
    crate::reviewer::legacy::run_review_job(job)
}

pub(crate) fn run_job_concurrent_with_event_sink(
    job: ReviewRunJobV1,
    event_sink: Arc<dyn RuntimeEventSink>,
) -> Result<ConcurrentRunReport> {
    crate::reviewer::legacy::run_review_job_with_event_sink(job, Some(event_sink))
}
