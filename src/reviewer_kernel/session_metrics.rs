use std::time::Instant;

use crate::reviewer_kernel::kernel_types::ModelMetricsSnapshot;
use crate::reviewer_kernel::model::ConcurrentModelClient;
use crate::reviewer_kernel::review_contract::TokenUsage;

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_micros().div_ceil(1000) as u64
}

pub(crate) fn record_usage(
    tokens: &mut TokenUsage,
    model_metrics: &mut ModelMetricsSnapshot,
    model: &dyn ConcurrentModelClient,
    usage: TokenUsage,
) {
    tokens.add(usage);
    model_metrics.input_tokens += usage.input_tokens;
    model_metrics.output_tokens += usage.output_tokens;
    model_metrics.total_tokens += usage.total_tokens;
    model_metrics.cached_input_tokens += usage.cached_input_tokens;
    if let Some(cost) = model.estimate_cost(&usage) {
        model_metrics.costed_calls += 1;
        model_metrics.estimated_input_cost_micro_usd += cost.input_cost_micro_usd;
        model_metrics.estimated_output_cost_micro_usd += cost.output_cost_micro_usd;
        model_metrics.estimated_total_cost_micro_usd += cost.total_cost_micro_usd;
    } else {
        model_metrics.unpriced_calls += 1;
    }
}

pub(crate) fn add_model_metrics(target: &mut ModelMetricsSnapshot, report: &ModelMetricsSnapshot) {
    target.calls += report.calls;
    target.successes += report.successes;
    target.errors += report.errors;
    target.retries += report.retries;
    target.timeout_errors += report.timeout_errors;
    target.cancellation_errors += report.cancellation_errors;
    target.retryable_provider_errors += report.retryable_provider_errors;
    target.non_retryable_provider_errors += report.non_retryable_provider_errors;
    target.other_errors += report.other_errors;
    target.terminal_timeout_failures += report.terminal_timeout_failures;
    target.terminal_cancelled_failures += report.terminal_cancelled_failures;
    target.terminal_retryable_provider_failures += report.terminal_retryable_provider_failures;
    target.terminal_non_retryable_provider_failures +=
        report.terminal_non_retryable_provider_failures;
    target.terminal_other_failures += report.terminal_other_failures;
    target.costed_calls += report.costed_calls;
    target.unpriced_calls += report.unpriced_calls;
    target.latency_ms += report.latency_ms;
    target.max_latency_ms = target.max_latency_ms.max(report.max_latency_ms);
    target.provider_request_ms += report.provider_request_ms;
    target.max_provider_request_ms = target
        .max_provider_request_ms
        .max(report.max_provider_request_ms);
    target.retry_backoff_ms += report.retry_backoff_ms;
    target.max_retry_backoff_ms = target.max_retry_backoff_ms.max(report.max_retry_backoff_ms);
    target.limiter_wait_ms += report.limiter_wait_ms;
    target.max_limiter_wait_ms = target.max_limiter_wait_ms.max(report.max_limiter_wait_ms);
    target.limiter_global_wait_ms += report.limiter_global_wait_ms;
    target.limiter_provider_wait_ms += report.limiter_provider_wait_ms;
    target.limiter_profile_wait_ms += report.limiter_profile_wait_ms;
    target.limiter_key_wait_ms += report.limiter_key_wait_ms;
    target.limiter_session_wait_ms += report.limiter_session_wait_ms;
    target.estimated_input_cost_micro_usd += report.estimated_input_cost_micro_usd;
    target.estimated_output_cost_micro_usd += report.estimated_output_cost_micro_usd;
    target.estimated_total_cost_micro_usd += report.estimated_total_cost_micro_usd;
    target.input_tokens += report.input_tokens;
    target.output_tokens += report.output_tokens;
    target.total_tokens += report.total_tokens;
    target.cached_input_tokens += report.cached_input_tokens;
}
