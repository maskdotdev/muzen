use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::reviewer_kernel::kernel_types::{RuntimeError, RuntimeResult};

use crate::reviewer_kernel::review_contract::ModelApiProtocol;
use crate::reviewer_kernel::system::timestamp_utc;

pub const MODEL_PROVIDER_CANARY_EVIDENCE_SCHEMA_VERSION: &str = "muzen.model-provider-canary.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProviderCanaryReport {
    pub protocol: ModelApiProtocol,
    pub base_url: String,
    pub model: String,
    pub credential_ref: String,
    pub status: ModelProviderCanaryStatus,
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

fn openai_provider_canary_protocol_slug(protocol: ModelApiProtocol) -> &'static str {
    match protocol {
        ModelApiProtocol::Responses => "responses",
        ModelApiProtocol::Messages => "messages",
    }
}

pub const CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION: &str = "muzen.canary-evidence-manifest.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryEvidenceManifest {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub model_provider: Option<ModelProviderCanaryEvidence>,
    pub gate: CanaryEvidenceGate,
}

impl CanaryEvidenceManifest {
    pub fn from_evidence(model_provider: Option<ModelProviderCanaryEvidence>) -> Self {
        Self::with_generated_at(timestamp_utc(), model_provider)
    }

    pub fn with_generated_at(
        generated_at_utc: impl Into<String>,
        model_provider: Option<ModelProviderCanaryEvidence>,
    ) -> Self {
        let gate = CanaryEvidenceGate::evaluate(&model_provider);
        Self {
            schema_version: CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION.to_string(),
            generated_at_utc: generated_at_utc.into(),
            model_provider,
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
        let evaluated = CanaryEvidenceGate::evaluate(&self.model_provider);
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
}

impl CanaryEvidenceStatusSummary {
    fn from_manifest(manifest: &CanaryEvidenceManifest) -> Self {
        Self {
            model_provider: CanaryModelProviderEvidenceStatus::from_evidence(
                manifest.model_provider.as_ref(),
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
pub struct CanaryEvidenceGate {
    pub valid: bool,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
}

impl CanaryEvidenceGate {
    fn evaluate(model_provider: &Option<ModelProviderCanaryEvidence>) -> Self {
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
