use std::sync::Arc;

use async_trait::async_trait;

use crate::reviewer_kernel::kernel_types::{
    ArtifactKey, LimitInfo, ProviderResourceId, RuntimeResult, SnapshotId, ToolEffects, ToolId,
    ToolProviderId,
};
use crate::reviewer_kernel::tool_engine::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOptions, CustomToolOutput,
    JsonRpcToolRegistration, JsonRpcToolTransport, ToolRegistry as RuntimeToolRegistry,
};

use crate::reviewer_kernel::spec::*;
use tokio_util::sync::CancellationToken as Cancellation;
pub struct ReviewToolRegistry {
    inner: RuntimeToolRegistry,
}

pub struct ReviewJsonRpcReadOnlyToolRegistration {
    pub provider_id: ToolProviderId,
    pub id: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub cacheable: bool,
    pub provider_resources: Vec<ProviderResourceId>,
    pub transport: Arc<dyn JsonRpcToolTransport>,
}

pub struct ReviewJsonRpcNetworkReadToolRegistration {
    pub provider_id: ToolProviderId,
    pub id: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub cacheable: bool,
    pub provider_resources: Vec<ProviderResourceId>,
    pub transport: Arc<dyn JsonRpcToolTransport>,
}

struct ReviewJsonRpcRuntimeToolRegistration {
    provider_id: ToolProviderId,
    id: String,
    description: String,
    parameters: serde_json::Value,
    cacheable: bool,
    provider_resources: Vec<ProviderResourceId>,
    effects: ToolEffects,
    transport: Arc<dyn JsonRpcToolTransport>,
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

    pub fn register_read_only_tool(
        &mut self,
        id: impl AsRef<str>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        cacheable: bool,
        handler: Arc<dyn ReviewToolHandler>,
    ) -> RuntimeResult<ToolId> {
        let id = ToolId::parse(id.as_ref())?;
        self.inner.register_custom(
            id.clone(),
            description,
            parameters,
            cacheable,
            Arc::new(ReviewToolHandlerAdapter::new(handler)),
        )?;
        Ok(id)
    }

    pub fn register_scoped_read_only_tool(
        &mut self,
        id: impl AsRef<str>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        cacheable: bool,
        provider_resources: Vec<ProviderResourceId>,
        handler: Arc<dyn ReviewToolHandler>,
    ) -> RuntimeResult<ToolId> {
        let id = ToolId::parse(id.as_ref())?;
        self.inner.register_custom_with_alias_and_effects(
            id.clone(),
            id.clone(),
            description,
            parameters,
            CustomToolOptions {
                cacheable,
                effects: ToolEffects::custom_read_only(),
                provider_resources,
            },
            Arc::new(ReviewToolHandlerAdapter::new(handler)),
        )?;
        Ok(id)
    }

    pub fn register_scoped_tool_with_effects(
        &mut self,
        id: impl AsRef<str>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        cacheable: bool,
        provider_resources: Vec<ProviderResourceId>,
        effects: ToolEffects,
        handler: Arc<dyn ReviewToolHandler>,
    ) -> RuntimeResult<ToolId> {
        let id = ToolId::parse(id.as_ref())?;
        self.inner.register_custom_with_alias_and_effects(
            id.clone(),
            id.clone(),
            description,
            parameters,
            CustomToolOptions {
                cacheable,
                effects,
                provider_resources,
            },
            Arc::new(ReviewToolHandlerAdapter::new(handler)),
        )?;
        Ok(id)
    }

    pub fn register_jsonrpc_read_only_tool(
        &mut self,
        provider_id: ToolProviderId,
        id: impl AsRef<str>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        cacheable: bool,
        transport: Arc<dyn JsonRpcToolTransport>,
    ) -> RuntimeResult<ToolId> {
        self.register_scoped_jsonrpc_read_only_tool(ReviewJsonRpcReadOnlyToolRegistration {
            provider_id,
            id: id.as_ref().to_string(),
            description: description.into(),
            parameters,
            cacheable,
            provider_resources: Vec::new(),
            transport,
        })
    }

    pub fn register_scoped_jsonrpc_read_only_tool(
        &mut self,
        registration: ReviewJsonRpcReadOnlyToolRegistration,
    ) -> RuntimeResult<ToolId> {
        self.register_jsonrpc_tool_registration(ReviewJsonRpcRuntimeToolRegistration {
            provider_id: registration.provider_id,
            id: registration.id,
            description: registration.description,
            parameters: registration.parameters,
            cacheable: registration.cacheable,
            provider_resources: registration.provider_resources,
            effects: ToolEffects::review_read_only(),
            transport: registration.transport,
        })
    }

    pub fn register_jsonrpc_network_read_tool(
        &mut self,
        provider_id: ToolProviderId,
        id: impl AsRef<str>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        cacheable: bool,
        transport: Arc<dyn JsonRpcToolTransport>,
    ) -> RuntimeResult<ToolId> {
        self.register_scoped_jsonrpc_network_read_tool(ReviewJsonRpcNetworkReadToolRegistration {
            provider_id,
            id: id.as_ref().to_string(),
            description: description.into(),
            parameters,
            cacheable,
            provider_resources: Vec::new(),
            transport,
        })
    }

    pub fn register_scoped_jsonrpc_network_read_tool(
        &mut self,
        registration: ReviewJsonRpcNetworkReadToolRegistration,
    ) -> RuntimeResult<ToolId> {
        self.register_jsonrpc_tool_registration(ReviewJsonRpcRuntimeToolRegistration {
            provider_id: registration.provider_id,
            id: registration.id,
            description: registration.description,
            parameters: registration.parameters,
            cacheable: registration.cacheable,
            provider_resources: registration.provider_resources,
            effects: provider_network_read_effects(),
            transport: registration.transport,
        })
    }

    fn register_jsonrpc_tool_registration(
        &mut self,
        registration: ReviewJsonRpcRuntimeToolRegistration,
    ) -> RuntimeResult<ToolId> {
        let id = ToolId::parse(registration.id.as_ref())?;
        self.inner
            .register_jsonrpc_tool_with_alias(JsonRpcToolRegistration {
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
