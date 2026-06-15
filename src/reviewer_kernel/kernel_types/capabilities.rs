use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    stable_id, ArtifactId, ProviderResourceId, ProviderResourceScope, RepoPath, ScopeKey,
    SnapshotId, ToolId, ToolProviderId,
};
use crate::reviewer_kernel::review_contract::ToolName;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsScope {
    pub cwd: Option<RepoPath>,
    pub allowed_roots: Vec<RepoPath>,
    pub candidate_set_hash: String,
}

impl FsScope {
    pub fn repo_root() -> Self {
        Self {
            cwd: None,
            allowed_roots: Vec::new(),
            candidate_set_hash: "root".to_string(),
        }
    }

    pub fn subtree(path: RepoPath) -> Self {
        let candidate_set_hash = stable_id(&[&path.display()]);
        Self {
            cwd: Some(path.clone()),
            allowed_roots: vec![path],
            candidate_set_hash,
        }
    }

    pub fn allows(&self, path: &RepoPath) -> bool {
        if let Some(cwd) = &self.cwd {
            if path.as_path() != cwd.as_path() && !path.as_path().starts_with(cwd.as_path()) {
                return false;
            }
        }
        if self.allowed_roots.is_empty() {
            return true;
        }
        self.allowed_roots.iter().any(|root| {
            path.as_path() == root.as_path() || path.as_path().starts_with(root.as_path())
        })
    }

    pub fn scope_key(&self, snapshot_id: &SnapshotId) -> ScopeKey {
        let mut owned_parts = vec![snapshot_id.0.clone(), self.candidate_set_hash.clone()];
        if let Some(cwd) = &self.cwd {
            owned_parts.push(cwd.display());
        }
        for root in &self.allowed_roots {
            owned_parts.push(root.display());
        }
        let parts = owned_parts.iter().map(String::as_str).collect::<Vec<_>>();
        ScopeKey(stable_id(&parts))
    }
}

#[derive(Debug, Copy, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolEffects {
    #[serde(default)]
    pub repo_read: bool,
    #[serde(default)]
    pub artifact_read: bool,
    #[serde(default)]
    pub artifact_write: bool,
    #[serde(default)]
    pub network_read: bool,
    #[serde(default)]
    pub host_read: bool,
    #[serde(default)]
    pub scratch_read: bool,
    #[serde(default)]
    pub scratch_write: bool,
    #[serde(default)]
    pub external_side_effect: bool,
}

impl ToolEffects {
    pub fn review_read_only() -> Self {
        Self {
            repo_read: true,
            artifact_read: true,
            artifact_write: true,
            network_read: false,
            host_read: false,
            scratch_read: false,
            scratch_write: false,
            external_side_effect: false,
        }
    }

    pub fn custom_read_only() -> Self {
        Self {
            repo_read: true,
            artifact_read: true,
            artifact_write: true,
            network_read: false,
            host_read: true,
            scratch_read: false,
            scratch_write: false,
            external_side_effect: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolGrant {
    pub allow: bool,
    pub max_calls: Option<u32>,
    pub effects_allowed: ToolEffects,
}

impl ToolGrant {
    pub fn allow_review_read_only() -> Self {
        Self {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::review_read_only(),
        }
    }

    pub fn allow_custom_read_only() -> Self {
        Self {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::custom_read_only(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySet {
    pub fs_scope: FsScope,
    pub tool_grants: BTreeMap<ToolId, ToolGrant>,
    #[serde(default)]
    pub artifact_access: ArtifactAccessPolicy,
    #[serde(default)]
    pub model_output: ModelOutputPolicy,
    #[serde(default)]
    pub tool_input: ToolInputPolicy,
    #[serde(default)]
    pub runtime_authority: RuntimeAuthorityPolicy,
}

impl CapabilitySet {
    pub fn review_read_only() -> Self {
        let mut capabilities = Self {
            fs_scope: FsScope::repo_root(),
            tool_grants: BTreeMap::new(),
            artifact_access: ArtifactAccessPolicy::review_read_only(),
            model_output: ModelOutputPolicy::review_read_only(),
            tool_input: ToolInputPolicy::review_read_only(),
            runtime_authority: RuntimeAuthorityPolicy::review_read_only(),
        };
        for &tool in ToolName::review_read_only_tools() {
            capabilities.grant(ToolId::from(tool), ToolGrant::allow_review_read_only());
        }
        capabilities
    }

    pub fn empty_review_policy(fs_scope: FsScope) -> Self {
        Self {
            fs_scope,
            tool_grants: BTreeMap::new(),
            artifact_access: ArtifactAccessPolicy::review_read_only(),
            model_output: ModelOutputPolicy::review_read_only(),
            tool_input: ToolInputPolicy::review_read_only(),
            runtime_authority: RuntimeAuthorityPolicy::review_read_only(),
        }
    }

    pub fn with_fs_scope(mut self, fs_scope: FsScope) -> Self {
        self.fs_scope = fs_scope;
        self
    }

    pub fn grant(&mut self, tool_id: ToolId, grant: ToolGrant) {
        self.tool_grants.insert(tool_id, grant);
    }

    pub fn grant_tool(&mut self, tool_id: ToolId, grant: ToolGrant) {
        self.grant(tool_id, grant);
    }

    pub fn allows_tool(&self, tool_id: &ToolId) -> bool {
        self.tool_grants
            .get(tool_id)
            .map(|grant| grant.allow)
            .unwrap_or(false)
    }

    pub fn allow_tool(&self, tool_id: &ToolId) -> bool {
        self.allows_tool(tool_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactAccessPolicy {
    pub read_redacted: bool,
    pub read_raw: bool,
    pub write: bool,
    #[serde(default)]
    pub allowed_artifact_ids: Option<Vec<ArtifactId>>,
}

impl ArtifactAccessPolicy {
    pub fn review_read_only() -> Self {
        Self {
            read_redacted: true,
            read_raw: false,
            write: true,
            allowed_artifact_ids: None,
        }
    }

    pub fn deny_all() -> Self {
        Self {
            read_redacted: false,
            read_raw: false,
            write: false,
            allowed_artifact_ids: None,
        }
    }

    pub fn allow_raw() -> Self {
        Self {
            read_redacted: true,
            read_raw: true,
            write: true,
            allowed_artifact_ids: None,
        }
    }

    pub fn scoped_to_artifacts(mut self, artifact_ids: Vec<ArtifactId>) -> Self {
        self.allowed_artifact_ids = Some(artifact_ids);
        self
    }

    pub fn allows_artifact(&self, artifact_id: &ArtifactId) -> bool {
        match &self.allowed_artifact_ids {
            Some(allowed) => allowed.iter().any(|allowed_id| allowed_id == artifact_id),
            None => true,
        }
    }
}

impl Default for ArtifactAccessPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelOutputPolicy {
    pub include_tool_data: bool,
    pub include_artifact_refs: bool,
    pub max_tool_data_bytes: usize,
}

impl ModelOutputPolicy {
    pub fn review_read_only() -> Self {
        Self {
            include_tool_data: true,
            include_artifact_refs: true,
            max_tool_data_bytes: 16 * 1024,
        }
    }

    pub fn metadata_only() -> Self {
        Self {
            include_tool_data: false,
            include_artifact_refs: false,
            max_tool_data_bytes: 0,
        }
    }
}

impl Default for ModelOutputPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolInputPolicy {
    pub max_argument_bytes: usize,
}

impl ToolInputPolicy {
    pub fn review_read_only() -> Self {
        Self {
            max_argument_bytes: 16 * 1024,
        }
    }
}

impl Default for ToolInputPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAuthorityPolicy {
    pub network_read: bool,
    pub host_read: bool,
    pub scratch_read: bool,
    pub scratch_write: bool,
    pub external_side_effect: bool,
    #[serde(default)]
    pub allowed_provider_ids: Option<Vec<ToolProviderId>>,
    #[serde(default)]
    pub allowed_provider_resources: Option<Vec<ProviderResourceScope>>,
}

impl RuntimeAuthorityPolicy {
    pub fn review_read_only() -> Self {
        Self {
            network_read: false,
            host_read: false,
            scratch_read: false,
            scratch_write: false,
            external_side_effect: false,
            allowed_provider_ids: None,
            allowed_provider_resources: None,
        }
    }

    pub fn trusted_host_read() -> Self {
        Self {
            host_read: true,
            ..Self::review_read_only()
        }
    }

    pub fn allow_all_trusted() -> Self {
        Self {
            network_read: true,
            host_read: true,
            scratch_read: true,
            scratch_write: true,
            external_side_effect: true,
            allowed_provider_ids: None,
            allowed_provider_resources: None,
        }
    }

    pub fn scoped_to_providers(mut self, provider_ids: Vec<ToolProviderId>) -> Self {
        self.allowed_provider_ids = Some(provider_ids);
        self
    }

    pub fn allows_provider(&self, provider_id: &ToolProviderId) -> bool {
        match &self.allowed_provider_ids {
            Some(allowed) => allowed.iter().any(|allowed_id| allowed_id == provider_id),
            None => true,
        }
    }

    pub fn scoped_to_provider_resources(mut self, resources: Vec<ProviderResourceScope>) -> Self {
        self.allowed_provider_resources = Some(resources);
        self
    }

    pub fn allows_provider_resource(
        &self,
        provider_id: &ToolProviderId,
        resource_id: &ProviderResourceId,
    ) -> bool {
        match &self.allowed_provider_resources {
            Some(allowed) => allowed.iter().any(|scope| {
                &scope.provider_id == provider_id && &scope.resource_id == resource_id
            }),
            None => true,
        }
    }

    pub fn allows_provider_resources(
        &self,
        provider_id: &ToolProviderId,
        resource_ids: &[ProviderResourceId],
    ) -> bool {
        resource_ids
            .iter()
            .all(|resource_id| self.allows_provider_resource(provider_id, resource_id))
    }
}

impl Default for RuntimeAuthorityPolicy {
    fn default() -> Self {
        Self::review_read_only()
    }
}
