use anyhow::Result;

use crate::contracts::ReviewRunJobV1;
use crate::runtime::contracts::ConcurrentRunReport;

pub(crate) fn run_job_concurrent(job: ReviewRunJobV1) -> Result<ConcurrentRunReport> {
    crate::reviewer::legacy::run_review_job(job)
}
