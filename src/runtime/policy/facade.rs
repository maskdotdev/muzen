use crate::runtime::contracts::RuntimeError;

#[derive(Debug, Clone, Default)]
pub struct ReviewerPolicy;

impl ReviewerPolicy {
    pub fn new() -> Self {
        Self
    }

    pub fn should_retry_model_error(&self, error: &RuntimeError) -> bool {
        match error {
            RuntimeError::Provider { retryable, .. } => *retryable,
            RuntimeError::ProviderMessage { retryable, .. } => *retryable,
            RuntimeError::Timeout => true,
            RuntimeError::Cancelled => false,
            _ => false,
        }
    }

    pub(crate) fn should_cancel_job_after_model_error(&self, error: &RuntimeError) -> bool {
        matches!(
            error,
            RuntimeError::Provider {
                retryable: false,
                ..
            } | RuntimeError::ProviderMessage {
                retryable: false,
                ..
            }
        )
    }
}
