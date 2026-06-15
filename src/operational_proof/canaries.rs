use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::kernel_types::{
    CapabilitySet, ConversationItem, ModelTurn, RuntimeError, RuntimeResult, SessionId,
    SessionScope, TurnId,
};
use crate::reviewer_kernel::model::{
    ConcurrentModelRouter, CredentialResolver, ModelLimiter, ProfileModelRouter,
};
use crate::reviewer_kernel::policy::ReviewerPolicy;

#[cfg(test)]
use crate::reviewer_kernel::kernel_types::stable_id;
use crate::reviewer_kernel::review_contract::{
    AgentBudget, BudgetSource, ModelApiProtocol, ModelProfileRefV1, ProviderKind, Role,
};
use crate::reviewer_kernel::system::timestamp_utc;
use crate::reviewer_kernel::tool_engine::ToolRegistry;
#[cfg(test)]
use crate::workspace::remote_content_addressed_uri;

#[cfg(test)]
use crate::reviewer_kernel::artifacts::{
    normalize_remote_store_base_uri, remote_artifact_object_uri, ArtifactViewMode,
    RemoteArtifactObjectClient,
};
#[cfg(test)]
use crate::reviewer_kernel::snapshots::{snapshot_content_hash, RemoteSnapshotObjectClient};

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
    let default_profile_id = profiles
        .first()
        .map(|profile| profile.id.clone())
        .unwrap_or_else(|| "real-provider-canary".to_string());
    let router = match ProfileModelRouter::from_profiles(
        &profiles,
        default_profile_id,
        config.base_url.clone(),
        limiter,
        registry,
        reviewer_policy,
        credential_resolver,
    ) {
        Ok(router) => router,
        Err(error) => {
            return profiles
                .iter()
                .map(|profile| {
                    ModelProviderCanaryReport::failed(
                        profile.api_protocol,
                        &config,
                        error.to_string(),
                    )
                })
                .collect();
        }
    };
    let mut reports = Vec::with_capacity(profiles.len());
    for profile in profiles {
        reports.push(run_openai_provider_canary(&router, profile, &config).await);
    }
    reports
}

async fn run_openai_provider_canary(
    router: &ProfileModelRouter,
    profile: ModelProfileRefV1,
    config: &OpenAiProviderCanaryConfig,
) -> ModelProviderCanaryReport {
    let protocol = profile.api_protocol;
    let scope = openai_provider_canary_scope(profile.id.clone(), config.max_output_tokens);
    let client = match router.client_for(&scope).await {
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
            budget_source: BudgetSource::RunReserve,
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

pub const REMOTE_OBJECT_STORE_CANARY_SCHEMA_VERSION: &str = "muzen.remote-object-store-canary.v1";

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteObjectStoreCanaryTarget {
    Snapshot,
    Artifact,
}

impl RemoteObjectStoreCanaryTarget {
    #[cfg(test)]
    fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Artifact => "artifact",
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteObjectStoreCanaryStepKind {
    Put,
    ReadAfterPut,
    Remove,
    ReadAfterRemove,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteObjectStoreCanaryStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObjectStoreCanaryStep {
    pub step: RemoteObjectStoreCanaryStepKind,
    pub status: RemoteObjectStoreCanaryStatus,
    pub uri: Option<String>,
    pub bytes: Option<usize>,
    pub message: Option<String>,
}

impl RemoteObjectStoreCanaryStep {
    #[cfg(test)]
    fn passed(step: RemoteObjectStoreCanaryStepKind, uri: &str, bytes: usize) -> Self {
        Self {
            step,
            status: RemoteObjectStoreCanaryStatus::Passed,
            uri: Some(uri.to_string()),
            bytes: Some(bytes),
            message: None,
        }
    }

    #[cfg(test)]
    fn failed(step: RemoteObjectStoreCanaryStepKind, uri: Option<&str>, message: String) -> Self {
        Self {
            step,
            status: RemoteObjectStoreCanaryStatus::Failed,
            uri: uri.map(ToString::to_string),
            bytes: None,
            message: Some(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObjectStoreCanaryGate {
    pub valid: bool,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
}

impl RemoteObjectStoreCanaryGate {
    fn evaluate(
        cleanup_supported: bool,
        steps: &[RemoteObjectStoreCanaryStep],
    ) -> RemoteObjectStoreCanaryGate {
        let expected_steps = [
            RemoteObjectStoreCanaryStepKind::Put,
            RemoteObjectStoreCanaryStepKind::ReadAfterPut,
            RemoteObjectStoreCanaryStepKind::Remove,
            RemoteObjectStoreCanaryStepKind::ReadAfterRemove,
        ];
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut failures = Vec::new();
        for expected_step in expected_steps {
            let count = steps
                .iter()
                .filter(|step| step.step == expected_step)
                .count();
            match count {
                0 => failures.push(format!("missing {expected_step:?} canary step")),
                1 => {}
                count => {
                    failures.push(format!("duplicate {expected_step:?} canary steps: {count}"))
                }
            }
        }
        for step in steps {
            match step.status {
                RemoteObjectStoreCanaryStatus::Passed => passed += 1,
                RemoteObjectStoreCanaryStatus::Failed => {
                    failed += 1;
                    failures.push(format!(
                        "{:?} failed: {}",
                        step.step,
                        step.message
                            .as_deref()
                            .unwrap_or("remote object-store canary step failed")
                    ));
                }
                RemoteObjectStoreCanaryStatus::Skipped => {
                    skipped += 1;
                    if cleanup_supported
                        || !matches!(
                            step.step,
                            RemoteObjectStoreCanaryStepKind::Remove
                                | RemoteObjectStoreCanaryStepKind::ReadAfterRemove
                        )
                    {
                        failures.push(format!(
                            "{:?} skipped: {}",
                            step.step,
                            step.message
                                .as_deref()
                                .unwrap_or("remote object-store canary step skipped")
                        ));
                    }
                }
            }
        }
        Self {
            valid: failures.is_empty() && failed == 0,
            passed,
            failed,
            skipped,
            failures,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObjectStoreCanaryEvidence {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub target: RemoteObjectStoreCanaryTarget,
    pub base_uri: String,
    pub object_uri: Option<String>,
    pub payload_bytes: usize,
    pub payload_hash: String,
    pub cleanup_supported: bool,
    pub steps: Vec<RemoteObjectStoreCanaryStep>,
    pub gate: RemoteObjectStoreCanaryGate,
}

#[cfg(test)]
struct RemoteObjectStoreCanaryEvidenceParts {
    generated_at_utc: String,
    target: RemoteObjectStoreCanaryTarget,
    base_uri: String,
    object_uri: Option<String>,
    payload_bytes: usize,
    payload_hash: String,
    cleanup_supported: bool,
    steps: Vec<RemoteObjectStoreCanaryStep>,
}

impl RemoteObjectStoreCanaryEvidence {
    #[cfg(test)]
    fn from_parts(parts: RemoteObjectStoreCanaryEvidenceParts) -> Self {
        let gate = RemoteObjectStoreCanaryGate::evaluate(parts.cleanup_supported, &parts.steps);
        Self {
            schema_version: REMOTE_OBJECT_STORE_CANARY_SCHEMA_VERSION.to_string(),
            generated_at_utc: parts.generated_at_utc,
            target: parts.target,
            base_uri: parts.base_uri,
            object_uri: parts.object_uri,
            payload_bytes: parts.payload_bytes,
            payload_hash: parts.payload_hash,
            cleanup_supported: parts.cleanup_supported,
            steps: parts.steps,
            gate,
        }
    }

    pub fn require_passed(&self) -> RuntimeResult<()> {
        let failures = self.validation_failures();
        if failures.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::InvalidInput(format!(
            "remote object-store canary gate failed: {}",
            failures.join("; ")
        )))
    }

    fn validation_failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != REMOTE_OBJECT_STORE_CANARY_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported remote object-store canary schema {}",
                self.schema_version
            ));
        }
        let evaluated = RemoteObjectStoreCanaryGate::evaluate(self.cleanup_supported, &self.steps);
        if evaluated != self.gate {
            failures
                .push("stored remote object-store canary gate does not match steps".to_string());
        }
        failures.extend(evaluated.failures);
        failures
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObjectStoreCanaryEvidenceExport {
    pub path: PathBuf,
    pub bytes: usize,
    pub valid: bool,
    pub failures: Vec<String>,
}

#[cfg(test)]
pub fn run_remote_snapshot_object_store_canary(
    base_uri: impl Into<String>,
    client: &dyn RemoteSnapshotObjectClient,
) -> RemoteObjectStoreCanaryEvidence {
    let generated_at_utc = timestamp_utc();
    let base_uri = base_uri.into();
    let payload = remote_object_store_canary_payload(
        RemoteObjectStoreCanaryTarget::Snapshot,
        &base_uri,
        &generated_at_utc,
    );
    let payload_hash = snapshot_content_hash(&payload);
    let mut steps = Vec::new();
    let object_uri = match normalize_remote_store_base_uri(base_uri.clone(), "snapshot")
        .and_then(|base_uri| remote_content_addressed_uri(&base_uri, &payload_hash))
    {
        Ok(uri) => Some(uri),
        Err(error) => {
            steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::Put,
                None,
                error.to_string(),
            ));
            None
        }
    };
    if let Some(uri) = object_uri.as_deref() {
        match client.put_remote_snapshot_object(uri, payload.clone()) {
            Ok(()) => steps.push(RemoteObjectStoreCanaryStep::passed(
                RemoteObjectStoreCanaryStepKind::Put,
                uri,
                payload.len(),
            )),
            Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::Put,
                Some(uri),
                error.to_string(),
            )),
        }
        push_remote_snapshot_read_step(client, uri, &payload, &mut steps);
        match client.remove_remote_snapshot_object(uri) {
            Ok(true) => steps.push(RemoteObjectStoreCanaryStep::passed(
                RemoteObjectStoreCanaryStepKind::Remove,
                uri,
                0,
            )),
            Ok(false) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::Remove,
                Some(uri),
                "remove returned false".to_string(),
            )),
            Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::Remove,
                Some(uri),
                error.to_string(),
            )),
        }
        match client.read_remote_snapshot_object(uri) {
            Ok(None) => steps.push(RemoteObjectStoreCanaryStep::passed(
                RemoteObjectStoreCanaryStepKind::ReadAfterRemove,
                uri,
                0,
            )),
            Ok(Some(bytes)) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::ReadAfterRemove,
                Some(uri),
                format!("read returned {} bytes after remove", bytes.len()),
            )),
            Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::ReadAfterRemove,
                Some(uri),
                error.to_string(),
            )),
        }
    }
    RemoteObjectStoreCanaryEvidence::from_parts(RemoteObjectStoreCanaryEvidenceParts {
        generated_at_utc,
        target: RemoteObjectStoreCanaryTarget::Snapshot,
        base_uri,
        object_uri,
        payload_bytes: payload.len(),
        payload_hash,
        cleanup_supported: true,
        steps,
    })
}

#[cfg(test)]
pub fn run_remote_artifact_object_store_canary(
    base_uri: impl Into<String>,
    client: &dyn RemoteArtifactObjectClient,
) -> RemoteObjectStoreCanaryEvidence {
    let generated_at_utc = timestamp_utc();
    let base_uri = base_uri.into();
    let payload = remote_object_store_canary_payload(
        RemoteObjectStoreCanaryTarget::Artifact,
        &base_uri,
        &generated_at_utc,
    );
    let payload_content = String::from_utf8(payload.clone())
        .expect("remote object-store canary payload is valid UTF-8");
    let payload_hash = stable_id(&[&payload_content]);
    let mut steps = Vec::new();
    let object_uri =
        match normalize_remote_store_base_uri(base_uri.clone(), "artifact").and_then(|base_uri| {
            remote_artifact_object_uri(&base_uri, ArtifactViewMode::Redacted, &payload_hash)
        }) {
            Ok(uri) => Some(uri),
            Err(error) => {
                steps.push(RemoteObjectStoreCanaryStep::failed(
                    RemoteObjectStoreCanaryStepKind::Put,
                    None,
                    error.to_string(),
                ));
                None
            }
        };
    if let Some(uri) = object_uri.as_deref() {
        match client.put_remote_artifact_object(uri, payload.clone()) {
            Ok(()) => steps.push(RemoteObjectStoreCanaryStep::passed(
                RemoteObjectStoreCanaryStepKind::Put,
                uri,
                payload.len(),
            )),
            Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::Put,
                Some(uri),
                error.to_string(),
            )),
        }
        match client.read_remote_artifact_object(uri) {
            Ok(Some(bytes)) if bytes == payload => {
                steps.push(RemoteObjectStoreCanaryStep::passed(
                    RemoteObjectStoreCanaryStepKind::ReadAfterPut,
                    uri,
                    bytes.len(),
                ));
            }
            Ok(Some(bytes)) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::ReadAfterPut,
                Some(uri),
                format!("read returned {} stale bytes", bytes.len()),
            )),
            Ok(None) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::ReadAfterPut,
                Some(uri),
                "read returned missing object".to_string(),
            )),
            Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::ReadAfterPut,
                Some(uri),
                error.to_string(),
            )),
        }
        match client.remove_remote_artifact_object(uri) {
            Ok(true) => steps.push(RemoteObjectStoreCanaryStep::passed(
                RemoteObjectStoreCanaryStepKind::Remove,
                uri,
                0,
            )),
            Ok(false) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::Remove,
                Some(uri),
                "remove returned false".to_string(),
            )),
            Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::Remove,
                Some(uri),
                error.to_string(),
            )),
        }
        match client.read_remote_artifact_object(uri) {
            Ok(None) => steps.push(RemoteObjectStoreCanaryStep::passed(
                RemoteObjectStoreCanaryStepKind::ReadAfterRemove,
                uri,
                0,
            )),
            Ok(Some(bytes)) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::ReadAfterRemove,
                Some(uri),
                format!("read returned {} bytes after remove", bytes.len()),
            )),
            Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
                RemoteObjectStoreCanaryStepKind::ReadAfterRemove,
                Some(uri),
                error.to_string(),
            )),
        }
    }
    RemoteObjectStoreCanaryEvidence::from_parts(RemoteObjectStoreCanaryEvidenceParts {
        generated_at_utc,
        target: RemoteObjectStoreCanaryTarget::Artifact,
        base_uri,
        object_uri,
        payload_bytes: payload.len(),
        payload_hash,
        cleanup_supported: true,
        steps,
    })
}

#[cfg(test)]
pub fn export_remote_object_store_canary_evidence(
    path: impl AsRef<Path>,
    evidence: &RemoteObjectStoreCanaryEvidence,
) -> RuntimeResult<RemoteObjectStoreCanaryEvidenceExport> {
    let path = path.as_ref();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::RepoUnavailable(format!(
                "failed to create remote object-store canary evidence directory: {error}"
            ))
        })?;
    }
    let mut bytes = serde_json::to_vec_pretty(evidence).map_err(|_| {
        RuntimeError::Invariant("remote object-store canary evidence serialization failed")
    })?;
    bytes.push(b'\n');
    let failures = evidence.validation_failures();
    let temp_path = remote_object_store_canary_evidence_temp_path(path)?;
    fs::write(&temp_path, &bytes).map_err(|error| {
        RuntimeError::RepoUnavailable(format!(
            "failed to write remote object-store canary evidence: {error}"
        ))
    })?;
    fs::rename(&temp_path, path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        RuntimeError::RepoUnavailable(format!(
            "failed to publish remote object-store canary evidence: {error}"
        ))
    })?;
    Ok(RemoteObjectStoreCanaryEvidenceExport {
        path: path.to_path_buf(),
        bytes: bytes.len(),
        valid: failures.is_empty(),
        failures,
    })
}

pub fn load_remote_object_store_canary_evidence(
    path: impl AsRef<Path>,
) -> RuntimeResult<RemoteObjectStoreCanaryEvidence> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!(
            "failed to read remote object-store canary evidence {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_str(&contents).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "invalid remote object-store canary evidence {}: {error}",
            path.display()
        ))
    })
}

pub const CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION: &str = "muzen.canary-evidence-manifest.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryEvidenceManifest {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub model_provider: Option<ModelProviderCanaryEvidence>,
    pub remote_object_stores: Vec<RemoteObjectStoreCanaryEvidence>,
    pub gate: CanaryEvidenceGate,
}

impl CanaryEvidenceManifest {
    pub fn from_evidence(
        model_provider: Option<ModelProviderCanaryEvidence>,
        remote_object_stores: Vec<RemoteObjectStoreCanaryEvidence>,
    ) -> Self {
        Self::with_generated_at(timestamp_utc(), model_provider, remote_object_stores)
    }

    pub fn with_generated_at(
        generated_at_utc: impl Into<String>,
        model_provider: Option<ModelProviderCanaryEvidence>,
        remote_object_stores: Vec<RemoteObjectStoreCanaryEvidence>,
    ) -> Self {
        let gate = CanaryEvidenceGate::evaluate(&model_provider, &remote_object_stores);
        Self {
            schema_version: CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION.to_string(),
            generated_at_utc: generated_at_utc.into(),
            model_provider,
            remote_object_stores,
            gate,
        }
    }

    #[cfg(test)]
    pub fn require_passed(&self) -> RuntimeResult<()> {
        let failures = self.validation_failures();
        if failures.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::InvalidInput(format!(
            "canary evidence manifest gate failed: {}",
            failures.join("; ")
        )))
    }

    pub fn require_passed_with_freshness(
        &self,
        policy: &CanaryEvidenceFreshnessPolicy,
    ) -> RuntimeResult<()> {
        let report = self.status_report(policy);
        let failures = report.failures();
        if failures.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::InvalidInput(format!(
            "canary evidence manifest gate failed: {}",
            failures.join("; ")
        )))
    }

    pub fn status_report(
        &self,
        policy: &CanaryEvidenceFreshnessPolicy,
    ) -> CanaryEvidenceStatusReport {
        let validation_failures = self.validation_failures();
        let freshness_failures = self.freshness_failures(policy);
        CanaryEvidenceStatusReport {
            ok: validation_failures.is_empty() && freshness_failures.is_empty(),
            manifest_schema_version: self.schema_version.clone(),
            generated_at_utc: self.generated_at_utc.clone(),
            freshness_checked_at_utc: policy.now_utc.clone(),
            max_evidence_age_seconds: policy.max_age_seconds,
            evidence: CanaryEvidenceStatusSummary::from_manifest(self),
            gate: self.gate.clone(),
            validation_failures,
            freshness_failures,
        }
    }

    fn validation_failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported canary evidence manifest schema {}",
                self.schema_version
            ));
        }
        let evaluated =
            CanaryEvidenceGate::evaluate(&self.model_provider, &self.remote_object_stores);
        if evaluated != self.gate {
            failures
                .push("stored canary evidence manifest gate does not match evidence".to_string());
        }
        failures.extend(evaluated.failures);
        failures
    }

    fn freshness_failures(&self, policy: &CanaryEvidenceFreshnessPolicy) -> Vec<String> {
        let mut failures = Vec::new();
        push_timestamp_freshness_failure(
            &mut failures,
            "canary evidence manifest",
            &self.generated_at_utc,
            policy,
        );
        if let Some(evidence) = &self.model_provider {
            push_timestamp_freshness_failure(
                &mut failures,
                "model provider canary evidence",
                &evidence.generated_at_utc,
                policy,
            );
        }
        for evidence in &self.remote_object_stores {
            push_timestamp_freshness_failure(
                &mut failures,
                &format!(
                    "{} remote object-store canary evidence",
                    remote_object_store_canary_target_name(evidence.target)
                ),
                &evidence.generated_at_utc,
                policy,
            );
        }
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryEvidenceStatusReport {
    pub ok: bool,
    pub manifest_schema_version: String,
    pub generated_at_utc: String,
    pub freshness_checked_at_utc: String,
    pub max_evidence_age_seconds: u64,
    pub evidence: CanaryEvidenceStatusSummary,
    pub gate: CanaryEvidenceGate,
    pub validation_failures: Vec<String>,
    pub freshness_failures: Vec<String>,
}

impl CanaryEvidenceStatusReport {
    pub fn failures(&self) -> Vec<String> {
        let mut failures =
            Vec::with_capacity(self.validation_failures.len() + self.freshness_failures.len());
        failures.extend(self.validation_failures.iter().cloned());
        failures.extend(self.freshness_failures.iter().cloned());
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryEvidenceStatusSummary {
    pub model_provider: CanaryModelProviderEvidenceStatus,
    pub remote_object_stores: Vec<CanaryRemoteObjectStoreEvidenceStatus>,
}

impl CanaryEvidenceStatusSummary {
    fn from_manifest(manifest: &CanaryEvidenceManifest) -> Self {
        Self {
            model_provider: CanaryModelProviderEvidenceStatus::from_evidence(
                manifest.model_provider.as_ref(),
            ),
            remote_object_stores: CanaryRemoteObjectStoreEvidenceStatus::from_evidence(
                &manifest.remote_object_stores,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryModelProviderEvidenceStatus {
    pub present: bool,
    pub generated_at_utc: Option<String>,
    pub required_protocols: Vec<ModelApiProtocol>,
    pub reported_protocols: Vec<ModelApiProtocol>,
    pub passed_protocols: Vec<ModelApiProtocol>,
    pub gate: Option<ModelProviderCanaryGate>,
}

impl CanaryModelProviderEvidenceStatus {
    fn from_evidence(evidence: Option<&ModelProviderCanaryEvidence>) -> Self {
        let required_protocols = openai_provider_canary_protocols().to_vec();
        match evidence {
            Some(evidence) => Self {
                present: true,
                generated_at_utc: Some(evidence.generated_at_utc.clone()),
                required_protocols,
                reported_protocols: evidence
                    .reports
                    .iter()
                    .map(|report| report.protocol)
                    .collect(),
                passed_protocols: evidence
                    .reports
                    .iter()
                    .filter(|report| matches!(report.status, ModelProviderCanaryStatus::Passed))
                    .map(|report| report.protocol)
                    .collect(),
                gate: Some(evidence.gate.clone()),
            },
            None => Self {
                present: false,
                generated_at_utc: None,
                required_protocols,
                reported_protocols: Vec::new(),
                passed_protocols: Vec::new(),
                gate: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryRemoteObjectStoreEvidenceStatus {
    pub target: RemoteObjectStoreCanaryTarget,
    pub evidence_count: usize,
    pub generated_at_utc: Option<String>,
    pub base_uri: Option<String>,
    pub object_uri: Option<String>,
    pub gate: Option<RemoteObjectStoreCanaryGate>,
}

impl CanaryRemoteObjectStoreEvidenceStatus {
    fn from_evidence(evidence: &[RemoteObjectStoreCanaryEvidence]) -> Vec<Self> {
        [
            RemoteObjectStoreCanaryTarget::Snapshot,
            RemoteObjectStoreCanaryTarget::Artifact,
        ]
        .into_iter()
        .map(|target| {
            let mut matching = evidence.iter().filter(|evidence| evidence.target == target);
            let first = matching.next();
            let evidence_count = usize::from(first.is_some()) + matching.count();
            Self {
                target,
                evidence_count,
                generated_at_utc: first.map(|evidence| evidence.generated_at_utc.clone()),
                base_uri: first.map(|evidence| evidence.base_uri.clone()),
                object_uri: first.and_then(|evidence| evidence.object_uri.clone()),
                gate: first.map(|evidence| evidence.gate.clone()),
            }
        })
        .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryEvidenceGate {
    pub valid: bool,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
}

impl CanaryEvidenceGate {
    fn evaluate(
        model_provider: &Option<ModelProviderCanaryEvidence>,
        remote_object_stores: &[RemoteObjectStoreCanaryEvidence],
    ) -> Self {
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut failures = Vec::new();

        match model_provider {
            Some(evidence) => {
                passed = passed.saturating_add(evidence.gate.passed);
                failed = failed.saturating_add(evidence.gate.failed);
                skipped = skipped.saturating_add(evidence.gate.skipped);
                if let Err(error) = evidence.require_passed() {
                    failures.push(format!("model provider canary invalid: {error}"));
                }
            }
            None => {
                failed = failed.saturating_add(1);
                failures.push("missing model provider canary evidence".to_string());
            }
        }

        for target in [
            RemoteObjectStoreCanaryTarget::Snapshot,
            RemoteObjectStoreCanaryTarget::Artifact,
        ] {
            let matching = remote_object_stores
                .iter()
                .filter(|evidence| evidence.target == target)
                .collect::<Vec<_>>();
            let target_name = remote_object_store_canary_target_name(target);
            match matching.len() {
                0 => {
                    failed = failed.saturating_add(1);
                    failures.push(format!(
                        "missing {target_name} remote object-store canary evidence"
                    ));
                }
                1 => {}
                count => {
                    failed = failed.saturating_add(count.saturating_sub(1));
                    failures.push(format!(
                        "duplicate {target_name} remote object-store canary evidence: {count}"
                    ));
                }
            }
            for evidence in matching {
                passed = passed.saturating_add(evidence.gate.passed);
                failed = failed.saturating_add(evidence.gate.failed);
                skipped = skipped.saturating_add(evidence.gate.skipped);
                if let Err(error) = evidence.require_passed() {
                    failures.push(format!(
                        "{target_name} remote object-store canary invalid: {error}"
                    ));
                }
            }
        }

        Self {
            valid: failures.is_empty(),
            passed,
            failed,
            skipped,
            failures,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanaryEvidenceFreshnessPolicy {
    pub now_utc: String,
    pub max_age_seconds: u64,
}

impl CanaryEvidenceFreshnessPolicy {
    pub fn current(max_age_seconds: u64) -> Self {
        Self {
            now_utc: timestamp_utc(),
            max_age_seconds,
        }
    }

    #[cfg(test)]
    pub fn at(now_utc: impl Into<String>, max_age_seconds: u64) -> Self {
        Self {
            now_utc: now_utc.into(),
            max_age_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryEvidenceManifestExport {
    pub path: PathBuf,
    pub bytes: usize,
    pub valid: bool,
    pub failures: Vec<String>,
}

pub fn export_canary_evidence_manifest(
    path: impl AsRef<Path>,
    manifest: &CanaryEvidenceManifest,
) -> RuntimeResult<CanaryEvidenceManifestExport> {
    let path = path.as_ref();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::RepoUnavailable(format!(
                "failed to create canary evidence manifest directory: {error}"
            ))
        })?;
    }
    let mut bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|_| RuntimeError::Invariant("canary evidence manifest serialization failed"))?;
    bytes.push(b'\n');
    let failures = manifest.validation_failures();
    let temp_path = canary_evidence_manifest_temp_path(path)?;
    fs::write(&temp_path, &bytes).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to write canary evidence manifest: {error}"))
    })?;
    fs::rename(&temp_path, path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        RuntimeError::RepoUnavailable(format!(
            "failed to publish canary evidence manifest: {error}"
        ))
    })?;
    Ok(CanaryEvidenceManifestExport {
        path: path.to_path_buf(),
        bytes: bytes.len(),
        valid: failures.is_empty(),
        failures,
    })
}

pub fn load_canary_evidence_manifest(
    path: impl AsRef<Path>,
) -> RuntimeResult<CanaryEvidenceManifest> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!(
            "failed to read canary evidence manifest {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_str(&contents).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "invalid canary evidence manifest {}: {error}",
            path.display()
        ))
    })
}

fn remote_object_store_canary_target_name(target: RemoteObjectStoreCanaryTarget) -> &'static str {
    match target {
        RemoteObjectStoreCanaryTarget::Snapshot => "snapshot",
        RemoteObjectStoreCanaryTarget::Artifact => "artifact",
    }
}

fn push_timestamp_freshness_failure(
    failures: &mut Vec<String>,
    label: &str,
    generated_at_utc: &str,
    policy: &CanaryEvidenceFreshnessPolicy,
) {
    let now_seconds = match parse_unix_timestamp_seconds(&policy.now_utc) {
        Ok(seconds) => seconds,
        Err(error) => {
            failures.push(format!("invalid freshness reference time: {error}"));
            return;
        }
    };
    let generated_seconds = match parse_unix_timestamp_seconds(generated_at_utc) {
        Ok(seconds) => seconds,
        Err(error) => {
            failures.push(format!("{label} generatedAtUtc is invalid: {error}"));
            return;
        }
    };
    if generated_seconds > now_seconds {
        failures.push(format!(
            "{label} generatedAtUtc is in the future by {} seconds",
            generated_seconds.saturating_sub(now_seconds)
        ));
        return;
    }
    let age = now_seconds.saturating_sub(generated_seconds);
    if age > policy.max_age_seconds {
        failures.push(format!(
            "{label} is stale: age {age} seconds exceeds max {} seconds",
            policy.max_age_seconds
        ));
    }
}

fn parse_unix_timestamp_seconds(timestamp: &str) -> Result<u64, String> {
    let without_z = timestamp
        .strip_suffix('Z')
        .ok_or_else(|| format!("{timestamp} does not end with Z"))?;
    let seconds = without_z
        .split_once('.')
        .map(|(seconds, _)| seconds)
        .unwrap_or(without_z);
    if seconds.is_empty() {
        return Err(format!("{timestamp} has no seconds"));
    }
    seconds
        .parse::<u64>()
        .map_err(|error| format!("{timestamp} has invalid seconds: {error}"))
}

#[cfg(test)]
fn push_remote_snapshot_read_step(
    client: &dyn RemoteSnapshotObjectClient,
    uri: &str,
    payload: &[u8],
    steps: &mut Vec<RemoteObjectStoreCanaryStep>,
) {
    match client.read_remote_snapshot_object(uri) {
        Ok(Some(bytes)) if bytes == payload => steps.push(RemoteObjectStoreCanaryStep::passed(
            RemoteObjectStoreCanaryStepKind::ReadAfterPut,
            uri,
            bytes.len(),
        )),
        Ok(Some(bytes)) => steps.push(RemoteObjectStoreCanaryStep::failed(
            RemoteObjectStoreCanaryStepKind::ReadAfterPut,
            Some(uri),
            format!("read returned {} stale bytes", bytes.len()),
        )),
        Ok(None) => steps.push(RemoteObjectStoreCanaryStep::failed(
            RemoteObjectStoreCanaryStepKind::ReadAfterPut,
            Some(uri),
            "read returned missing object".to_string(),
        )),
        Err(error) => steps.push(RemoteObjectStoreCanaryStep::failed(
            RemoteObjectStoreCanaryStepKind::ReadAfterPut,
            Some(uri),
            error.to_string(),
        )),
    }
}

#[cfg(test)]
fn remote_object_store_canary_payload(
    target: RemoteObjectStoreCanaryTarget,
    base_uri: &str,
    generated_at_utc: &str,
) -> Vec<u8> {
    format!(
        "muzen remote object-store canary\ntarget={}\nbaseUri={base_uri}\ngeneratedAt={generated_at_utc}\n",
        target.as_str()
    )
    .into_bytes()
}

#[cfg(test)]
fn remote_object_store_canary_evidence_temp_path(path: &Path) -> RuntimeResult<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        RuntimeError::InvalidInput(
            "remote object-store canary evidence path must name a file".to_string(),
        )
    })?;
    Ok(path.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    )))
}

fn canary_evidence_manifest_temp_path(path: &Path) -> RuntimeResult<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        RuntimeError::InvalidInput("canary evidence manifest path must name a file".to_string())
    })?;
    Ok(path.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    )))
}
