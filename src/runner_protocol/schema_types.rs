use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerHandshakeResult {
    pub protocol_version: String,
    pub runner_name: String,
    pub runner_version: String,
    pub capabilities: RunnerCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerCapabilities {
    pub supported_methods: Vec<String>,
    pub planned_methods: Vec<String>,
    pub transports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerCheckResult {
    pub ok: bool,
    pub protocol_version: String,
    pub runner_name: String,
    pub runner_version: String,
    pub rust_package: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerProtocolSchema {
    pub schema_version: String,
    pub transport: String,
    pub requests: Vec<RunnerMethodSchema>,
    pub callbacks: Vec<RunnerMethodSchema>,
    pub notifications: Vec<RunnerMethodSchema>,
    pub definitions: Vec<RunnerPayloadSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerMethodSchema {
    pub method: String,
    pub direction: RunnerMessageDirection,
    pub status: RunnerMethodStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<RunnerPayloadRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RunnerPayloadRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerPayloadRef {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerPayloadSchema {
    pub name: String,
    pub shape: RunnerPayloadShape,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<RunnerPayloadFieldSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunnerPayloadFieldSchema {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPayloadShape {
    Object,
    Enum,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMessageDirection {
    SdkToRunner,
    RunnerToSdk,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMethodStatus {
    Implemented,
    Reserved,
}
