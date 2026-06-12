//! Retry wrapper for model turns: transient provider errors (429s, 5xxs,
//! connection failures, per-attempt timeouts) are retried with exponential
//! backoff before a turn is declared failed. Every failed attempt emits a
//! `ModelFailed` event so hosts can observe retries; only the helper emits
//! these events, so call sites must not emit their own.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::runtime::contracts::{
    ConversationItem, ModelTurn, RuntimeError, RuntimeLimits, RuntimeResult, SessionScope, TurnId,
};
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::model::ConcurrentModelClient;
use crate::runtime::policy::ReviewerPolicy;

pub(crate) struct ModelTurnOutcome {
    pub(crate) result: RuntimeResult<ModelTurn>,
    /// Model calls actually made; on success the last attempt is the one
    /// that succeeded, so `attempts - 1` of them errored.
    pub(crate) attempts: usize,
}

/// `event_scope` names the session in emitted events; `call_scope` may differ
/// when the call strips capabilities (e.g. the text-only final turn).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_model_turn(
    model: &dyn ConcurrentModelClient,
    policy: &ReviewerPolicy,
    events: &RuntimeEventDispatcher,
    limits: &RuntimeLimits,
    event_scope: &SessionScope,
    call_scope: &SessionScope,
    transcript: &[ConversationItem],
    turn_id: TurnId,
    cancel: &CancellationToken,
) -> ModelTurnOutcome {
    let max_attempts = limits.model_retry_max_attempts.max(1);
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let outcome = tokio::time::timeout(
            Duration::from_millis(limits.max_model_turn_ms.max(1)),
            model.complete(call_scope, transcript, turn_id, cancel.child_token()),
        )
        .await;
        let error = match outcome {
            Ok(Ok(turn)) => {
                return ModelTurnOutcome {
                    result: Ok(turn),
                    attempts: attempt,
                }
            }
            Ok(Err(error)) => error,
            Err(_) => RuntimeError::Timeout,
        };
        let retrying =
            attempt < max_attempts && retryable_model_error(&error) && !cancel.is_cancelled();
        events.emit_planned_runtime(policy.plan_model_failed_runtime_event(
            event_scope,
            turn_id,
            attempt,
            retrying,
            &error,
        ));
        if !retrying {
            return ModelTurnOutcome {
                result: Err(error),
                attempts: attempt,
            };
        }
        let delay = backoff_delay(limits, &event_scope.id.0, attempt);
        tokio::select! {
            _ = cancel.cancelled() => {
                return ModelTurnOutcome {
                    result: Err(RuntimeError::Cancelled),
                    attempts: attempt,
                }
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn retryable_model_error(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::Provider { retryable, .. }
        | RuntimeError::ProviderMessage { retryable, .. } => *retryable,
        RuntimeError::Timeout => true,
        _ => false,
    }
}

/// Exponential backoff with deterministic per-session jitter in
/// `[delay/2, delay]`, so a swarm of sessions tripping the same rate limit
/// does not retry in lockstep.
fn backoff_delay(limits: &RuntimeLimits, session_id: &str, attempt: usize) -> Duration {
    let base = limits.model_retry_base_delay_ms.max(1);
    let cap = limits.model_retry_max_delay_ms.max(base);
    let shift = attempt.saturating_sub(1).min(16) as u32;
    let delay = base.saturating_mul(1u64 << shift).min(cap);
    let floor = delay / 2;
    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    attempt.hash(&mut hasher);
    let jitter = hasher.finish() % (delay - floor + 1);
    Duration::from_millis(floor + jitter)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::contracts::{AgentBudget, Role};
    use crate::runtime::contracts::{CapabilitySet, RuntimeEvent, RuntimeEventSink, SessionId};

    struct FlakyModel {
        calls: AtomicUsize,
        failures_before_success: usize,
        error: fn() -> RuntimeError,
    }

    #[async_trait]
    impl ConcurrentModelClient for FlakyModel {
        async fn complete(
            &self,
            _scope: &SessionScope,
            _transcript: &[ConversationItem],
            _turn_id: TurnId,
            _cancel: CancellationToken,
        ) -> RuntimeResult<ModelTurn> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures_before_success {
                return Err((self.error)());
            }
            Ok(ModelTurn::Text {
                content: "ok".to_string(),
                usage: Default::default(),
            })
        }
    }

    #[derive(Default)]
    struct CaptureSink {
        events: Mutex<Vec<RuntimeEvent>>,
    }

    impl RuntimeEventSink for CaptureSink {
        fn emit(&self, event: RuntimeEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn retryable_provider_error() -> RuntimeError {
        RuntimeError::Provider {
            status: Some(429),
            retryable: true,
        }
    }

    fn non_retryable_provider_error() -> RuntimeError {
        RuntimeError::ProviderMessage {
            status: Some(429),
            retryable: false,
            message: "insufficient_quota".to_string(),
        }
    }

    fn test_scope() -> SessionScope {
        SessionScope {
            id: SessionId("retry-test".to_string()),
            role: Role::Generalist,
            objective: "retry test".to_string(),
            instructions: Vec::new(),
            snapshot_id: None,
            model_profile_id: None,
            capabilities: CapabilitySet::review_read_only(),
            budget: AgentBudget {
                max_turns: 1,
                max_tool_calls: 1,
                max_prompt_tokens: 1_024,
                max_output_tokens: 64,
            },
        }
    }

    fn fast_retry_limits(max_attempts: usize) -> RuntimeLimits {
        let mut limits = RuntimeLimits::standard(1, 64_000, 50);
        limits.model_retry_max_attempts = max_attempts;
        limits.model_retry_base_delay_ms = 1;
        limits.model_retry_max_delay_ms = 2;
        limits
    }

    async fn run_complete(
        model: &FlakyModel,
        limits: &RuntimeLimits,
        sink: Arc<CaptureSink>,
    ) -> ModelTurnOutcome {
        let scope = test_scope();
        let events = RuntimeEventDispatcher::new(Some(sink), None);
        complete_model_turn(
            model,
            &ReviewerPolicy::new(),
            &events,
            limits,
            &scope,
            &scope,
            &[],
            TurnId(0),
            &CancellationToken::new(),
        )
        .await
    }

    fn model_failed_flags(sink: &CaptureSink) -> Vec<(usize, bool)> {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ModelFailed {
                    attempt, retrying, ..
                } => Some((*attempt, *retrying)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn retries_retryable_errors_until_success() {
        let model = FlakyModel {
            calls: AtomicUsize::new(0),
            failures_before_success: 2,
            error: retryable_provider_error,
        };
        let sink = Arc::new(CaptureSink::default());
        let outcome = run_complete(&model, &fast_retry_limits(3), Arc::clone(&sink)).await;
        assert!(outcome.result.is_ok());
        assert_eq!(outcome.attempts, 3);
        assert_eq!(model_failed_flags(&sink), vec![(1, true), (2, true)]);
    }

    #[tokio::test]
    async fn exhausted_attempts_fail_with_final_event() {
        let model = FlakyModel {
            calls: AtomicUsize::new(0),
            failures_before_success: usize::MAX,
            error: retryable_provider_error,
        };
        let sink = Arc::new(CaptureSink::default());
        let outcome = run_complete(&model, &fast_retry_limits(3), Arc::clone(&sink)).await;
        assert!(outcome.result.is_err());
        assert_eq!(outcome.attempts, 3);
        assert_eq!(
            model_failed_flags(&sink),
            vec![(1, true), (2, true), (3, false)]
        );
    }

    #[tokio::test]
    async fn non_retryable_errors_fail_immediately() {
        let model = FlakyModel {
            calls: AtomicUsize::new(0),
            failures_before_success: usize::MAX,
            error: non_retryable_provider_error,
        };
        let sink = Arc::new(CaptureSink::default());
        let outcome = run_complete(&model, &fast_retry_limits(3), Arc::clone(&sink)).await;
        assert!(outcome.result.is_err());
        assert_eq!(outcome.attempts, 1);
        assert_eq!(model_failed_flags(&sink), vec![(1, false)]);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_grows_exponentially_within_bounds() {
        let mut limits = RuntimeLimits::standard(1, 64_000, 50);
        limits.model_retry_base_delay_ms = 100;
        limits.model_retry_max_delay_ms = 1_000;
        let mut previous_cap = 0u64;
        for attempt in 1..=5 {
            let delay = backoff_delay(&limits, "session", attempt).as_millis() as u64;
            let cap = (100u64 << (attempt - 1)).min(1_000);
            assert!(delay >= cap / 2, "attempt {attempt}: {delay} < {}", cap / 2);
            assert!(delay <= cap, "attempt {attempt}: {delay} > {cap}");
            assert!(cap >= previous_cap);
            previous_cap = cap;
        }
    }
}
