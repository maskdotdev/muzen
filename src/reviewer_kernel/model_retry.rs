//! Retry wrapper for model turns: transient provider errors (429s, 5xxs,
//! connection failures, per-attempt timeouts) are retried with exponential
//! backoff before a turn is declared failed. Every failed attempt emits a
//! `ModelFailed` event so hosts can observe retries; only the helper emits
//! these events, so call sites must not emit their own.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::{
    ConversationItem, ModelTurn, RuntimeError, RuntimeLimits, RuntimeResult, SessionScope, TurnId,
};
use crate::reviewer_kernel::model::{
    observe_model_limiter_waits, ConcurrentModelClient, ModelLimiterWaitSnapshot,
};
use crate::reviewer_kernel::policy::ReviewerPolicy;
use crate::reviewer_kernel::system::timestamp_utc;

pub(crate) struct ModelTurnOutcome {
    pub(crate) result: RuntimeResult<ModelTurn>,
    /// Model calls actually made; on success the last attempt is the one
    /// that succeeded, so `attempts - 1` of them errored.
    pub(crate) attempts: usize,
    pub(crate) diagnostics: ModelTurnDiagnostics,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ModelTurnDiagnostics {
    pub(crate) lifecycle_started_at_utc: Option<String>,
    pub(crate) queued_at_utc: Option<String>,
    pub(crate) completed_at_utc: Option<String>,
    pub(crate) lifecycle_ms: u64,
    pub(crate) limiter_wait: ModelLimiterWaitSnapshot,
    pub(crate) provider_request_ms: u64,
    pub(crate) max_provider_request_ms: u64,
    pub(crate) retry_backoff_ms: u64,
    pub(crate) max_retry_backoff_ms: u64,
    pub(crate) timeout_errors: usize,
    pub(crate) cancellation_errors: usize,
    pub(crate) retryable_provider_errors: usize,
    pub(crate) non_retryable_provider_errors: usize,
    pub(crate) other_errors: usize,
    pub(crate) terminal_failure_kind: Option<ModelFailureKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFailureKind {
    Timeout,
    Cancelled,
    ProviderRetryable,
    ProviderNonRetryable,
    Other,
}

impl ModelFailureKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ModelFailureKind::Timeout => "timeout",
            ModelFailureKind::Cancelled => "cancelled",
            ModelFailureKind::ProviderRetryable => "provider_retryable",
            ModelFailureKind::ProviderNonRetryable => "provider_non_retryable",
            ModelFailureKind::Other => "other",
        }
    }
}

impl ModelTurnDiagnostics {
    fn record_attempt(
        &mut self,
        queued_at_utc: String,
        limiter_wait: ModelLimiterWaitSnapshot,
        attempt_ms: u64,
    ) {
        if self.queued_at_utc.is_none() {
            self.queued_at_utc = Some(queued_at_utc);
        }
        self.limiter_wait.add(limiter_wait);
        let provider_request_ms = attempt_ms.saturating_sub(limiter_wait.total_wait_ms);
        self.provider_request_ms += provider_request_ms;
        self.max_provider_request_ms = self.max_provider_request_ms.max(provider_request_ms);
    }

    fn record_error(&mut self, error: &RuntimeError) {
        match classify_model_error(error) {
            ModelFailureKind::Timeout => self.timeout_errors += 1,
            ModelFailureKind::Cancelled => self.cancellation_errors += 1,
            ModelFailureKind::ProviderRetryable => self.retryable_provider_errors += 1,
            ModelFailureKind::ProviderNonRetryable => self.non_retryable_provider_errors += 1,
            ModelFailureKind::Other => self.other_errors += 1,
        }
    }

    fn record_backoff(&mut self, elapsed_ms: u64) {
        self.retry_backoff_ms += elapsed_ms;
        self.max_retry_backoff_ms = self.max_retry_backoff_ms.max(elapsed_ms);
    }
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
    let lifecycle_started = Instant::now();
    let max_attempts = limits.model_retry_max_attempts.max(1);
    let mut attempt = 0usize;
    let mut diagnostics = ModelTurnDiagnostics {
        lifecycle_started_at_utc: Some(timestamp_utc()),
        ..ModelTurnDiagnostics::default()
    };
    loop {
        attempt += 1;
        let attempt_queued_at_utc = timestamp_utc();
        let attempt_started = Instant::now();
        let (outcome, attempt_limiter_wait) = observe_model_limiter_waits(tokio::time::timeout(
            Duration::from_millis(limits.max_model_turn_ms.max(1)),
            model.complete(call_scope, transcript, turn_id, cancel.child_token()),
        ))
        .await;
        diagnostics.record_attempt(
            attempt_queued_at_utc,
            attempt_limiter_wait,
            elapsed_ms(attempt_started),
        );
        let error = match outcome {
            Ok(Ok(turn)) => {
                return finish_model_turn(Ok(turn), attempt, diagnostics, lifecycle_started)
            }
            Ok(Err(error)) => error,
            Err(_) => RuntimeError::Timeout,
        };
        diagnostics.record_error(&error);
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
            diagnostics.terminal_failure_kind = Some(classify_model_error(&error));
            return finish_model_turn(Err(error), attempt, diagnostics, lifecycle_started);
        }
        let delay = backoff_delay(limits, &event_scope.id.0, attempt);
        let backoff_started = Instant::now();
        tokio::select! {
            _ = cancel.cancelled() => {
                diagnostics.record_backoff(elapsed_ms(backoff_started));
                diagnostics.terminal_failure_kind = Some(ModelFailureKind::Cancelled);
                return finish_model_turn(
                    Err(RuntimeError::Cancelled),
                    attempt,
                    diagnostics,
                    lifecycle_started,
                );
            }
            _ = tokio::time::sleep(delay) => {
                diagnostics.record_backoff(elapsed_ms(backoff_started));
            }
        }
    }
}

fn finish_model_turn(
    result: RuntimeResult<ModelTurn>,
    attempts: usize,
    mut diagnostics: ModelTurnDiagnostics,
    lifecycle_started: Instant,
) -> ModelTurnOutcome {
    diagnostics.lifecycle_ms = elapsed_ms(lifecycle_started);
    diagnostics.completed_at_utc = Some(timestamp_utc());
    ModelTurnOutcome {
        result,
        attempts,
        diagnostics,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_micros().div_ceil(1000) as u64
}

fn retryable_model_error(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::Provider { retryable, .. }
        | RuntimeError::ProviderMessage { retryable, .. } => *retryable,
        RuntimeError::Timeout => true,
        _ => false,
    }
}

fn classify_model_error(error: &RuntimeError) -> ModelFailureKind {
    match error {
        RuntimeError::Timeout => ModelFailureKind::Timeout,
        RuntimeError::Cancelled => ModelFailureKind::Cancelled,
        RuntimeError::Provider { retryable, .. }
        | RuntimeError::ProviderMessage { retryable, .. } => {
            if *retryable {
                ModelFailureKind::ProviderRetryable
            } else {
                ModelFailureKind::ProviderNonRetryable
            }
        }
        _ => ModelFailureKind::Other,
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
