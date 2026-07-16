//! Transport-neutral public contract for Muzen Agent Sessions and Runs.
//!
//! Scheduling, model execution, persistence, and transports stay behind this
//! Module. The value types here are the single wire contract consumed by every
//! Adapter and SDK.

mod client;
mod errors;
pub mod http;
mod ids;
mod local;
mod output_schema;
pub mod runner;
mod runtime_types;
mod store;
mod types;
mod validation;

pub use client::{AgentSession, Artifact, EventStream, Muzen, Run, RuntimeTransport};
pub use errors::{ErrorCode, MuzenError};
pub use http::{HttpServiceConfig, HttpTransport, HttpTransportOptions};
pub use ids::{
    AgentName, ArtifactId, IdempotencyKey, ModelProfileId, RunId, SecretRef, SessionId,
    ToolProviderId,
};
pub use local::{
    CredentialResolver, LocalRuntime, LocalRuntimeConfig, LocalStoreConfig, ModelProvider,
    ModelProviderError, ModelRequest, ModelStop, ModelToolCall, ModelTurn, ResolvedSecret,
};
pub use runtime_types::*;
pub use types::*;

#[cfg(test)]
mod tests;
