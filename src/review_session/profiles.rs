use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use postgres::{Client, NoTls};
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

pub struct PostgresWorkspaceProfileStore {
    client: Mutex<Client>,
}

impl std::fmt::Debug for PostgresWorkspaceProfileStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresWorkspaceProfileStore")
            .finish_non_exhaustive()
    }
}

impl PostgresWorkspaceProfileStore {
    pub fn connect(database_url: &str) -> Result<Self, ReviewSessionError> {
        let client = Client::connect(database_url, NoTls).map_err(postgres_profile_error)?;
        let store = Self {
            client: Mutex::new(client),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn migrate(&self) -> Result<(), ReviewSessionError> {
        let mut client = self.lock_client()?;
        client
            .batch_execute(WORKSPACE_PROFILE_SCHEMA_SQL)
            .map_err(postgres_profile_error)?;
        Ok(())
    }

    fn lock_client(&self) -> Result<std::sync::MutexGuard<'_, Client>, ReviewSessionError> {
        self.client
            .lock()
            .map_err(|_| ReviewSessionError::Profile("postgres profile store poisoned".to_string()))
    }
}

#[async_trait]
impl WorkspaceProfileStore for PostgresWorkspaceProfileStore {
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
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_profile_error)?;
        let current = transaction
            .query_opt(
                "SELECT version FROM muzen_workspace_model_profiles \
                 WHERE workspace_id = $1 AND name = $2 \
                 FOR UPDATE",
                &[&workspace_id, &name],
            )
            .map_err(postgres_profile_error)?;
        let version = next_version(
            current
                .as_ref()
                .map(|row| row.get::<_, String>("version"))
                .as_deref(),
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
        let profile_json = serde_json::to_value(&profile).map_err(|error| {
            ReviewSessionError::Profile(format!("model profile JSON error: {error}"))
        })?;
        transaction
            .execute(
                "INSERT INTO muzen_workspace_model_profiles \
                    (workspace_id, name, version, profile, updated_at_utc) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (workspace_id, name) DO UPDATE SET \
                    version = EXCLUDED.version, \
                    profile = EXCLUDED.profile, \
                    updated_at_utc = EXCLUDED.updated_at_utc",
                &[
                    &profile.workspace_id,
                    &profile.name,
                    &profile.version,
                    &profile_json,
                    &profile.updated_at_utc,
                ],
            )
            .map_err(postgres_profile_error)?;
        transaction.commit().map_err(postgres_profile_error)?;
        Ok(profile)
    }

    async fn get_model_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ModelProfile>, ReviewSessionError> {
        let mut client = self.lock_client()?;
        client
            .query_opt(
                "SELECT profile FROM muzen_workspace_model_profiles \
                 WHERE workspace_id = $1 AND name = $2",
                &[&workspace_id, &name],
            )
            .map_err(postgres_profile_error)?
            .map(|row| deserialize_profile(row.get("profile"), "model"))
            .transpose()
    }

    async fn list_model_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ModelProfile>, ReviewSessionError> {
        let mut client = self.lock_client()?;
        client
            .query(
                "SELECT profile FROM muzen_workspace_model_profiles \
                 WHERE workspace_id = $1 \
                 ORDER BY name ASC",
                &[&workspace_id],
            )
            .map_err(postgres_profile_error)?
            .into_iter()
            .map(|row| deserialize_profile(row.get("profile"), "model"))
            .collect()
    }

    async fn set_provider_profile(
        &self,
        workspace_id: &str,
        name: String,
        input: ProviderProfileInput,
    ) -> Result<ProviderProfile, ReviewSessionError> {
        validate_profile_key(workspace_id, &name)?;
        let mut client = self.lock_client()?;
        let mut transaction = client.transaction().map_err(postgres_profile_error)?;
        let current = transaction
            .query_opt(
                "SELECT version FROM muzen_workspace_provider_profiles \
                 WHERE workspace_id = $1 AND name = $2 \
                 FOR UPDATE",
                &[&workspace_id, &name],
            )
            .map_err(postgres_profile_error)?;
        let version = next_version(
            current
                .as_ref()
                .map(|row| row.get::<_, String>("version"))
                .as_deref(),
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
        let profile_json = serde_json::to_value(&profile).map_err(|error| {
            ReviewSessionError::Profile(format!("provider profile JSON error: {error}"))
        })?;
        transaction
            .execute(
                "INSERT INTO muzen_workspace_provider_profiles \
                    (workspace_id, name, version, profile, updated_at_utc) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (workspace_id, name) DO UPDATE SET \
                    version = EXCLUDED.version, \
                    profile = EXCLUDED.profile, \
                    updated_at_utc = EXCLUDED.updated_at_utc",
                &[
                    &profile.workspace_id,
                    &profile.name,
                    &profile.version,
                    &profile_json,
                    &profile.updated_at_utc,
                ],
            )
            .map_err(postgres_profile_error)?;
        transaction.commit().map_err(postgres_profile_error)?;
        Ok(profile)
    }

    async fn get_provider_profile(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError> {
        let mut client = self.lock_client()?;
        client
            .query_opt(
                "SELECT profile FROM muzen_workspace_provider_profiles \
                 WHERE workspace_id = $1 AND name = $2",
                &[&workspace_id, &name],
            )
            .map_err(postgres_profile_error)?
            .map(|row| deserialize_profile(row.get("profile"), "provider"))
            .transpose()
    }

    async fn list_provider_profiles(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ProviderProfile>, ReviewSessionError> {
        let mut client = self.lock_client()?;
        client
            .query(
                "SELECT profile FROM muzen_workspace_provider_profiles \
                 WHERE workspace_id = $1 \
                 ORDER BY name ASC",
                &[&workspace_id],
            )
            .map_err(postgres_profile_error)?
            .into_iter()
            .map(|row| deserialize_profile(row.get("profile"), "provider"))
            .collect()
    }
}

const WORKSPACE_PROFILE_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS muzen_workspace_model_profiles (
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    profile JSONB NOT NULL,
    updated_at_utc TEXT NOT NULL,
    PRIMARY KEY (workspace_id, name)
);

CREATE TABLE IF NOT EXISTS muzen_workspace_provider_profiles (
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    profile JSONB NOT NULL,
    updated_at_utc TEXT NOT NULL,
    PRIMARY KEY (workspace_id, name)
);
"#;

fn deserialize_profile<T: for<'de> Deserialize<'de>>(
    value: serde_json::Value,
    kind: &str,
) -> Result<T, ReviewSessionError> {
    serde_json::from_value(value)
        .map_err(|error| ReviewSessionError::Profile(format!("{kind} profile JSON error: {error}")))
}

fn postgres_profile_error(error: postgres::Error) -> ReviewSessionError {
    ReviewSessionError::Profile(format!("postgres profile store error: {error}"))
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
