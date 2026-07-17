use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeLimits, RuntimeResult};
use crate::workspace::{FileMeta, RepoSnapshot};

use super::metrics::ConcurrentAtomicCounters;

#[derive(Debug)]
pub(crate) struct ReadService {
    snapshot: Arc<RepoSnapshot>,
    limits: Arc<RuntimeLimits>,
    counters: Arc<ConcurrentAtomicCounters>,
}

impl ReadService {
    pub(super) fn new(
        snapshot: Arc<RepoSnapshot>,
        limits: Arc<RuntimeLimits>,
        counters: Arc<ConcurrentAtomicCounters>,
    ) -> Self {
        Self {
            snapshot,
            limits,
            counters,
        }
    }

    pub(super) async fn read_file(&self, file: &FileMeta) -> RuntimeResult<ReadResult> {
        let (bytes, truncated) = self
            .snapshot
            .read_bounded(file.file_id, self.limits.max_file_bytes_read)?;
        self.counters
            .read_file_reads
            .fetch_add(1, Ordering::Relaxed);
        decode_read(&bytes, truncated)
    }
}

#[derive(Debug)]
pub(super) struct ReadResult {
    pub(super) content: String,
    pub(super) truncated: bool,
}

fn decode_read(bytes: &[u8], truncated: bool) -> RuntimeResult<ReadResult> {
    let content = String::from_utf8(bytes.to_vec())
        .map_err(|_| RuntimeError::InvalidInput("file is not valid UTF-8".to_string()))?;
    Ok(ReadResult { content, truncated })
}
