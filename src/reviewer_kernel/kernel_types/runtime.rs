use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("resource limit exceeded: {kind}")]
    LimitExceeded { kind: &'static str },
    #[error("operation timed out")]
    Timeout,
    #[error("operation cancelled")]
    Cancelled,
    #[error("provider error: {status:?}")]
    Provider {
        status: Option<u16>,
        retryable: bool,
    },
    #[error("provider error: {status:?}: {message}")]
    ProviderMessage {
        status: Option<u16>,
        retryable: bool,
        message: String,
    },
    #[error("repository access denied")]
    RepoAccessDenied,
    #[error("repository unavailable: {0}")]
    RepoUnavailable(String),
    #[error("snapshot content changed after capture: {path}")]
    SnapshotStale { path: String },
    #[error("internal invariant violation: {0}")]
    Invariant(&'static str),
}

pub type RuntimeResult<T> = Result<T, RuntimeError>;
