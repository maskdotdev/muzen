use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use moka::future::Cache;
use parking_lot::Mutex;
use tokio::sync::{Mutex as TokioMutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::kernel_types::{
    CacheStatus, ToolErrorCode, ToolInvocation, ToolProviderId, ToolResultEnvelope,
};
use crate::reviewer_kernel::review_contract::ToolName;

use super::metrics::ConcurrentAtomicCounters;

#[derive(Debug, Default)]
struct InflightToolResult {
    result: TokioMutex<Option<Arc<ToolResultEnvelope>>>,
    notify: Notify,
}

pub(super) struct ToolResultCache {
    result_cache: Cache<String, Arc<ToolResultEnvelope>>,
    inflight: Mutex<HashMap<String, Arc<InflightToolResult>>>,
}

impl ToolResultCache {
    pub(super) fn new(max_cache_weight: u64) -> Self {
        Self {
            result_cache: Cache::new(max_cache_weight.max(1)),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    pub(super) async fn execute<F, Fut>(
        &self,
        key: String,
        invocation: ToolInvocation,
        cancel: CancellationToken,
        counters: &ConcurrentAtomicCounters,
        snapshot_id: crate::reviewer_kernel::kernel_types::SnapshotId,
        compute: F,
    ) -> ToolResultEnvelope
    where
        F: FnOnce(ToolInvocation, CancellationToken, CacheStatus) -> Fut,
        Fut: Future<Output = ToolResultEnvelope>,
    {
        if let Some(hit) = self.result_cache.get(&key).await {
            counters.artifact_cache_hits.fetch_add(1, Ordering::Relaxed);
            return hit.for_call(invocation.call_id, invocation.tool_id, CacheStatus::Hit);
        }
        let (cell, owner) = {
            let mut inflight = self.inflight.lock();
            if let Some(cell) = inflight.get(&key) {
                (Arc::clone(cell), false)
            } else {
                let cell = Arc::new(InflightToolResult::default());
                inflight.insert(key.clone(), Arc::clone(&cell));
                (cell, true)
            }
        };
        if owner {
            let result = Arc::new(compute(invocation, cancel, CacheStatus::Miss).await);
            *cell.result.lock().await = Some(Arc::clone(&result));
            cell.notify.notify_waiters();
            if result.ok {
                self.result_cache
                    .insert(key.clone(), Arc::clone(&result))
                    .await;
            }
            self.inflight.lock().remove(&key);
            result.as_ref().clone()
        } else {
            counters.search_dedupe_waiters.fetch_add(
                (invocation.builtin_name == Some(ToolName::SearchText)) as usize,
                Ordering::Relaxed,
            );
            loop {
                if let Some(result) = cell.result.lock().await.clone() {
                    return result.for_call(
                        invocation.call_id,
                        invocation.tool_id,
                        CacheStatus::Deduped,
                    );
                }
                tokio::select! {
                    _ = cancel.cancelled() => {
                        return cancelled_result(invocation, snapshot_id);
                    }
                    _ = cell.notify.notified() => {}
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn inflight_count(&self) -> usize {
        self.inflight.lock().len()
    }
}

fn cancelled_result(
    invocation: ToolInvocation,
    snapshot_id: crate::reviewer_kernel::kernel_types::SnapshotId,
) -> ToolResultEnvelope {
    ToolResultEnvelope {
        ok: false,
        tool_call_id: invocation.call_id,
        tool_name: invocation.tool_id,
        provider_id: ToolProviderId::builtin_review(),
        snapshot_id,
        artifact_id: None,
        cache: crate::reviewer_kernel::kernel_types::CacheInfo {
            status: CacheStatus::Deduped,
            key_hash: None,
        },
        limits: crate::reviewer_kernel::kernel_types::LimitInfo::default(),
        data: None,
        error: Some(crate::reviewer_kernel::kernel_types::ToolErrorInfo {
            code: ToolErrorCode::Cancelled,
            message: "operation cancelled".to_string(),
            retryable: true,
            partial: true,
        }),
    }
}
