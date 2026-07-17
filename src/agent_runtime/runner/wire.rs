//! Shared newline-delimited JSON-RPC 2.0 wire contract for the agent runner.
//!
//! Parameter envelopes are deliberately shallow and camel-cased:
//! `session.create` uses `{ spec, options? }`; session reads use
//! `{ sessionId, ... }`; run methods use `{ runId, ... }`; `run.events` adds
//! `{ after?, subscriptionId }`; and mutating commands carry their existing
//! options or command value without duplicating idempotency keys. Secret input
//! is sent directly for `secret.put`, while `secret.delete` uses `{ secret }`.
//! `artifact.read` follows the public ranged-read shape exactly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::{
    AnswerToolCallInput, ArtifactId, CancelOptions, CommandOptions, CreateOptions, IdempotencyKey,
    MessagePage, PutSecretInput, RunId, RunSpec, SecretRef, SendCommand, SessionId, SessionSpec,
    SpawnCommand,
};

pub(crate) const JSONRPC_VERSION: &str = "2.0";
pub(crate) const MUZEN_ERROR_CODE: i64 = -32000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OutboundRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub(crate) fn protocol(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyParams {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SecretDeleteParams {
    pub secret: SecretRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionCreateParams {
    pub spec: SessionSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<CreateOptions>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionParams {
    pub session_id: SessionId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionMessagesParams {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<MessagePage>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionArchiveParams {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<CommandOptions>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunStartParams {
    pub spec: RunSpec,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunParams {
    pub run_id: RunId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunEventsParams {
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    pub subscription_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunEventsResult {
    pub events: Vec<super::super::AgentEvent>,
    pub subscribed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunEventParams {
    pub subscription_id: String,
    pub run_id: RunId,
    pub event: super::super::AgentEvent,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UnsubscribeParams {
    pub subscription_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunSendParams {
    pub run_id: RunId,
    pub command: SendCommand,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunSpawnParams {
    pub run_id: RunId,
    pub command: SpawnCommand,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunCancelParams {
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<CancelOptions>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunAnswerToolCallParams {
    pub run_id: RunId,
    pub input: AnswerToolCallInput,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ArtifactReadParams {
    pub artifact_id: ArtifactId,
    pub offset: u64,
    pub max_bytes: u32,
}

pub(crate) fn put_secret_params(input: PutSecretInput) -> Value {
    serde_json::to_value(input).expect("secret input serializes")
}
