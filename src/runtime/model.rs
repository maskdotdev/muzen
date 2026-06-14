use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::contracts::{
    AgentBudget, ModelApiProtocol, ModelProfileRefV1, ProviderKind, Role, TokenUsage,
};
use crate::runtime::contracts::*;
use crate::runtime::model_anthropic::{anthropic_default_base_url, AnthropicMessagesClient};
use crate::runtime::policy::ReviewerPolicy;
use crate::runtime::tools::ToolRegistry;
use crate::util::{redact_known_secrets, resolve_credential_ref, timestamp_utc};

#[async_trait]
pub trait ConcurrentModelClient: Send + Sync {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        turn_id: TurnId,
        cancel: CancellationToken,
    ) -> RuntimeResult<ModelTurn>;

    fn estimate_cost(&self, _usage: &TokenUsage) -> Option<ModelCostEstimate> {
        None
    }
}

#[async_trait]
pub trait ConcurrentModelRouter: Send + Sync {
    async fn client_for(
        &self,
        scope: &SessionScope,
    ) -> RuntimeResult<Arc<dyn ConcurrentModelClient>>;
}

pub struct StaticModelRouter {
    client: Arc<dyn ConcurrentModelClient>,
}

impl StaticModelRouter {
    pub fn new(client: Arc<dyn ConcurrentModelClient>) -> Self {
        Self { client }
    }
}

impl std::fmt::Debug for StaticModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticModelRouter").finish_non_exhaustive()
    }
}

#[async_trait]
impl ConcurrentModelRouter for StaticModelRouter {
    async fn client_for(
        &self,
        _scope: &SessionScope,
    ) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
        Ok(Arc::clone(&self.client))
    }
}

pub struct ProfileModelRouter {
    clients: HashMap<String, Arc<dyn ConcurrentModelClient>>,
    default_profile_id: String,
}

pub trait CredentialResolver: Send + Sync {
    fn resolve_credential(&self, credential_ref: &str) -> RuntimeResult<String>;
}

#[derive(Debug, Clone, Default)]
pub struct EnvCredentialResolver;

impl CredentialResolver for EnvCredentialResolver {
    fn resolve_credential(&self, credential_ref: &str) -> RuntimeResult<String> {
        resolve_credential_ref(credential_ref)
            .map_err(|_| RuntimeError::InvalidInput("model credential is unavailable".to_string()))
    }
}

impl ProfileModelRouter {
    pub(crate) fn from_profiles(
        profiles: &[ModelProfileRefV1],
        default_profile_id: String,
        default_base_url: String,
        limiter: Arc<ModelLimiter>,
        tool_registry: Arc<ToolRegistry>,
        reviewer_policy: Arc<ReviewerPolicy>,
        credential_resolver: Arc<dyn CredentialResolver>,
    ) -> RuntimeResult<Self> {
        if profiles.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "at least one model profile is required".to_string(),
            ));
        }
        let mut clients = HashMap::new();
        for profile in profiles {
            let client = match profile.provider_kind {
                ProviderKind::OpenaiCompatible => openai_client_from_profile(
                    profile.clone(),
                    profile_base_url(profile, &default_base_url),
                    Arc::clone(&limiter),
                    Arc::clone(&tool_registry),
                    Arc::clone(&reviewer_policy),
                    Arc::clone(&credential_resolver),
                )?,
                ProviderKind::Anthropic => anthropic_client_from_profile(
                    profile.clone(),
                    // Anthropic profiles never fall back to the run-level
                    // OpenAI-compatible endpoint.
                    profile_base_url(profile, &anthropic_default_base_url()),
                    Arc::clone(&limiter),
                    Arc::clone(&tool_registry),
                    Arc::clone(&reviewer_policy),
                    Arc::clone(&credential_resolver),
                )?,
            };
            clients.insert(profile.id.clone(), client);
        }
        if !clients.contains_key(&default_profile_id) {
            return Err(RuntimeError::InvalidInput(format!(
                "missing default model profile {default_profile_id}"
            )));
        }
        Ok(Self {
            clients,
            default_profile_id,
        })
    }
}

impl std::fmt::Debug for ProfileModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileModelRouter")
            .field("clients", &self.clients.len())
            .field("default_profile_id", &self.default_profile_id)
            .finish()
    }
}

#[async_trait]
impl ConcurrentModelRouter for ProfileModelRouter {
    async fn client_for(
        &self,
        scope: &SessionScope,
    ) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
        if let Some(profile_id) = scope.model_profile_id.as_ref() {
            return self.clients.get(profile_id).cloned().ok_or_else(|| {
                RuntimeError::InvalidInput(format!("missing model profile {profile_id}"))
            });
        }
        self.clients
            .get(&self.default_profile_id)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "missing model profile {}",
                    self.default_profile_id
                ))
            })
    }
}

#[derive(Debug)]
pub struct ModelLimiter {
    global: Arc<Semaphore>,
    per_provider: DashMap<String, Arc<Semaphore>>,
    per_profile: DashMap<String, Arc<Semaphore>>,
    per_key: DashMap<String, Arc<Semaphore>>,
    per_session: DashMap<String, Arc<Semaphore>>,
    max_per_provider: usize,
    max_per_profile: usize,
    max_per_key: usize,
    max_per_session: usize,
}

#[derive(Debug)]
pub struct ModelPermit {
    _global: OwnedSemaphorePermit,
    _buckets: Vec<OwnedSemaphorePermit>,
}

impl ModelLimiter {
    pub fn new(global_concurrency: usize) -> Self {
        Self::new_with_per_key(global_concurrency, global_concurrency)
    }

    pub fn new_with_per_key(global_concurrency: usize, max_per_key: usize) -> Self {
        Self::new_with_buckets(
            global_concurrency,
            global_concurrency,
            global_concurrency,
            max_per_key,
            1,
        )
    }

    pub fn new_with_buckets(
        global_concurrency: usize,
        max_per_provider: usize,
        max_per_profile: usize,
        max_per_key: usize,
        max_per_session: usize,
    ) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_concurrency.max(1))),
            per_provider: DashMap::new(),
            per_profile: DashMap::new(),
            per_key: DashMap::new(),
            per_session: DashMap::new(),
            max_per_provider: max_per_provider.max(1),
            max_per_profile: max_per_profile.max(1),
            max_per_key: max_per_key.max(1),
            max_per_session: max_per_session.max(1),
        }
    }

    pub async fn acquire(&self) -> RuntimeResult<ModelPermit> {
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        Ok(ModelPermit {
            _global: global,
            _buckets: Vec::new(),
        })
    }

    pub async fn acquire_for_key(&self, key: &str) -> RuntimeResult<ModelPermit> {
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        let key_limiter = self
            .per_key
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.max_per_key)))
            .clone();
        let key = key_limiter
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        Ok(ModelPermit {
            _global: global,
            _buckets: vec![key],
        })
    }

    pub async fn acquire_for_model(
        &self,
        provider: &str,
        profile_id: &str,
        credential_ref: &str,
        session_id: &SessionId,
    ) -> RuntimeResult<ModelPermit> {
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        let buckets = vec![
            self.acquire_bucket(&self.per_provider, provider, self.max_per_provider)
                .await?,
            self.acquire_bucket(&self.per_profile, profile_id, self.max_per_profile)
                .await?,
            self.acquire_bucket(&self.per_key, credential_ref, self.max_per_key)
                .await?,
            self.acquire_bucket(&self.per_session, &session_id.0, self.max_per_session)
                .await?,
        ];
        Ok(ModelPermit {
            _global: global,
            _buckets: buckets,
        })
    }

    async fn acquire_bucket(
        &self,
        buckets: &DashMap<String, Arc<Semaphore>>,
        key: &str,
        max_per_bucket: usize,
    ) -> RuntimeResult<OwnedSemaphorePermit> {
        buckets
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(max_per_bucket)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RuntimeError::Cancelled)
    }
}

/// Each profile may route to its own endpoint; profiles without one share the
/// run-level default.
fn profile_base_url(profile: &ModelProfileRefV1, default_base_url: &str) -> String {
    profile
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| default_base_url.to_string())
}

fn openai_client_from_profile(
    profile: ModelProfileRefV1,
    base_url: String,
    limiter: Arc<ModelLimiter>,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    credential_resolver: Arc<dyn CredentialResolver>,
) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
    match profile.api_protocol {
        ModelApiProtocol::Responses => Ok(Arc::new(OpenAiResponsesClient::from_profile(
            profile,
            base_url,
            limiter,
            tool_registry,
            reviewer_policy,
            credential_resolver,
        )?)),
        ModelApiProtocol::Messages => Err(RuntimeError::InvalidInput(format!(
            "model profile {} is openai_compatible but uses the messages protocol; use provider anthropic",
            profile.id
        ))),
    }
}

fn anthropic_client_from_profile(
    profile: ModelProfileRefV1,
    base_url: String,
    limiter: Arc<ModelLimiter>,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    credential_resolver: Arc<dyn CredentialResolver>,
) -> RuntimeResult<Arc<dyn ConcurrentModelClient>> {
    if profile.api_protocol != ModelApiProtocol::Messages {
        return Err(RuntimeError::InvalidInput(format!(
            "model profile {} is anthropic and must use the messages protocol",
            profile.id
        )));
    }
    Ok(Arc::new(AnthropicMessagesClient::from_profile(
        profile,
        base_url,
        limiter,
        tool_registry,
        reviewer_policy,
        credential_resolver,
    )?))
}

const OPENAI_PROVIDER_CANARY_PROMPT: &str = "Return the word ready.";
const OPENAI_PROVIDER_CANARY_MAX_INPUT_TOKENS: u32 = 4_096;
pub const MODEL_PROVIDER_CANARY_EVIDENCE_SCHEMA_VERSION: &str = "muzen.model-provider-canary.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiProviderCanaryConfig {
    pub enabled: bool,
    pub base_url: String,
    pub credential_ref: String,
    pub model: String,
    pub max_output_tokens: u32,
    pub prompt: String,
}

impl OpenAiProviderCanaryConfig {
    pub fn from_env(default_model: impl Into<String>) -> Self {
        let default_model = default_model.into();
        Self {
            enabled: std::env::var("MUZEN_RUN_REAL_PROVIDER_CANARY")
                .or_else(|_| std::env::var("MUZEN_RUN_REAL_PROVIDER_SMOKE"))
                .ok()
                .as_deref()
                == Some("1"),
            base_url: std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            credential_ref: "env:OPENAI_API_KEY".to_string(),
            model: std::env::var("MUZEN_REAL_PROVIDER_MODEL").unwrap_or(default_model),
            max_output_tokens: 32,
            prompt: OPENAI_PROVIDER_CANARY_PROMPT.to_string(),
        }
    }

    fn profile_for_protocol(&self, protocol: ModelApiProtocol) -> ModelProfileRefV1 {
        let protocol_slug = openai_provider_canary_protocol_slug(protocol);
        ModelProfileRefV1 {
            id: format!("real-provider-canary-{protocol_slug}"),
            provider_kind: ProviderKind::OpenaiCompatible,
            api_protocol: protocol,
            provider_profile_id: format!("real-provider-canary-openai-{protocol_slug}"),
            credential_ref: self.credential_ref.clone(),
            model: self.model.clone(),
            base_url: None,
            max_input_tokens: OPENAI_PROVIDER_CANARY_MAX_INPUT_TOKENS,
            max_output_tokens: self.max_output_tokens,
            temperature: Some(0.0),
            top_p: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProviderCanaryReport {
    pub protocol: ModelApiProtocol,
    pub base_url: String,
    pub model: String,
    pub credential_ref: String,
    pub status: ModelProviderCanaryStatus,
}

impl ModelProviderCanaryReport {
    fn skipped(
        protocol: ModelApiProtocol,
        config: &OpenAiProviderCanaryConfig,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            credential_ref: config.credential_ref.clone(),
            status: ModelProviderCanaryStatus::Skipped {
                reason: reason.into(),
            },
        }
    }

    fn passed(protocol: ModelApiProtocol, config: &OpenAiProviderCanaryConfig) -> Self {
        Self {
            protocol,
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            credential_ref: config.credential_ref.clone(),
            status: ModelProviderCanaryStatus::Passed,
        }
    }

    fn failed(
        protocol: ModelApiProtocol,
        config: &OpenAiProviderCanaryConfig,
        error: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            credential_ref: config.credential_ref.clone(),
            status: ModelProviderCanaryStatus::Failed {
                error: error.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelProviderCanaryStatus {
    Skipped { reason: String },
    Passed,
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderCanaryEvidence {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub required_protocols: Vec<ModelApiProtocol>,
    pub reports: Vec<ModelProviderCanaryReport>,
    pub gate: ModelProviderCanaryGate,
}

impl ModelProviderCanaryEvidence {
    pub fn from_reports(reports: Vec<ModelProviderCanaryReport>) -> Self {
        Self::with_generated_at(timestamp_utc(), reports)
    }

    pub fn with_generated_at(
        generated_at_utc: impl Into<String>,
        reports: Vec<ModelProviderCanaryReport>,
    ) -> Self {
        let required_protocols = openai_provider_canary_protocols().to_vec();
        let gate = ModelProviderCanaryGate::evaluate(&required_protocols, &reports);
        Self {
            schema_version: MODEL_PROVIDER_CANARY_EVIDENCE_SCHEMA_VERSION.to_string(),
            generated_at_utc: generated_at_utc.into(),
            required_protocols,
            reports,
            gate,
        }
    }

    pub fn require_passed(&self) -> RuntimeResult<()> {
        let failures = self.validation_failures();
        if failures.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::InvalidInput(format!(
            "real-provider canary gate failed: {}",
            failures.join("; ")
        )))
    }

    fn validation_failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != MODEL_PROVIDER_CANARY_EVIDENCE_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported provider canary evidence schema {}",
                self.schema_version
            ));
        }
        if self.required_protocols.as_slice() != openai_provider_canary_protocols() {
            failures.push(
                "required protocol matrix does not match current provider canary contract"
                    .to_string(),
            );
        }
        let evaluated = ModelProviderCanaryGate::evaluate(&self.required_protocols, &self.reports);
        if evaluated != self.gate {
            failures.push("stored provider canary gate does not match reports".to_string());
        }
        failures.extend(evaluated.failures);
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderCanaryGate {
    pub valid: bool,
    pub passed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub failures: Vec<String>,
}

impl ModelProviderCanaryGate {
    fn evaluate(
        required_protocols: &[ModelApiProtocol],
        reports: &[ModelProviderCanaryReport],
    ) -> Self {
        let mut passed = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        let mut failures = Vec::new();

        for protocol in required_protocols {
            let protocol_reports = reports
                .iter()
                .filter(|report| report.protocol == *protocol)
                .collect::<Vec<_>>();
            let protocol_slug = openai_provider_canary_protocol_slug(*protocol);
            match protocol_reports.len() {
                0 => failures.push(format!("missing {protocol_slug} canary report")),
                1 => {}
                count => {
                    failures.push(format!("duplicate {protocol_slug} canary reports: {count}"))
                }
            }
            for report in protocol_reports {
                match &report.status {
                    ModelProviderCanaryStatus::Passed => passed += 1,
                    ModelProviderCanaryStatus::Skipped { reason } => {
                        skipped += 1;
                        failures.push(format!("{protocol_slug} skipped: {reason}"));
                    }
                    ModelProviderCanaryStatus::Failed { error } => {
                        failed += 1;
                        failures.push(format!("{protocol_slug} failed: {error}"));
                    }
                }
            }
        }

        Self {
            valid: failures.is_empty() && passed == required_protocols.len(),
            passed,
            skipped,
            failed,
            failures,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderCanaryEvidenceExport {
    pub path: PathBuf,
    pub bytes: usize,
    pub valid: bool,
    pub failures: Vec<String>,
}

pub fn export_model_provider_canary_evidence(
    path: impl AsRef<Path>,
    evidence: &ModelProviderCanaryEvidence,
) -> RuntimeResult<ModelProviderCanaryEvidenceExport> {
    let path = path.as_ref();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::RepoUnavailable(format!(
                "failed to create provider canary evidence directory: {error}"
            ))
        })?;
    }
    let mut bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|_| RuntimeError::Invariant("provider canary evidence serialization failed"))?;
    bytes.push(b'\n');
    let temp_path = provider_canary_evidence_temp_path(path)?;
    let failures = evidence.validation_failures();
    fs::write(&temp_path, &bytes).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to write provider canary evidence: {error}"))
    })?;
    fs::rename(&temp_path, path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        RuntimeError::RepoUnavailable(format!(
            "failed to publish provider canary evidence: {error}"
        ))
    })?;
    Ok(ModelProviderCanaryEvidenceExport {
        path: path.to_path_buf(),
        bytes: bytes.len(),
        valid: failures.is_empty(),
        failures,
    })
}

pub fn load_model_provider_canary_evidence(
    path: impl AsRef<Path>,
) -> RuntimeResult<ModelProviderCanaryEvidence> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!(
            "failed to read provider canary evidence {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_str(&contents).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "invalid provider canary evidence {}: {error}",
            path.display()
        ))
    })
}

fn provider_canary_evidence_temp_path(path: &Path) -> RuntimeResult<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        RuntimeError::InvalidInput("provider canary evidence path must name a file".to_string())
    })?;
    Ok(path.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    )))
}

pub fn openai_provider_canary_protocols() -> &'static [ModelApiProtocol] {
    const PROTOCOLS: &[ModelApiProtocol] = &[ModelApiProtocol::Responses];
    PROTOCOLS
}

pub(crate) fn openai_provider_canary_profiles(
    config: &OpenAiProviderCanaryConfig,
) -> Vec<ModelProfileRefV1> {
    openai_provider_canary_protocols()
        .iter()
        .copied()
        .map(|protocol| config.profile_for_protocol(protocol))
        .collect()
}

pub async fn run_openai_provider_canaries(
    config: OpenAiProviderCanaryConfig,
    credential_resolver: Arc<dyn CredentialResolver>,
) -> Vec<ModelProviderCanaryReport> {
    let profiles = openai_provider_canary_profiles(&config);
    if !config.enabled {
        return profiles
            .iter()
            .map(|profile| {
                ModelProviderCanaryReport::skipped(
                    profile.api_protocol,
                    &config,
                    "disabled: set MUZEN_RUN_REAL_PROVIDER_CANARY=1 to run live provider canaries",
                )
            })
            .collect();
    }
    if credential_resolver
        .resolve_credential(&config.credential_ref)
        .is_err()
    {
        return profiles
            .iter()
            .map(|profile| {
                ModelProviderCanaryReport::skipped(
                    profile.api_protocol,
                    &config,
                    "credential unavailable",
                )
            })
            .collect();
    }
    let registry = match ToolRegistry::review_defaults() {
        Ok(registry) => Arc::new(registry),
        Err(error) => {
            return profiles
                .iter()
                .map(|profile| {
                    ModelProviderCanaryReport::failed(
                        profile.api_protocol,
                        &config,
                        format!("failed to build tool registry: {error}"),
                    )
                })
                .collect();
        }
    };
    let limiter = Arc::new(ModelLimiter::new_with_buckets(
        profiles.len().max(1),
        profiles.len().max(1),
        profiles.len().max(1),
        profiles.len().max(1),
        1,
    ));
    let reviewer_policy = Arc::new(ReviewerPolicy::new());
    let mut reports = Vec::with_capacity(profiles.len());
    for profile in profiles {
        reports.push(
            run_openai_provider_canary(
                profile,
                &config,
                Arc::clone(&credential_resolver),
                Arc::clone(&registry),
                Arc::clone(&reviewer_policy),
                Arc::clone(&limiter),
            )
            .await,
        );
    }
    reports
}

async fn run_openai_provider_canary(
    profile: ModelProfileRefV1,
    config: &OpenAiProviderCanaryConfig,
    credential_resolver: Arc<dyn CredentialResolver>,
    registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    limiter: Arc<ModelLimiter>,
) -> ModelProviderCanaryReport {
    let protocol = profile.api_protocol;
    let scope = openai_provider_canary_scope(profile.id.clone(), config.max_output_tokens);
    let client = match openai_client_from_profile(
        profile,
        config.base_url.clone(),
        limiter,
        registry,
        reviewer_policy,
        credential_resolver,
    ) {
        Ok(client) => client,
        Err(error) => {
            return ModelProviderCanaryReport::failed(protocol, config, error.to_string());
        }
    };
    let transcript = [ConversationItem::User {
        content: config.prompt.clone(),
    }];
    match client
        .complete(&scope, &transcript, TurnId(0), CancellationToken::new())
        .await
    {
        Ok(turn) if model_turn_has_payload(&turn) => {
            ModelProviderCanaryReport::passed(protocol, config)
        }
        Ok(_) => ModelProviderCanaryReport::failed(protocol, config, "empty model response"),
        Err(error) => ModelProviderCanaryReport::failed(protocol, config, error.to_string()),
    }
}

fn openai_provider_canary_scope(profile_id: String, max_output_tokens: u32) -> SessionScope {
    SessionScope {
        id: SessionId(format!("real-provider-canary-{profile_id}")),
        role: Role::Generalist,
        objective: "real-provider protocol canary".to_string(),
        instructions: Vec::new(),
        snapshot_id: None,
        model_profile_id: Some(profile_id),
        response_format: None,
        capabilities: CapabilitySet::review_read_only(),
        budget: AgentBudget {
            max_turns: 1,
            max_tool_calls: 1,
            max_prompt_tokens: OPENAI_PROVIDER_CANARY_MAX_INPUT_TOKENS as u64,
            max_output_tokens: max_output_tokens as u64,
            budget_source: crate::contracts::BudgetSource::RunReserve,
        },
    }
}

fn model_turn_has_payload(turn: &ModelTurn) -> bool {
    match turn {
        ModelTurn::Text { content, .. } => !content.trim().is_empty(),
        ModelTurn::ToolCalls { calls, .. } => !calls.is_empty(),
    }
}

fn openai_provider_canary_protocol_slug(protocol: ModelApiProtocol) -> &'static str {
    match protocol {
        ModelApiProtocol::Responses => "responses",
        ModelApiProtocol::Messages => "messages",
    }
}

#[derive(Debug)]
pub struct OpenAiResponsesClient {
    http: reqwest::Client,
    profile: ModelProfileRefV1,
    api_key: String,
    base_url: String,
    limiter: Arc<ModelLimiter>,
    tool_registry: Arc<ToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
}

impl OpenAiResponsesClient {
    pub(crate) fn from_profile(
        profile: ModelProfileRefV1,
        base_url: String,
        limiter: Arc<ModelLimiter>,
        tool_registry: Arc<ToolRegistry>,
        reviewer_policy: Arc<ReviewerPolicy>,
        credential_resolver: Arc<dyn CredentialResolver>,
    ) -> RuntimeResult<Self> {
        let api_key = credential_resolver.resolve_credential(&profile.credential_ref)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|_| RuntimeError::Invariant("failed to build async HTTP client"))?;
        Ok(Self {
            http,
            profile,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            limiter,
            tool_registry,
            reviewer_policy,
        })
    }
}

#[async_trait]
impl ConcurrentModelClient for OpenAiResponsesClient {
    async fn complete(
        &self,
        scope: &SessionScope,
        transcript: &[ConversationItem],
        _turn_id: TurnId,
        cancel: CancellationToken,
    ) -> RuntimeResult<ModelTurn> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        let _permit = self
            .limiter
            .acquire_for_model(
                "openai_compatible",
                &self.profile.id,
                &self.profile.credential_ref,
                &scope.id,
            )
            .await?;
        let body = responses_request_body(
            &self.profile,
            &self.reviewer_policy,
            &self.tool_registry,
            scope,
            transcript,
        )?;
        let response = self
            .http
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| RuntimeError::Provider {
                status: None,
                retryable: true,
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(provider_http_error(status, response).await);
        }
        let decoded: ResponsesResponse =
            response.json().await.map_err(|_| RuntimeError::Provider {
                status: None,
                retryable: false,
            })?;
        parse_responses_response(decoded, &self.tool_registry)
    }
}

pub(crate) async fn provider_http_error(
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> RuntimeError {
    let message = response
        .text()
        .await
        .map(provider_error_message)
        .unwrap_or_else(|_| "provider error body unavailable".to_string());
    let retryable = (status.as_u16() == 429 && !is_non_retryable_provider_quota_error(&message))
        || status.is_server_error();
    RuntimeError::ProviderMessage {
        status: Some(status.as_u16()),
        retryable,
        message,
    }
}

pub(crate) fn provider_error_message(body: String) -> String {
    let redacted = redact_known_secrets(body.trim(), &[]);
    truncate_chars(&redacted, 1_000)
}

fn is_non_retryable_provider_quota_error(message: &str) -> bool {
    message.contains("\"code\":\"insufficient_quota\"")
        || message.contains("\"code\": \"insufficient_quota\"")
        || message.contains("insufficient_quota")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn responses_request_body(
    profile: &ModelProfileRefV1,
    reviewer_policy: &ReviewerPolicy,
    tool_registry: &ToolRegistry,
    scope: &SessionScope,
    transcript: &[ConversationItem],
) -> RuntimeResult<Value> {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for item in transcript {
        match item {
            ConversationItem::System { content } => instructions.push(content.as_str()),
            ConversationItem::User { content } => {
                input.push(response_message("user", content));
            }
            ConversationItem::AssistantText { content } => {
                input.push(response_message("assistant", content));
            }
            ConversationItem::AssistantToolCalls { calls } => {
                for call in calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.call_id.0,
                        "name": model_alias_for_tool(tool_registry, &call.name)?.as_str(),
                        "arguments": call.raw_arguments,
                    }));
                }
            }
            ConversationItem::ToolResult {
                call_id, content, ..
            } => {
                let compact = reviewer_policy.compact_tool_result(content, &scope.capabilities);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id.0,
                    "output": serde_json::to_string(&compact)
                        .map_err(|_| RuntimeError::Invariant("tool result serialization failed"))?,
                }));
            }
        }
    }
    if input.is_empty() {
        input.push(response_message("user", "Begin the task."));
    }

    let mut body = json!({
        "model": profile.model.as_str(),
        "input": input,
        "tools": openai_response_tools(
            reviewer_policy,
            tool_registry,
            transcript,
            &scope.capabilities,
        )?,
        "parallel_tool_calls": false,
        "tool_choice": "auto",
        "max_output_tokens": profile.max_output_tokens,
    });
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions.join("\n\n"));
    }
    if !(profile.model.starts_with("gpt-5") || profile.model.starts_with('o')) {
        body["temperature"] = json!(profile.temperature.unwrap_or(0.0));
    }
    if let Some(top_p) = profile.top_p {
        body["top_p"] = json!(top_p);
    }
    if let Some(response_format) = &scope.response_format {
        body["text"] = json!({
            "format": responses_json_schema_text_format(response_format),
        });
    }
    Ok(body)
}

fn responses_json_schema_text_format(response_format: &ModelResponseFormat) -> Value {
    json!({
        "type": "json_schema",
        "name": response_format.name.as_str(),
        "strict": response_format.strict,
        "schema": response_format.schema.clone(),
    })
}

fn response_message(role: &str, content: &str) -> Value {
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{
            "type": content_type,
            "text": content,
        }],
    })
}

fn openai_response_tools(
    reviewer_policy: &ReviewerPolicy,
    tool_registry: &ToolRegistry,
    transcript: &[ConversationItem],
    capabilities: &CapabilitySet,
) -> RuntimeResult<Vec<Value>> {
    let tools =
        reviewer_policy.tool_schemas_for_transcript(tool_registry, transcript, capabilities);
    tools
        .into_iter()
        .map(response_tool_from_chat_tool)
        .collect::<RuntimeResult<Vec<_>>>()
}

fn response_tool_from_chat_tool(tool: Value) -> RuntimeResult<Value> {
    let function = tool
        .get("function")
        .and_then(Value::as_object)
        .ok_or(RuntimeError::Invariant("chat tool schema missing function"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or(RuntimeError::Invariant("chat tool schema missing name"))?;
    let description =
        function
            .get("description")
            .and_then(Value::as_str)
            .ok_or(RuntimeError::Invariant(
                "chat tool schema missing description",
            ))?;
    let parameters = function
        .get("parameters")
        .cloned()
        .ok_or(RuntimeError::Invariant(
            "chat tool schema missing parameters",
        ))?;
    Ok(json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters,
        "strict": true,
    }))
}

pub(crate) fn model_alias_for_tool(
    tool_registry: &ToolRegistry,
    tool_id: &ToolId,
) -> RuntimeResult<ToolId> {
    tool_registry
        .alias_table()?
        .alias_for(tool_id)
        .cloned()
        .ok_or(RuntimeError::Invariant("missing model alias for tool"))
}

fn parse_responses_response(
    response: ResponsesResponse,
    tool_registry: &ToolRegistry,
) -> RuntimeResult<ModelTurn> {
    let usage = response.usage.unwrap_or_default().into_token_usage();
    let mut seen = HashSet::new();
    let mut calls = Vec::new();
    let mut text = String::new();
    for (index, item) in response.output.into_iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str);
        match item_type {
            Some("function_call") => {
                let call_id =
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .ok_or(RuntimeError::Provider {
                            status: None,
                            retryable: false,
                        })?;
                if !seen.insert(call_id.to_string()) {
                    return Err(RuntimeError::Provider {
                        status: None,
                        retryable: false,
                    });
                }
                let alias = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(RuntimeError::Provider {
                        status: None,
                        retryable: false,
                    })
                    .and_then(ToolId::parse)?;
                let Some(name) = tool_registry.tool_id_for_model_alias(&alias) else {
                    return Err(RuntimeError::InvalidInput("unknown tool name".to_string()));
                };
                let raw_arguments = item.get("arguments").and_then(Value::as_str).ok_or(
                    RuntimeError::Provider {
                        status: None,
                        retryable: false,
                    },
                )?;
                calls.push(ModelToolCall {
                    call_id: ToolCallId(call_id.to_string()),
                    index,
                    name,
                    raw_arguments: raw_arguments.to_string(),
                });
            }
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("output_text" | "text")
                        ) {
                            if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                                text.push_str(part_text);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !calls.is_empty() {
        Ok(ModelTurn::ToolCalls { calls, usage })
    } else {
        Ok(ModelTurn::Text {
            content: response.output_text.unwrap_or(text),
            usage,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    output: Vec<Value>,
    output_text: Option<String>,
    usage: Option<ResponsesUsage>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ResponsesUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl ResponsesUsage {
    fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.unwrap_or(0),
            output_tokens: self.output_tokens.unwrap_or(0),
            total_tokens: self.total_tokens.unwrap_or(0),
            cached_input_tokens: 0,
        }
    }
}

#[cfg(test)]
mod tests;
