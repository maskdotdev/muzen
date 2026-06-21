use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::*;
use crate::reviewer_kernel::kernel_types::{
    CapabilitySet, RuntimeEvent, RuntimeEventSink, SessionId,
};
use crate::reviewer_kernel::model::ModelLimiter;
use crate::reviewer_kernel::review_contract::{AgentBudget, Role};

struct FlakyModel {
    calls: AtomicUsize,
    failures_before_success: usize,
    error: fn() -> RuntimeError,
}

struct LimiterBackedModel {
    limiter: Arc<ModelLimiter>,
}

struct SlowModel {
    delay: std::time::Duration,
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

#[async_trait]
impl ConcurrentModelClient for LimiterBackedModel {
    async fn complete(
        &self,
        scope: &SessionScope,
        _transcript: &[ConversationItem],
        _turn_id: TurnId,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ModelTurn> {
        let _permit = self
            .limiter
            .acquire_for_model("provider", "profile", "credential", &scope.id)
            .await?;
        Ok(ModelTurn::Text {
            content: "ok".to_string(),
            usage: Default::default(),
        })
    }
}

#[async_trait]
impl ConcurrentModelClient for SlowModel {
    async fn complete(
        &self,
        _scope: &SessionScope,
        _transcript: &[ConversationItem],
        _turn_id: TurnId,
        _cancel: CancellationToken,
    ) -> RuntimeResult<ModelTurn> {
        tokio::time::sleep(self.delay).await;
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
        response_format: None,
        capabilities: CapabilitySet::review_read_only(),
        budget: AgentBudget {
            max_turns: 1,
            max_tool_calls: 1,
            max_prompt_tokens: 1_024,
            max_output_tokens: 64,
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::PlannedDefault,
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
    let events = RuntimeEventDispatcher::new(Some(sink));
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
    assert_eq!(outcome.diagnostics.retryable_provider_errors, 2);
    assert_eq!(outcome.diagnostics.non_retryable_provider_errors, 0);
    assert_eq!(outcome.diagnostics.terminal_failure_kind, None);
    assert!(outcome.diagnostics.retry_backoff_ms >= 1);
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
    assert_eq!(outcome.diagnostics.retryable_provider_errors, 3);
    assert_eq!(
        outcome.diagnostics.terminal_failure_kind,
        Some(ModelFailureKind::ProviderRetryable)
    );
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
    assert_eq!(outcome.diagnostics.non_retryable_provider_errors, 1);
    assert_eq!(
        outcome.diagnostics.terminal_failure_kind,
        Some(ModelFailureKind::ProviderNonRetryable)
    );
    assert_eq!(model_failed_flags(&sink), vec![(1, false)]);
}

#[tokio::test]
async fn records_provider_request_duration_from_mock_client() {
    let scope = test_scope();
    let model = SlowModel {
        delay: std::time::Duration::from_millis(10),
    };
    let events = RuntimeEventDispatcher::new(None);
    let mut limits = fast_retry_limits(1);
    limits.max_model_turn_ms = 500;

    let outcome = complete_model_turn(
        &model,
        &ReviewerPolicy::new(),
        &events,
        &limits,
        &scope,
        &scope,
        &[],
        TurnId(0),
        &CancellationToken::new(),
    )
    .await;

    assert!(outcome.result.is_ok());
    assert_eq!(outcome.attempts, 1);
    assert!(
        outcome.diagnostics.provider_request_ms >= 1,
        "expected provider request duration, got {:?}",
        outcome.diagnostics
    );
    assert_eq!(outcome.diagnostics.limiter_wait.total_wait_ms, 0);
}

#[tokio::test]
async fn records_successful_model_limiter_waits() {
    let scope = test_scope();
    let limiter = Arc::new(ModelLimiter::new_with_buckets(1, 1, 1, 1, 1));
    let blocking_permit = limiter
        .acquire_for_model("provider", "profile", "credential", &scope.id)
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        drop(blocking_permit);
    });
    let model = LimiterBackedModel {
        limiter: Arc::clone(&limiter),
    };
    let events = RuntimeEventDispatcher::new(None);
    let mut limits = fast_retry_limits(1);
    limits.max_model_turn_ms = 500;

    let outcome = complete_model_turn(
        &model,
        &ReviewerPolicy::new(),
        &events,
        &limits,
        &scope,
        &scope,
        &[],
        TurnId(0),
        &CancellationToken::new(),
    )
    .await;
    release.await.unwrap();

    assert!(outcome.result.is_ok());
    assert_eq!(outcome.attempts, 1);
    assert!(
        outcome.diagnostics.limiter_wait.total_wait_ms >= 1,
        "expected limiter wait to be recorded, got {:?}",
        outcome.diagnostics.limiter_wait
    );
    assert!(
        outcome.diagnostics.limiter_wait.max_bucket_wait_ms >= 1,
        "expected max limiter bucket wait to be recorded, got {:?}",
        outcome.diagnostics.limiter_wait
    );
    assert!(
        outcome.diagnostics.limiter_wait.global_wait_ms >= 1,
        "expected global limiter wait to be recorded, got {:?}",
        outcome.diagnostics.limiter_wait
    );
}

#[tokio::test]
async fn limiter_queue_wait_counts_against_model_turn_timeout() {
    let scope = test_scope();
    let limiter = Arc::new(ModelLimiter::new_with_buckets(1, 1, 1, 1, 1));
    let blocking_permit = limiter
        .acquire_for_model("provider", "profile", "credential", &scope.id)
        .await
        .unwrap();
    let model = LimiterBackedModel {
        limiter: Arc::clone(&limiter),
    };
    let sink = Arc::new(CaptureSink::default());
    let events = RuntimeEventDispatcher::new(Some(sink.clone()));
    let mut limits = fast_retry_limits(1);
    limits.max_model_turn_ms = 5;

    let outcome = complete_model_turn(
        &model,
        &ReviewerPolicy::new(),
        &events,
        &limits,
        &scope,
        &scope,
        &[],
        TurnId(0),
        &CancellationToken::new(),
    )
    .await;
    drop(blocking_permit);

    assert!(matches!(outcome.result, Err(RuntimeError::Timeout)));
    assert_eq!(outcome.attempts, 1);
    assert_eq!(outcome.diagnostics.timeout_errors, 1);
    assert_eq!(
        outcome.diagnostics.terminal_failure_kind,
        Some(ModelFailureKind::Timeout)
    );
    assert_eq!(model_failed_flags(&sink), vec![(1, false)]);
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
