use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub limits: Option<ReviewLimits>,
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
            limits: None,
            config_snapshot: None,
        }
    }
}

impl ReviewOptions {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewChangedFile {
    pub path: String,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewInstruction {
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub trusted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScope {
    #[serde(default)]
    pub files: Vec<String>,
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
