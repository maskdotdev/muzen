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

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub(crate) fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ResourceExhausted, message)
    }

    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            details: None,
        }
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
