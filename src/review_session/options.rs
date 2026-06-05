use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contracts::{AgentBudget, Role};
use crate::runner::{
    RunAgentBudgetParams, RunLimitParams, RunSessionParams, RunSourceProviderParams,
};

use super::ReviewSource;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewOptions {
    #[serde(default)]
    pub dedupe: DedupePolicy,
    #[serde(default)]
    pub cancel_superseded: bool,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub scope: ReviewScope,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub sessions: Vec<ReviewAgentSession>,
    #[serde(default)]
    pub limits: Option<ReviewLimits>,
    #[serde(default)]
    pub config_snapshot: Option<EffectiveConfigSnapshot>,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            dedupe: DedupePolicy::None,
            cancel_superseded: false,
            user_id: None,
            model: None,
            scope: ReviewScope::default(),
            metadata: BTreeMap::new(),
            sessions: Vec::new(),
            limits: None,
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

    pub(crate) fn runner_source_provider(&self) -> Option<RunSourceProviderParams> {
        self.config_snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .routing
                .get("provider.baseUrl")
                .map(|base_url| RunSourceProviderParams {
                    base_url: Some(base_url.clone()),
                })
        })
    }

    pub(crate) fn dedupe_key(&self, source: &ReviewSource) -> Option<String> {
        self.dedupe.key_for_source(source, &self.metadata)
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScope {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
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
            max_file_bytes: self.max_file_bytes,
            max_search_matches: self.max_search_matches,
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
