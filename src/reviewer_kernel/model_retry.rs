//! Retry wrapper for model turns: transient provider errors (429s, 5xxs,
//! connection failures, per-attempt timeouts) are retried with exponential
//! backoff before a turn is declared failed. Every failed attempt emits a
//! `ModelFailed` event so hosts can observe retries; only the helper emits
//! these events, so call sites must not emit their own.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::{
    ConversationItem, ModelTurn, RuntimeError, RuntimeLimits, RuntimeResult, SessionScope, TurnId,
};
use crate::reviewer_kernel::model::{
    observe_model_limiter_waits, ConcurrentModelClient, ModelLimiterWaitSnapshot,
};
use crate::reviewer_kernel::policy::ReviewerPolicy;

pub(crate) struct ModelTurnOutcome {
    pub(crate) result: RuntimeResult<ModelTurn>,
    /// Model calls actually made; on success the last attempt is the one
    /// that succeeded, so `attempts - 1` of them errored.
    pub(crate) attempts: usize,
    pub(crate) limiter_wait: ModelLimiterWaitSnapshot,
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
    let mut limiter_wait = ModelLimiterWaitSnapshot::default();
    loop {
        attempt += 1;
        let (outcome, attempt_limiter_wait) = observe_model_limiter_waits(tokio::time::timeout(
            Duration::from_millis(limits.max_model_turn_ms.max(1)),
            model.complete(call_scope, transcript, turn_id, cancel.child_token()),
        ))
        .await;
        limiter_wait.add(attempt_limiter_wait);
        let error = match outcome {
            Ok(Ok(turn)) => {
                return ModelTurnOutcome {
                    result: Ok(turn),
                    attempts: attempt,
                    limiter_wait,
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
                limiter_wait,
            };
        }
        let delay = backoff_delay(limits, &event_scope.id.0, attempt);
        tokio::select! {
            _ = cancel.cancelled() => {
                return ModelTurnOutcome {
                    result: Err(RuntimeError::Cancelled),
                    attempts: attempt,
                    limiter_wait,
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
mod tests;
