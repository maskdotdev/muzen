use serde::{Deserialize, Serialize};

use crate::reviewer_kernel::kernel_types::{
    CapabilitySet, ModelResponseFormat, ProviderResourceId, ProviderResourceScope, RuntimeLimits,
    SessionId, SessionInstruction, SessionScope, SnapshotId, ToolEffects, ToolGrant, ToolId,
    ToolProviderId,
};
use crate::reviewer_kernel::review_contract::{AgentBudget, Role};

use crate::reviewer_kernel::adapters::{capabilities, runtime};
use crate::reviewer_kernel::snapshots::*;
pub struct RunSpec {
    pub run_id: String,
    pub snapshots: Vec<SnapshotSpec>,
    pub sessions: Vec<ReviewSessionSpec>,
    pub limits: ReviewRunLimits,
    pub mode: RunMode,
}

/// How a run turns its sessions into work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Run the autonomous review orchestrator for the snapshot diff.
    #[default]
    Review,
    /// Run each supplied session directly through the generic agent loop and
    /// return its final text output. No review planning or findings.
    DirectSessions,
}

impl RunSpec {
    pub fn single_snapshot(
        run_id: impl Into<String>,
        snapshot: SnapshotSpec,
        sessions: Vec<ReviewSessionSpec>,
        limits: impl Into<ReviewRunLimits>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            snapshots: vec![snapshot],
            sessions,
            limits: limits.into(),
            mode: RunMode::default(),
        }
    }

    pub fn with_mode(mut self, mode: RunMode) -> Self {
        self.mode = mode;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRunLimits {
    limits: RuntimeLimits,
}

impl ReviewRunLimits {
    pub fn standard(sessions: usize, max_file_bytes: usize, max_search_matches: usize) -> Self {
        Self {
            limits: RuntimeLimits::standard(sessions, max_file_bytes, max_search_matches),
        }
    }

    pub fn from_runtime_limits(limits: runtime::RuntimeLimits) -> Self {
        Self { limits }
    }

    pub fn as_runtime_limits(&self) -> &runtime::RuntimeLimits {
        &self.limits
    }

    pub(crate) fn into_runtime_limits(self) -> RuntimeLimits {
        self.limits
    }
}

impl From<RuntimeLimits> for ReviewRunLimits {
    fn from(value: RuntimeLimits) -> Self {
        Self::from_runtime_limits(value)
    }
}

#[derive(Debug, Clone)]
pub struct ReviewSessionSpec {
    id: SessionId,
    role: Role,
    objective: String,
    instructions: Vec<SessionInstruction>,
    snapshot_id: Option<SnapshotId>,
    model_profile_id: Option<String>,
    response_format: Option<ModelResponseFormat>,
    capabilities: CapabilitySet,
    budget: AgentBudget,
}

impl ReviewSessionSpec {
    pub fn review_read_only(
        id: impl Into<String>,
        role: Role,
        objective: impl Into<String>,
        budget: AgentBudget,
    ) -> Self {
        Self {
            id: SessionId(id.into()),
            role,
            objective: objective.into(),
            instructions: Vec::new(),
            snapshot_id: None,
            model_profile_id: None,
            response_format: None,
            capabilities: CapabilitySet::review_read_only(),
            budget,
        }
    }

    pub fn with_snapshot_id(mut self, snapshot_id: SnapshotId) -> Self {
        self.snapshot_id = Some(snapshot_id);
        self
    }

    pub fn with_model_profile_id(mut self, model_profile_id: impl Into<String>) -> Self {
        self.model_profile_id = Some(model_profile_id.into());
        self
    }

    pub fn with_response_format(mut self, response_format: ModelResponseFormat) -> Self {
        self.response_format = Some(response_format);
        self
    }

    pub fn with_instructions(
        mut self,
        instructions: impl IntoIterator<Item = SessionInstruction>,
    ) -> Self {
        self.instructions = instructions.into_iter().collect();
        self
    }

    pub fn with_capabilities(mut self, capabilities: capabilities::CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn deny_tool(mut self, tool_id: ToolId) -> Self {
        self.capabilities.tool_grants.remove(&tool_id);
        self
    }

    pub fn grant_custom_read_only_tool(mut self, tool_id: ToolId) -> Self {
        self.capabilities.runtime_authority.host_read = true;
        self.capabilities
            .grant_tool(tool_id, ToolGrant::allow_custom_read_only());
        self
    }

    pub fn grant_provider_read_only_tool(
        mut self,
        provider_id: ToolProviderId,
        tool_id: ToolId,
    ) -> Self {
        allow_runtime_provider(&mut self.capabilities, provider_id);
        self.capabilities.grant_tool(
            tool_id,
            ToolGrant {
                allow: true,
                max_calls: None,
                effects_allowed: ToolEffects::review_read_only(),
            },
        );
        self
    }

    pub fn grant_provider_read_only_tool_for_resources(
        mut self,
        provider_id: ToolProviderId,
        tool_id: ToolId,
        provider_resources: Vec<ProviderResourceId>,
    ) -> Self {
        allow_runtime_provider(&mut self.capabilities, provider_id.clone());
        allow_runtime_provider_resources(&mut self.capabilities, provider_id, provider_resources);
        self.capabilities.grant_tool(
            tool_id,
            ToolGrant {
                allow: true,
                max_calls: None,
                effects_allowed: ToolEffects::review_read_only(),
            },
        );
        self
    }

    pub fn grant_provider_network_read_tool(
        mut self,
        provider_id: ToolProviderId,
        tool_id: ToolId,
    ) -> Self {
        self.capabilities.runtime_authority.network_read = true;
        allow_runtime_provider(&mut self.capabilities, provider_id);
        self.capabilities.grant_tool(
            tool_id,
            ToolGrant {
                allow: true,
                max_calls: None,
                effects_allowed: provider_network_read_effects(),
            },
        );
        self
    }

    pub fn grant_provider_network_read_tool_for_resources(
        mut self,
        provider_id: ToolProviderId,
        tool_id: ToolId,
        provider_resources: Vec<ProviderResourceId>,
    ) -> Self {
        self.capabilities.runtime_authority.network_read = true;
        allow_runtime_provider(&mut self.capabilities, provider_id.clone());
        allow_runtime_provider_resources(&mut self.capabilities, provider_id, provider_resources);
        self.capabilities.grant_tool(
            tool_id,
            ToolGrant {
                allow: true,
                max_calls: None,
                effects_allowed: provider_network_read_effects(),
            },
        );
        self
    }

    pub fn grant_custom_read_only_tool_for_resources(
        self,
        tool_id: ToolId,
        provider_resources: Vec<ProviderResourceId>,
    ) -> Self {
        self.grant_custom_tool_with_effects_for_resources(
            tool_id,
            provider_resources,
            ToolEffects::custom_read_only(),
        )
    }

    pub fn grant_custom_tool_with_effects(mut self, tool_id: ToolId, effects: ToolEffects) -> Self {
        allow_custom_tool_effect_authority(&mut self.capabilities, effects);
        self.capabilities.grant_tool(
            tool_id,
            ToolGrant {
                allow: true,
                max_calls: None,
                effects_allowed: effects,
            },
        );
        self
    }

    pub fn grant_custom_tool_with_effects_for_resources(
        mut self,
        tool_id: ToolId,
        provider_resources: Vec<ProviderResourceId>,
        effects: ToolEffects,
    ) -> Self {
        allow_custom_tool_effect_authority(&mut self.capabilities, effects);
        self.capabilities.runtime_authority.host_read = true;
        let scopes = provider_resources
            .into_iter()
            .map(|resource_id| {
                ProviderResourceScope::new(ToolProviderId::in_process(), resource_id)
            })
            .collect::<Vec<_>>();
        match &mut self
            .capabilities
            .runtime_authority
            .allowed_provider_resources
        {
            Some(existing) => existing.extend(scopes),
            None => {
                self.capabilities
                    .runtime_authority
                    .allowed_provider_resources = Some(scopes)
            }
        }
        self.capabilities.grant_tool(
            tool_id,
            ToolGrant {
                allow: true,
                max_calls: None,
                effects_allowed: effects,
            },
        );
        self
    }

    pub(crate) fn into_session_scope(self) -> SessionScope {
        SessionScope {
            id: self.id,
            role: self.role,
            objective: self.objective,
            instructions: self.instructions,
            snapshot_id: self.snapshot_id,
            model_profile_id: self.model_profile_id,
            response_format: self.response_format,
            capabilities: self.capabilities,
            budget: self.budget,
        }
    }
}

impl From<SessionScope> for ReviewSessionSpec {
    fn from(value: SessionScope) -> Self {
        Self {
            id: value.id,
            role: value.role,
            objective: value.objective,
            instructions: value.instructions,
            snapshot_id: value.snapshot_id,
            model_profile_id: value.model_profile_id,
            response_format: value.response_format,
            capabilities: value.capabilities,
            budget: value.budget,
        }
    }
}

fn allow_runtime_provider(capabilities: &mut CapabilitySet, provider_id: ToolProviderId) {
    let providers = capabilities
        .runtime_authority
        .allowed_provider_ids
        .get_or_insert_with(|| {
            vec![
                ToolProviderId::builtin_review(),
                ToolProviderId::in_process(),
            ]
        });
    if !providers.iter().any(|allowed| allowed == &provider_id) {
        providers.push(provider_id);
    }
}

fn allow_runtime_provider_resources(
    capabilities: &mut CapabilitySet,
    provider_id: ToolProviderId,
    provider_resources: Vec<ProviderResourceId>,
) {
    let resources = capabilities
        .runtime_authority
        .allowed_provider_resources
        .get_or_insert_with(Vec::new);
    for resource_id in provider_resources {
        let scope = ProviderResourceScope::new(provider_id.clone(), resource_id);
        if !resources.iter().any(|allowed| allowed == &scope) {
            resources.push(scope);
        }
    }
}

fn allow_custom_tool_effect_authority(capabilities: &mut CapabilitySet, effects: ToolEffects) {
    if effects.host_read {
        capabilities.runtime_authority.host_read = true;
    }
    if effects.network_read {
        capabilities.runtime_authority.network_read = true;
    }
    if effects.scratch_read {
        capabilities.runtime_authority.scratch_read = true;
    }
    if effects.scratch_write {
        capabilities.runtime_authority.scratch_write = true;
    }
    if effects.external_side_effect {
        capabilities.runtime_authority.external_side_effect = true;
    }
}

pub(crate) fn provider_network_read_effects() -> ToolEffects {
    ToolEffects {
        network_read: true,
        ..ToolEffects::review_read_only()
    }
}
