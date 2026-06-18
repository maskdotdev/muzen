use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::kernel_types::{
    ArtifactKey, LimitInfo, ProviderResourceId, RuntimeError, RuntimeResult, SessionId, SnapshotId,
    ToolCallId, ToolEffects, ToolId, ToolProviderId, TurnId,
};
use crate::reviewer_kernel::review_contract::ToolName;

use super::catalog::{review_builtin_specs, BuiltinToolSpec};

#[derive(Clone)]
pub(crate) struct ToolRegistry {
    definitions: HashMap<ToolId, ToolDefinition>,
    aliases: HashMap<ToolId, ToolId>,
    jsonrpc_transports: HashMap<ToolProviderId, Arc<dyn JsonRpcToolTransport>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("definitions", &self.definitions.len())
            .field("aliases", &self.aliases.len())
            .field("jsonrpc_transports", &self.jsonrpc_transports.len())
            .finish()
    }
}

impl ToolRegistry {
    pub(crate) fn review_defaults() -> RuntimeResult<Self> {
        let mut registry = Self {
            definitions: HashMap::new(),
            aliases: HashMap::new(),
            jsonrpc_transports: HashMap::new(),
        };
        for spec in review_builtin_specs() {
            registry.register_builtin(spec)?;
        }
        Ok(registry)
    }

    pub(crate) fn register_custom_with_effects(
        &mut self,
        id: ToolId,
        description: impl Into<String>,
        parameters: Value,
        cacheable: bool,
        effects: ToolEffects,
        handler: Arc<dyn CustomToolHandler>,
    ) -> RuntimeResult<()> {
        self.register_custom_with_options(
            id,
            description,
            parameters,
            CustomToolOptions {
                cacheable,
                effects,
                provider_resources: Vec::new(),
            },
            handler,
        )
    }

    pub(crate) fn register_custom_with_options(
        &mut self,
        id: ToolId,
        description: impl Into<String>,
        parameters: Value,
        options: CustomToolOptions,
        handler: Arc<dyn CustomToolHandler>,
    ) -> RuntimeResult<()> {
        self.register_custom_with_model_alias(
            id.clone(),
            id,
            description,
            parameters,
            options,
            handler,
        )
    }

    #[cfg(test)]
    pub(crate) fn register_custom_with_alias_for_test(
        &mut self,
        id: ToolId,
        model_alias: ToolId,
        description: impl Into<String>,
        parameters: Value,
        options: CustomToolOptions,
        handler: Arc<dyn CustomToolHandler>,
    ) -> RuntimeResult<()> {
        self.register_custom_with_model_alias(
            id,
            model_alias,
            description,
            parameters,
            options,
            handler,
        )
    }

    fn register_custom_with_model_alias(
        &mut self,
        id: ToolId,
        model_alias: ToolId,
        description: impl Into<String>,
        parameters: Value,
        options: CustomToolOptions,
        handler: Arc<dyn CustomToolHandler>,
    ) -> RuntimeResult<()> {
        self.register(ToolDefinition {
            id,
            model_alias,
            description: description.into(),
            parameters: validate_parameters(parameters)?,
            builtin: None,
            cacheable: options.cacheable,
            effects: options.effects,
            provider_resources: options.provider_resources,
            provider_id: ToolProviderId::in_process(),
            handler: Some(handler),
        })
    }

    #[cfg(test)]
    pub(crate) fn register_jsonrpc_tool_with_alias(
        &mut self,
        registration: JsonRpcToolRegistration,
    ) -> RuntimeResult<()> {
        let JsonRpcToolRegistration {
            provider_id,
            id,
            model_alias,
            description,
            parameters,
            options,
            transport,
        } = registration;
        if provider_id == ToolProviderId::builtin_review()
            || provider_id == ToolProviderId::in_process()
            || provider_id == ToolProviderId::runtime()
        {
            return Err(RuntimeError::InvalidInput(
                "reserved tool provider id".to_string(),
            ));
        }
        self.jsonrpc_transports
            .entry(provider_id.clone())
            .or_insert_with(|| transport);
        self.register(ToolDefinition {
            id,
            model_alias,
            description,
            parameters: validate_parameters(parameters)?,
            builtin: None,
            cacheable: options.cacheable,
            effects: options.effects,
            provider_resources: options.provider_resources,
            provider_id,
            handler: None,
        })
    }

    pub(crate) fn definition(&self, id: &ToolId) -> Option<&ToolDefinition> {
        self.definitions.get(id)
    }

    pub(crate) fn model_alias_for_tool(&self, id: &ToolId) -> Option<&ToolId> {
        self.definitions
            .get(id)
            .map(|definition| &definition.model_alias)
    }

    pub(crate) fn tool_id_for_model_alias(&self, alias: &ToolId) -> Option<ToolId> {
        self.aliases.get(alias).cloned()
    }

    pub(crate) fn jsonrpc_transports(
        &self,
    ) -> Vec<(ToolProviderId, Arc<dyn JsonRpcToolTransport>)> {
        self.jsonrpc_transports
            .iter()
            .map(|(provider_id, transport)| (provider_id.clone(), Arc::clone(transport)))
            .collect()
    }

    fn register_builtin(&mut self, spec: BuiltinToolSpec) -> RuntimeResult<()> {
        self.register(ToolDefinition {
            id: ToolId::from(spec.name),
            model_alias: spec.model_alias()?,
            description: spec.description.to_string(),
            parameters: validate_parameters(spec.parameters())?,
            builtin: Some(spec.name),
            cacheable: spec.cacheable,
            effects: ToolEffects::review_read_only(),
            provider_resources: Vec::new(),
            provider_id: ToolProviderId::builtin_review(),
            handler: None,
        })
    }

    fn register(&mut self, definition: ToolDefinition) -> RuntimeResult<()> {
        if self.definitions.contains_key(&definition.id) {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate tool id {}",
                definition.id.as_str()
            )));
        }
        if self.aliases.contains_key(&definition.model_alias) {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate tool alias {}",
                definition.model_alias.as_str()
            )));
        }
        self.aliases
            .insert(definition.model_alias.clone(), definition.id.clone());
        self.definitions.insert(definition.id.clone(), definition);
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ToolDefinition {
    pub(crate) id: ToolId,
    pub(crate) model_alias: ToolId,
    pub(crate) description: String,
    pub(crate) parameters: Value,
    pub(crate) builtin: Option<ToolName>,
    pub(crate) cacheable: bool,
    pub(crate) effects: ToolEffects,
    pub(crate) provider_resources: Vec<ProviderResourceId>,
    pub(crate) provider_id: ToolProviderId,
    pub(crate) handler: Option<Arc<dyn CustomToolHandler>>,
}

impl fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("id", &self.id)
            .field("model_alias", &self.model_alias)
            .field("description", &self.description)
            .field("builtin", &self.builtin)
            .field("cacheable", &self.cacheable)
            .field("effects", &self.effects)
            .field("provider_resources", &self.provider_resources)
            .field("provider_id", &self.provider_id)
            .field("has_handler", &self.handler.is_some())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CustomToolOptions {
    pub(crate) cacheable: bool,
    pub(crate) effects: ToolEffects,
    pub(crate) provider_resources: Vec<ProviderResourceId>,
}

impl Default for CustomToolOptions {
    fn default() -> Self {
        Self {
            cacheable: false,
            effects: ToolEffects::custom_read_only(),
            provider_resources: Vec::new(),
        }
    }
}

#[async_trait]
pub(crate) trait CustomToolHandler: Send + Sync {
    async fn execute(
        &self,
        context: CustomToolContext,
        args: Value,
        cancel: CancellationToken,
    ) -> RuntimeResult<CustomToolOutput>;
}

#[async_trait]
pub(crate) trait JsonRpcToolTransport: Send + Sync {
    async fn call(
        &self,
        request: JsonRpcToolRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<JsonRpcToolResponse>;
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct JsonRpcToolRegistration {
    pub(crate) provider_id: ToolProviderId,
    pub(crate) id: ToolId,
    pub(crate) model_alias: ToolId,
    pub(crate) description: String,
    pub(crate) parameters: Value,
    pub(crate) options: CustomToolOptions,
    pub(crate) transport: Arc<dyn JsonRpcToolTransport>,
}

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsonRpcToolRequest {
    pub(crate) provider_id: ToolProviderId,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) call_id: ToolCallId,
    pub(crate) tool_id: ToolId,
    pub(crate) snapshot_id: SnapshotId,
    #[serde(default)]
    pub(crate) provider_resources: Vec<ProviderResourceId>,
    pub(crate) arguments: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct CustomToolContext {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) call_id: ToolCallId,
    pub(crate) tool_id: ToolId,
    pub(crate) snapshot_id: SnapshotId,
    pub(crate) provider_resources: Vec<ProviderResourceId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsonRpcToolResponse {
    pub(crate) data: Option<Value>,
    pub(crate) artifact: Option<CustomToolArtifact>,
    pub(crate) limits: LimitInfo,
}

pub(crate) type CustomToolOutput = JsonRpcToolResponse;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CustomToolArtifact {
    pub(crate) key: ArtifactKey,
    pub(crate) content: String,
}

fn validate_parameters(parameters: Value) -> RuntimeResult<Value> {
    let object = parameters.as_object().ok_or_else(|| {
        RuntimeError::InvalidInput("tool parameters must be an object".to_string())
    })?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(RuntimeError::InvalidInput(
            "tool parameters must use type=object".to_string(),
        ));
    }
    if !object.contains_key("properties") {
        return Err(RuntimeError::InvalidInput(
            "tool parameters must declare properties".to_string(),
        ));
    }
    Ok(parameters)
}
