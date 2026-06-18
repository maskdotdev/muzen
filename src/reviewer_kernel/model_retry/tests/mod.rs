use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::*;
use crate::reviewer_kernel::kernel_types::{
    CapabilitySet, RuntimeEvent, RuntimeEventSink, SessionId,
};
use crate::reviewer_kernel::review_contract::{AgentBudget, Role};

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
