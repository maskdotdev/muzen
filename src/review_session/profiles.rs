use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{ProfileVersionRef, ReviewSessionError};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProviderKind {
    Openai,
    Anthropic,
    OpenaiCompatible,
}

impl ModelProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::OpenaiCompatible => "openai_compatible",
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceProviderKind {
    Github,
    Gitlab,
}

impl SourceProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfileInput {
    pub provider: ModelProviderKind,
    pub model: String,
    #[serde(default)]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub routing: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfileInput {
    pub provider: SourceProviderKind,
    #[serde(default)]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub routing: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfile {
    pub workspace_id: String,
    pub name: String,
    pub version: String,
    pub provider: ModelProviderKind,
    pub model: String,
    #[serde(default)]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub routing: BTreeMap<String, String>,
    pub updated_at_utc: String,
}

impl ModelProfile {
    pub(crate) fn version_ref(&self) -> ProfileVersionRef {
        ProfileVersionRef {
            id: format!("workspace:{}/models/{}", self.workspace_id, self.name),
            version: self.version.clone(),
            secret_ref: self.secret_ref.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub workspace_id: String,
    pub name: String,
    pub version: String,
    pub provider: SourceProviderKind,
    #[serde(default)]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub routing: BTreeMap<String, String>,
    pub updated_at_utc: String,
}

impl ProviderProfile {
    pub(crate) fn version_ref(&self) -> ProfileVersionRef {
        ProfileVersionRef {
            id: format!("workspace:{}/providers/{}", self.workspace_id, self.name),
            version: self.version.clone(),
            secret_ref: self.secret_ref.clone(),
        }
    }
}

#[async_trait]
pub trait WorkspaceProfileStore: Send + Sync {
    async fn set_model_profile(
        &self,
        workspace_id: &str,
        name: String,
        input: ModelProfileInput,
    ) -> Result<ModelProfile, ReviewSessionError>;

    async fn get_model_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ModelProfile>, ReviewSessionError>;

    async fn list_model_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ModelProfile>, ReviewSessionError>;

    async fn set_provider_profile(
        &self,
        workspace_id: &str,
        name: String,
        input: ProviderProfileInput,
    ) -> Result<ProviderProfile, ReviewSessionError>;

    async fn get_provider_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError>;

    async fn list_provider_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ProviderProfile>, ReviewSessionError>;
}

#[derive(Debug, Default)]
pub struct InMemoryWorkspaceProfileStore {
    state: Mutex<ProfileStoreState>,
}

#[derive(Debug, Default)]
struct ProfileStoreState {
    models: BTreeMap<(String, String), ModelProfile>,
    providers: BTreeMap<(String, String), ProviderProfile>,
}

#[async_trait]
impl WorkspaceProfileStore for InMemoryWorkspaceProfileStore {
    async fn set_model_profile(
        &self,
        workspace_id: &str,
        name: String,
        input: ModelProfileInput,
    ) -> Result<ModelProfile, ReviewSessionError> {
        validate_profile_key(workspace_id, &name)?;
        if input.model.trim().is_empty() {
            return Err(ReviewSessionError::Profile(
                "model profile model cannot be empty".to_string(),
            ));
        }
        let mut state = self.lock_state()?;
        let key = (workspace_id.to_string(), name.clone());
        let version = next_version(
            state
                .models
                .get(&key)
                .map(|profile| profile.version.as_str()),
        );
        let profile = ModelProfile {
            workspace_id: workspace_id.to_string(),
            name,
            version,
            provider: input.provider,
            model: input.model,
            secret_ref: input.secret_ref,
            base_url: input.base_url,
            routing: input.routing,
            updated_at_utc: crate::util::timestamp_utc(),
        };
        state.models.insert(key, profile.clone());
        Ok(profile)
    }

    async fn get_model_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ModelProfile>, ReviewSessionError> {
        let state = self.lock_state()?;
        Ok(state
            .models
            .get(&(workspace_id.to_string(), name.to_string()))
            .cloned())
    }

    async fn list_model_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ModelProfile>, ReviewSessionError> {
        let state = self.lock_state()?;
        Ok(state
            .models
            .iter()
            .filter(|((stored_workspace_id, _), _)| stored_workspace_id == workspace_id)
            .map(|(_, profile)| profile.clone())
            .collect())
    }

    async fn set_provider_profile(
        &self,
        workspace_id: &str,
        name: String,
        input: ProviderProfileInput,
    ) -> Result<ProviderProfile, ReviewSessionError> {
        validate_profile_key(workspace_id, &name)?;
        let mut state = self.lock_state()?;
        let key = (workspace_id.to_string(), name.clone());
        let version = next_version(
            state
                .providers
                .get(&key)
                .map(|profile| profile.version.as_str()),
        );
        let profile = ProviderProfile {
            workspace_id: workspace_id.to_string(),
            name,
            version,
            provider: input.provider,
            secret_ref: input.secret_ref,
            base_url: input.base_url,
            routing: input.routing,
            updated_at_utc: crate::util::timestamp_utc(),
        };
        state.providers.insert(key, profile.clone());
        Ok(profile)
    }

    async fn get_provider_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError> {
        let state = self.lock_state()?;
        Ok(state
            .providers
            .get(&(workspace_id.to_string(), name.to_string()))
            .cloned())
    }

    async fn list_provider_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ProviderProfile>, ReviewSessionError> {
        let state = self.lock_state()?;
        Ok(state
            .providers
            .iter()
            .filter(|((stored_workspace_id, _), _)| stored_workspace_id == workspace_id)
            .map(|(_, profile)| profile.clone())
            .collect())
    }
}

impl InMemoryWorkspaceProfileStore {
    fn lock_state(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ProfileStoreState>, ReviewSessionError> {
        self.state.lock().map_err(|_| {
            ReviewSessionError::Profile("workspace profile store poisoned".to_string())
        })
    }
}

fn validate_profile_key(workspace_id: &str, name: &str) -> Result<(), ReviewSessionError> {
    if workspace_id.trim().is_empty() {
        return Err(ReviewSessionError::Profile(
            "workspace id cannot be empty".to_string(),
        ));
    }
    if name.trim().is_empty() {
        return Err(ReviewSessionError::Profile(
            "profile name cannot be empty".to_string(),
        ));
    }
    Ok(())
}

fn next_version(previous: Option<&str>) -> String {
    previous
        .and_then(|version| version.parse::<u64>().ok())
        .map_or(1, |version| version + 1)
        .to_string()
}
