use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerFailureKind {
    SourceUnavailable,
    AuthFailed,
    ToolFailed,
    ModelFailed,
    BudgetExhausted,
    Cancelled,
    PolicyDenied,
    InternalError,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerRetryHint {
    Retryable,
    NotRetryable,
    RetryAfter,
    RequiresUserAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunFailedNotification {
    pub error: String,
    pub kind: String,
    pub failure_kind: RunnerFailureKind,
    pub retry_hint: RunnerRetryHint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl RunFailedNotification {
    pub(crate) fn from_runner_error(error: impl Into<String>) -> Self {
        let error = error.into();
        let (failure_kind, retry_hint) = classify_runner_failure(&error);
        Self {
            error,
            kind: "runner_error".to_string(),
            failure_kind,
            retry_hint,
            retry_after_seconds: None,
        }
    }
}

fn classify_runner_failure(message: &str) -> (RunnerFailureKind, RunnerRetryHint) {
    let lower = message.to_ascii_lowercase();
    if lower.contains("cancel") || lower.contains("abort") {
        return (RunnerFailureKind::Cancelled, RunnerRetryHint::NotRetryable);
    }
    if lower.contains("auth")
        || lower.contains("credential")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("permission")
        || lower.contains("access denied")
    {
        return (
            RunnerFailureKind::AuthFailed,
            RunnerRetryHint::RequiresUserAction,
        );
    }
    if lower.contains("budget") || lower.contains("limit exceeded") {
        return (
            RunnerFailureKind::BudgetExhausted,
            RunnerRetryHint::NotRetryable,
        );
    }
    if lower.contains("policy") || lower.contains("not allowed") || lower.contains("denied") {
        return (
            RunnerFailureKind::PolicyDenied,
            RunnerRetryHint::NotRetryable,
        );
    }
    if lower.contains("source.materialize")
        || lower.contains("sourceprovider")
        || lower.contains("materializ")
        || lower.contains("repository unavailable")
        || lower.contains("repo unavailable")
    {
        let retry_hint = if lower.contains("requires") || lower.contains("invalid") {
            RunnerRetryHint::RequiresUserAction
        } else {
            RunnerRetryHint::Retryable
        };
        return (RunnerFailureKind::SourceUnavailable, retry_hint);
    }
    if lower.contains("model.complete") || lower.contains("model") {
        return (RunnerFailureKind::ModelFailed, RunnerRetryHint::Retryable);
    }
    if lower.contains("tool.execute") || lower.contains("tool") {
        return (RunnerFailureKind::ToolFailed, RunnerRetryHint::Retryable);
    }
    (RunnerFailureKind::InternalError, RunnerRetryHint::Retryable)
}
