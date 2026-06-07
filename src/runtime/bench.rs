use std::sync::Arc;

use anyhow::Result;

use crate::contracts::ReviewRunJobV1;
use crate::events::EventEmitter;
use crate::runtime::contracts::ConcurrentRunReport;

pub(crate) fn run_job_concurrent(job: ReviewRunJobV1) -> Result<ConcurrentRunReport> {
    run_job_concurrent_with_events(job, None)
}

pub(crate) fn run_job_concurrent_with_events(
    job: ReviewRunJobV1,
    emitter: Option<Arc<EventEmitter>>,
) -> Result<ConcurrentRunReport> {
    crate::reviewer::legacy::run_review_job_with_events(job, emitter)
}
