use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{CapabilitySet, SessionId, SnapshotId};
use crate::reviewer_kernel::review_contract::{AgentBudget, Role};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionScope {
    pub id: SessionId,
    pub role: Role,
    pub objective: String,
    pub instructions: Vec<SessionInstruction>,
    pub snapshot_id: Option<SnapshotId>,
    pub model_profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ModelResponseFormat>,
    pub capabilities: CapabilitySet,
    pub budget: AgentBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelResponseFormat {
    pub name: String,
    pub schema: Value,
    #[serde(default = "default_strict_response_format")]
    pub strict: bool,
}

impl ModelResponseFormat {
    pub fn json_schema(name: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            schema,
            strict: true,
        }
    }
}

fn default_strict_response_format() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInstruction {
    pub kind: String,
    pub text: String,
    pub trusted: bool,
}

impl SessionScope {
    pub fn review_read_only(
        id: SessionId,
        role: Role,
        objective: impl Into<String>,
        budget: AgentBudget,
    ) -> Self {
        Self {
            id,
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

    #[cfg(test)]
    pub fn with_snapshot_id(mut self, snapshot_id: SnapshotId) -> Self {
        self.snapshot_id = Some(snapshot_id);
        self
    }

    #[cfg(test)]
    pub fn with_response_format(mut self, response_format: ModelResponseFormat) -> Self {
        self.response_format = Some(response_format);
        self
    }
}
