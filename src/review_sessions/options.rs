use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context_engine::ContextEngineConfig;
use crate::reviewer_kernel::review_contract::{AgentBudget, Role};
use crate::runner_protocol::{
    RunAgentBudgetParams, RunChangeFileParams, RunChangeParams, RunInstructionParams,
    RunLimitParams, RunModelParams, RunSessionParams, RunSourceProviderParams, RunToolParams,
};
#[cfg(not(test))]
use crate::runner_protocol::{RunModelCredentialParams, RunModelProfileParams};

use super::ReviewSource;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewOptions {
    #[serde(default)]
    pub dedupe: DedupePolicy,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub change: Option<ReviewChangeSpec>,
    #[serde(default)]
    pub scope: ReviewScope,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub instructions: Vec<ReviewInstruction>,
    #[serde(default)]
    pub tools: Vec<ReviewToolOption>,
    #[serde(default)]
    pub sessions: Vec<ReviewAgentSession>,
    #[serde(default)]
    pub limits: Option<ReviewLimits>,
    #[serde(default)]
    pub context_engine: Option<ContextEngineConfig>,
    #[serde(default)]
    pub config_snapshot: Option<EffectiveConfigSnapshot>,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            dedupe: DedupePolicy::None,
            user_id: None,
            model: None,
            change: None,
            scope: ReviewScope::default(),
            metadata: BTreeMap::new(),
            instructions: Vec::new(),
            tools: Vec::new(),
            sessions: Vec::new(),
            limits: None,
            context_engine: None,
            config_snapshot: None,
        }
    }
}

impl ReviewOptions {
    pub(crate) fn runner_sessions(&self) -> Vec<RunSessionParams> {
        self.sessions
            .iter()
            .map(|session| session.to_runner_session(self.model.as_deref()))
            .collect()
    }

    pub(crate) fn runner_instructions(&self) -> Vec<RunInstructionParams> {
        self.instructions
            .iter()
            .map(ReviewInstruction::to_runner_instruction)
            .collect()
    }

    pub(crate) fn runner_tools(&self) -> Vec<RunToolParams> {
        self.tools
            .iter()
            .map(ReviewToolOption::to_runner_tool)
            .collect()
    }

    pub(crate) fn runner_change(&self) -> Option<RunChangeParams> {
        self.change.as_ref().map(ReviewChangeSpec::to_runner_change)
    }

    pub(crate) fn runner_changed_files(&self, source: &ReviewSource) -> Vec<String> {
        if !self.scope.files.is_empty() {
            return self.scope.files.clone();
        }
        if let Some(change) = &self.change {
            let changed_files = change.changed_file_paths();
            if !changed_files.is_empty() {
                return changed_files;
            }
        }
        source.runner_changed_files(&self.scope)
    }

    pub(crate) fn runner_source_provider(&self) -> Option<RunSourceProviderParams> {
        self.config_snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .routing
                .get("provider.baseUrl")
                .map(|base_url| RunSourceProviderParams {
                    base_url: Some(base_url.clone()),
                    callback: false,
                })
        })
    }

    pub(crate) fn runner_model(&self) -> Option<RunModelParams> {
        #[cfg(test)]
        {
            Some(RunModelParams {
                callback: false,
                default_model_profile_id: None,
                model_profiles: Vec::new(),
            })
        }
        #[cfg(not(test))]
        {
            self.hosted_runner_model()
        }
    }

    #[cfg(not(test))]
    pub(crate) fn hosted_runner_model(&self) -> Option<RunModelParams> {
        let snapshot = self.config_snapshot.as_ref()?;
        let profile = snapshot.model_profile.as_ref()?;
        let provider = snapshot.routing.get("model.provider")?.clone();
        let model = snapshot.routing.get("model.name")?.clone();
        Some(RunModelParams {
            callback: false,
            default_model_profile_id: Some(profile.id.clone()),
            model_profiles: vec![RunModelProfileParams {
                id: profile.id.clone(),
                provider,
                model,
                credential: profile.secret_ref.as_deref().map(model_credential_from_ref),
                base_url: snapshot.routing.get("model.baseUrl").cloned(),
                api_protocol: Some("responses".to_string()),
                max_input_tokens: None,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
            }],
        })
    }

    pub(crate) fn dedupe_key(&self, source: &ReviewSource) -> Option<String> {
        self.dedupe.key_for_source(source, &self.metadata)
    }
}

#[cfg(not(test))]
fn model_credential_from_ref(secret_ref: &str) -> RunModelCredentialParams {
    if let Some(env) = secret_ref.strip_prefix("env:") {
        return RunModelCredentialParams {
            env: Some(env.to_string()),
            secret_ref: None,
        };
    }
    RunModelCredentialParams {
        env: None,
        secret_ref: Some(secret_ref.to_owned()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupePolicy {
    None,
    Source,
    SourceHead,
    Key(String),
}

impl Default for DedupePolicy {
    fn default() -> Self {
        Self::None
    }
}

impl DedupePolicy {
    fn key_for_source(
        &self,
        source: &ReviewSource,
        metadata: &BTreeMap<String, Value>,
    ) -> Option<String> {
        match self {
            Self::None => None,
            Self::Source => Some(format!("source:{}", source.source_key())),
            Self::SourceHead => {
                let source_key = source.source_key();
                let head_sha = metadata
                    .get("source.headSha")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|head_sha| !head_sha.is_empty());
                Some(match head_sha {
                    Some(head_sha) => format!("source-head:{source_key}@{head_sha}"),
                    None => format!("source-head:{source_key}"),
                })
            }
            Self::Key(key) => Some(format!("key:{key}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewChangeSpec {
    pub kind: String,
    #[serde(default)]
    pub base_revision: Option<String>,
    #[serde(default)]
    pub start_revision: Option<String>,
    #[serde(default)]
    pub head_revision: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<ReviewChangedFile>,
    #[serde(default)]
    pub diff: Option<String>,
    #[serde(default)]
    pub review_target: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl ReviewChangeSpec {
    fn changed_file_paths(&self) -> Vec<String> {
        self.changed_files
            .iter()
            .map(|file| file.path.clone())
            .collect()
    }

    fn to_runner_change(&self) -> RunChangeParams {
        RunChangeParams {
            kind: self.kind.clone(),
            base_revision: self.base_revision.clone(),
            start_revision: self.start_revision.clone(),
            head_revision: self.head_revision.clone(),
            changed_files: self
                .changed_files
                .iter()
                .map(ReviewChangedFile::to_runner_changed_file)
                .collect(),
            diff: self.diff.clone(),
            review_target: self.review_target.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewChangedFile {
    pub path: String,
    #[serde(default)]
    pub status: Option<String>,
}

impl ReviewChangedFile {
    fn to_runner_changed_file(&self) -> RunChangeFileParams {
        RunChangeFileParams {
            path: self.path.clone(),
            status: self.status.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewInstruction {
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub trusted: bool,
}

impl ReviewInstruction {
    fn to_runner_instruction(&self) -> RunInstructionParams {
        RunInstructionParams {
            kind: self.kind.clone(),
            text: self.text.clone(),
            trusted: self.trusted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewToolOption {
    pub id: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub effects: Vec<String>,
    #[serde(default)]
    pub cacheable: bool,
    #[serde(default)]
    pub provider_resources: Vec<String>,
}

impl ReviewToolOption {
    fn to_runner_tool(&self) -> RunToolParams {
        RunToolParams {
            id: self.id.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            effects: self.effects.clone(),
            cacheable: self.cacheable,
            provider_resources: self.provider_resources.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScope {
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAgentSession {
    pub id: String,
    pub role: Role,
    pub objective: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model_profile_id: Option<String>,
    #[serde(default)]
    pub instructions: Vec<ReviewInstruction>,
    #[serde(default)]
    pub tool_grants: Vec<String>,
    #[serde(default)]
    pub budget: Option<AgentBudget>,
}

impl ReviewAgentSession {
    pub fn new(id: impl Into<String>, role: Role, objective: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role,
            objective: objective.into(),
            cwd: None,
            model_profile_id: None,
            instructions: Vec::new(),
            tool_grants: Vec::new(),
            budget: None,
        }
    }

    fn to_runner_session(&self, default_model: Option<&str>) -> RunSessionParams {
        RunSessionParams {
            id: self.id.clone(),
            role: self.role,
            objective: self.objective.clone(),
            cwd: self.cwd.clone(),
            model_profile_id: self
                .model_profile_id
                .clone()
                .or_else(|| default_model.map(str::to_string)),
            response_format: None,
            instructions: self
                .instructions
                .iter()
                .map(ReviewInstruction::to_runner_instruction)
                .collect(),
            tool_grants: self.tool_grants.clone(),
            budget: self.budget.as_ref().map(|budget| RunAgentBudgetParams {
                max_turns: budget.max_turns,
                max_tool_calls: budget.max_tool_calls,
                max_prompt_tokens: budget.max_prompt_tokens,
                max_output_tokens: budget.max_output_tokens,
            }),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewLimits {
    #[serde(default)]
    pub max_active_sessions: Option<usize>,
    #[serde(default)]
    pub max_file_bytes: Option<usize>,
    #[serde(default)]
    pub max_search_matches: Option<usize>,
}

impl ReviewLimits {
    pub(crate) fn into_runner_limits(self) -> RunLimitParams {
        RunLimitParams {
            max_active_sessions: self.max_active_sessions,
            max_child_sessions: None,
            max_file_bytes: self.max_file_bytes,
            max_search_matches: self.max_search_matches,
            orchestrator_model_profile_id: None,
            search_model_profile_id: None,
            explore_model_profile_id: None,
            validator_model_profile_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveConfigSnapshot {
    #[serde(default)]
    pub model_profile: Option<ProfileVersionRef>,
    #[serde(default)]
    pub provider_profile: Option<ProfileVersionRef>,
    #[serde(default)]
    pub routing: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileVersionRef {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub secret_ref: Option<String>,
}
