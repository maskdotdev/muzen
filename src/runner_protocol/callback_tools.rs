use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken as Cancellation;

use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeResult};
use crate::reviewer_kernel::tool_engine::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOutput,
};

use super::transport::RunnerCallbackTransport;
use super::RUNNER_PROTOCOL_VERSION;

pub(crate) struct CallbackReviewTool {
    run_id: String,
    transport: Arc<dyn RunnerCallbackTransport>,
}

impl CallbackReviewTool {
    pub(crate) fn new(run_id: String, transport: Arc<dyn RunnerCallbackTransport>) -> Self {
        Self { run_id, transport }
    }
}

#[async_trait]
impl CustomToolHandler for CallbackReviewTool {
    async fn execute(
        &self,
        context: CustomToolContext,
        arguments: Value,
        _cancel: Cancellation,
    ) -> RuntimeResult<CustomToolOutput> {
        let params = RunnerToolExecuteParams {
            protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
            run_id: self.run_id.clone(),
            session_id: context.session_id.0,
            turn: context.turn_id.0,
            call_id: context.call_id.0,
            tool_id: context.tool_id.as_str().to_string(),
            snapshot_id: context.snapshot_id.0,
            provider_resources: context
                .provider_resources
                .iter()
                .map(|resource| resource.as_str().to_string())
                .collect(),
            arguments,
        };
        let value = self
            .transport
            .request("tool.execute", json!(params))
            .map_err(runtime_error)?;
        let result = serde_json::from_value::<RunnerToolExecuteResult>(value).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid tool.execute result: {error}"))
        })?;
        Ok(CustomToolOutput {
            data: result.data,
            artifact: result.artifact.map(|artifact| CustomToolArtifact {
                key: crate::reviewer_kernel::kernel_types::ArtifactKey(artifact.key),
                content: artifact.content,
            }),
            limits: crate::reviewer_kernel::kernel_types::LimitInfo::default(),
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RunnerToolExecuteParams {
    protocol_version: String,
    run_id: String,
    session_id: String,
    turn: u32,
    call_id: String,
    tool_id: String,
    snapshot_id: String,
    provider_resources: Vec<String>,
    arguments: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunnerToolExecuteResult {
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    artifact: Option<RunnerToolArtifactResult>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunnerToolArtifactResult {
    key: String,
    content: String,
}

fn runtime_error(error: anyhow::Error) -> RuntimeError {
    RuntimeError::InvalidInput(error.to_string())
}
