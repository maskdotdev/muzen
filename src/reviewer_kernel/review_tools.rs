use std::sync::Arc;

use async_trait::async_trait;

use crate::reviewer_kernel::kernel_types::{
    ArtifactKey, LimitInfo, ProviderResourceId, RuntimeResult, SnapshotId, ToolEffects, ToolId,
    ToolProviderId,
};
use crate::reviewer_kernel::tool_engine::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOptions, CustomToolOutput,
    JsonRpcToolRegistration as RuntimeJsonRpcToolRegistration, JsonRpcToolTransport,
    ToolRegistry as RuntimeToolRegistry,
};

use tokio_util::sync::CancellationToken as Cancellation;
pub struct ReviewToolRegistry {
    inner: RuntimeToolRegistry,
}

pub struct ReviewToolRegistration {
    pub id: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub cacheable: bool,
    pub provider_resources: Vec<ProviderResourceId>,
    pub effects: ToolEffects,
    pub handler: Arc<dyn ReviewToolHandler>,
}

pub struct ReviewJsonRpcToolRegistration {
    pub provider_id: ToolProviderId,
    pub id: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub cacheable: bool,
    pub provider_resources: Vec<ProviderResourceId>,
    pub effects: ToolEffects,
    pub transport: Arc<dyn JsonRpcToolTransport>,
}

impl std::fmt::Debug for ReviewToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewToolRegistry").finish_non_exhaustive()
    }
}

impl ReviewToolRegistry {
    pub fn review_defaults() -> RuntimeResult<Self> {
        Ok(Self {
            inner: RuntimeToolRegistry::review_defaults()?,
        })
    }

    pub fn register_tool(&mut self, registration: ReviewToolRegistration) -> RuntimeResult<ToolId> {
        let id = ToolId::parse(registration.id.as_ref())?;
        self.inner.register_custom_with_alias_and_effects(
            id.clone(),
            id.clone(),
            registration.description,
            registration.parameters,
            CustomToolOptions {
                cacheable: registration.cacheable,
                effects: registration.effects,
                provider_resources: registration.provider_resources,
            },
            Arc::new(ReviewToolHandlerAdapter::new(registration.handler)),
        )?;
        Ok(id)
    }

    pub fn register_jsonrpc_tool(
        &mut self,
        registration: ReviewJsonRpcToolRegistration,
    ) -> RuntimeResult<ToolId> {
        let id = ToolId::parse(registration.id.as_ref())?;
        self.inner
            .register_jsonrpc_tool_with_alias(RuntimeJsonRpcToolRegistration {
                provider_id: registration.provider_id,
                id: id.clone(),
                model_alias: id.clone(),
                description: registration.description,
                parameters: registration.parameters,
                options: CustomToolOptions {
                    cacheable: registration.cacheable,
                    effects: registration.effects,
                    provider_resources: registration.provider_resources,
                },
                transport: registration.transport,
            })?;
        Ok(id)
    }

    pub(crate) fn into_tool_registry(self) -> RuntimeToolRegistry {
        self.inner
    }
}

#[async_trait]
pub trait ReviewToolHandler: Send + Sync {
    async fn execute_review_tool(
        &self,
        context: ReviewToolContext,
        arguments: serde_json::Value,
        cancel: Cancellation,
    ) -> RuntimeResult<ReviewToolOutput>;
}

#[derive(Debug, Clone)]
pub struct ReviewToolContext {
    pub session_id: String,
    pub turn: u32,
    pub call_id: String,
    pub tool_id: String,
    pub snapshot_id: SnapshotId,
    pub provider_resources: Vec<ProviderResourceId>,
}

impl ReviewToolContext {
    fn from_custom_tool_context(context: CustomToolContext) -> Self {
        Self {
            session_id: context.session_id.0,
            turn: context.turn_id.0,
            call_id: context.call_id.0,
            tool_id: context.tool_id.as_str().to_string(),
            snapshot_id: context.snapshot_id,
            provider_resources: context.provider_resources,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReviewToolOutput {
    pub data: Option<serde_json::Value>,
    pub artifact: Option<ReviewToolArtifact>,
}

#[derive(Debug, Clone)]
pub struct ReviewToolArtifact {
    pub key: String,
    pub content: String,
}

struct ReviewToolHandlerAdapter {
    handler: Arc<dyn ReviewToolHandler>,
}

impl ReviewToolHandlerAdapter {
    fn new(handler: Arc<dyn ReviewToolHandler>) -> Self {
        Self { handler }
    }
}

#[async_trait]
impl CustomToolHandler for ReviewToolHandlerAdapter {
    async fn execute(
        &self,
        context: CustomToolContext,
        args: serde_json::Value,
        cancel: Cancellation,
    ) -> RuntimeResult<CustomToolOutput> {
        let output = self
            .handler
            .execute_review_tool(
                ReviewToolContext::from_custom_tool_context(context),
                args,
                cancel,
            )
            .await?;
        Ok(CustomToolOutput {
            data: output.data,
            artifact: output.artifact.map(|artifact| CustomToolArtifact {
                key: ArtifactKey(artifact.key),
                content: artifact.content,
            }),
            limits: LimitInfo::default(),
        })
    }
}
