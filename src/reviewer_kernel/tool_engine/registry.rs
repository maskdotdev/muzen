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
pub struct ToolRegistry {
    definitions: HashMap<ToolId, ToolDefinition>,
    jsonrpc_transports: HashMap<ToolProviderId, Arc<dyn JsonRpcToolTransport>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("definitions", &self.definitions.len())
            .field("jsonrpc_transports", &self.jsonrpc_transports.len())
            .finish()
    }
}

impl ToolRegistry {
    pub fn review_defaults() -> RuntimeResult<Self> {
        let mut registry = Self {
            definitions: HashMap::new(),
            jsonrpc_transports: HashMap::new(),
        };
        for spec in review_builtin_specs() {
            registry.register_builtin(spec)?;
        }
        Ok(registry)
    }

    pub fn register_custom_with_effects(
        &mut self,
        id: ToolId,
        description: impl Into<String>,
        parameters: Value,
        cacheable: bool,
        effects: ToolEffects,
        handler: Arc<dyn CustomToolHandler>,
    ) -> RuntimeResult<()> {
        self.register_custom_with_alias_and_effects(
            id.clone(),
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

    pub fn register_custom_with_alias_and_effects(
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

    pub fn definition(&self, id: &ToolId) -> Option<&ToolDefinition> {
        self.definitions.get(id)
    }

    pub fn alias_table(&self) -> RuntimeResult<ToolAliasTable> {
        ToolAliasTable::from_registry(self)
    }

    pub(crate) fn tool_id_for_model_alias(&self, alias: &ToolId) -> Option<ToolId> {
        self.definitions
            .values()
            .find(|definition| &definition.model_alias == alias)
            .map(|definition| definition.id.clone())
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
        if self
            .definitions
            .values()
            .any(|existing| existing.model_alias == definition.model_alias)
        {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate tool alias {}",
                definition.model_alias.as_str()
            )));
        }
        self.definitions.insert(definition.id.clone(), definition);
        Ok(())
    }
}

#[derive(Clone)]
pub struct ToolDefinition {
    pub id: ToolId,
    pub model_alias: ToolId,
    pub description: String,
    pub parameters: Value,
    pub(crate) builtin: Option<ToolName>,
    pub cacheable: bool,
    pub effects: ToolEffects,
    pub provider_resources: Vec<ProviderResourceId>,
    pub provider_id: ToolProviderId,
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
pub struct CustomToolOptions {
    pub cacheable: bool,
    pub effects: ToolEffects,
    pub provider_resources: Vec<ProviderResourceId>,
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

#[derive(Debug, Clone)]
pub struct ToolAliasTable {
    by_tool: HashMap<ToolId, ToolId>,
}

impl ToolAliasTable {
    pub fn from_registry(registry: &ToolRegistry) -> RuntimeResult<Self> {
        let mut by_tool = HashMap::new();
        let mut aliases = std::collections::HashSet::new();
        for definition in registry.definitions.values() {
            if !aliases.insert(definition.model_alias.clone()) {
                return Err(RuntimeError::InvalidInput(format!(
                    "duplicate tool alias {}",
                    definition.model_alias.as_str()
                )));
            }
            by_tool.insert(definition.id.clone(), definition.model_alias.clone());
        }
        Ok(Self { by_tool })
    }

    pub fn alias_for(&self, tool_id: &ToolId) -> Option<&ToolId> {
        self.by_tool.get(tool_id)
    }
}

#[async_trait]
pub trait CustomToolHandler: Send + Sync {
    async fn execute(
        &self,
        context: CustomToolContext,
        args: Value,
        cancel: CancellationToken,
    ) -> RuntimeResult<CustomToolOutput>;
}

#[async_trait]
pub trait JsonRpcToolTransport: Send + Sync {
    async fn call(
        &self,
        request: JsonRpcToolRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<JsonRpcToolResponse>;
}

#[cfg(test)]
#[derive(Clone)]
pub struct JsonRpcToolRegistration {
    pub provider_id: ToolProviderId,
    pub id: ToolId,
    pub model_alias: ToolId,
    pub description: String,
    pub parameters: Value,
    pub options: CustomToolOptions,
    pub transport: Arc<dyn JsonRpcToolTransport>,
}

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRpcToolRequest {
    pub provider_id: ToolProviderId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub tool_id: ToolId,
    pub snapshot_id: SnapshotId,
    #[serde(default)]
    pub provider_resources: Vec<ProviderResourceId>,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct CustomToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub tool_id: ToolId,
    pub snapshot_id: SnapshotId,
    pub provider_resources: Vec<ProviderResourceId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRpcToolResponse {
    pub data: Option<Value>,
    pub artifact: Option<CustomToolArtifact>,
    pub limits: LimitInfo,
}

pub type CustomToolOutput = JsonRpcToolResponse;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomToolArtifact {
    pub key: ArtifactKey,
    pub content: String,
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
