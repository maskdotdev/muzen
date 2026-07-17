use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::validation::SpecValidationError;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    NotFound,
    Conflict,
    Unauthenticated,
    PermissionDenied,
    ResourceExhausted,
    Unsupported,
    Unavailable,
    DeadlineExceeded,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error, PartialEq)]
#[error("{message}")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MuzenError {
    code: ErrorCode,
    message: String,
    retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

impl MuzenError {
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }

    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    pub(crate) fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub(crate) fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PermissionDenied, message)
    }

    pub(crate) fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthenticated, message)
    }

    pub(crate) fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ResourceExhausted, message)
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message)
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        let mut error = Self::new(ErrorCode::Unavailable, message);
        error.retryable = true;
        error
    }

    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    pub(crate) fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub(crate) fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl From<SpecValidationError> for MuzenError {
    fn from(error: SpecValidationError) -> Self {
        Self {
            code: ErrorCode::InvalidInput,
            message: error.message,
            retryable: false,
            details: Some(serde_json::json!({ "path": error.path })),
        }
    }
}
