use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::kernel_types::{
    CacheStatus, RuntimeError, RuntimeLimits, RuntimeResult, ToolErrorCode, ToolInvocation,
    ToolProviderId, ToolResultEnvelope,
};

use super::engine::{elapsed_ms_allow_zero, ToolEngine};
use super::provider::ToolProvider;

pub(super) struct ToolProviderDispatcher {
    providers: Vec<Arc<dyn ToolProvider>>,
    provider_permits: DashMap<String, Arc<Semaphore>>,
    limits: Arc<RuntimeLimits>,
}

impl ToolProviderDispatcher {
    pub(super) fn new(providers: Vec<Arc<dyn ToolProvider>>, limits: Arc<RuntimeLimits>) -> Self {
        Self {
            providers,
            provider_permits: DashMap::new(),
            limits,
        }
    }

    pub(super) async fn execute(
        &self,
        engine: &ToolEngine,
        invocation: ToolInvocation,
        cancel: CancellationToken,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let call_id = invocation.call_id.clone();
        let tool_id = invocation.tool_id.clone();
        let Some(definition) = engine.registry().definition(&tool_id) else {
            return engine.error_result(
                call_id,
                tool_id,
                ToolErrorCode::UnknownTool,
                "tool is not registered",
                false,
            );
        };
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.accepts(definition))
        else {
            return engine.error_result(
                call_id,
                tool_id,
                ToolErrorCode::UnknownTool,
                "tool has no provider",
                false,
            );
        };
        let provider_id = provider.id();
        let timeout = Duration::from_millis(self.limits.max_tool_provider_ms.max(1));
        let queue_started = Instant::now();
        let permit = match self.acquire_provider_permit(&provider_id, timeout).await {
            Ok(permit) => permit,
            Err(error) => {
                let mut result = engine.runtime_error_result(call_id, tool_id, error);
                result.limits.queue_wait_ms = elapsed_ms_allow_zero(queue_started);
                return result;
            }
        };
        let queue_wait_ms = elapsed_ms_allow_zero(queue_started);
        let _permit = permit;
        let mut result = match tokio::time::timeout(
            timeout,
            provider.execute(engine, invocation, cancel, cache_status),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => engine.runtime_error_result(call_id, tool_id, error),
            Err(_) => engine.runtime_error_result(call_id, tool_id, RuntimeError::Timeout),
        };
        result.limits.queue_wait_ms = queue_wait_ms;
        result
    }

    async fn acquire_provider_permit(
        &self,
        provider_id: &ToolProviderId,
        timeout: Duration,
    ) -> RuntimeResult<OwnedSemaphorePermit> {
        let permits = self
            .provider_permits
            .entry(provider_id.as_str().to_string())
            .or_insert_with(|| {
                Arc::new(Semaphore::new(
                    self.limits
                        .max_tool_provider_concurrency_per_provider
                        .max(1),
                ))
            })
            .clone();
        tokio::time::timeout(timeout, permits.acquire_owned())
            .await
            .map_err(|_| RuntimeError::Timeout)?
            .map_err(|_| RuntimeError::Cancelled)
    }
}
