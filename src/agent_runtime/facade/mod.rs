//! Ergonomic Rust authoring facade over the transport-neutral Agent Runtime wire types.

mod result;
mod tools;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use base64::Engine as _;
use serde_json::Value;

#[cfg(test)]
use result::result_from_run;
use result::run_in_session;
pub use result::AgentResult;
pub use tools::{LoopbackToolServer, Tool, TypedToolHandler};

use super::validation::SpecValidationError;
use super::{
    AgentBudget, AgentDefinition, AgentInput, AgentName, AgentSession, CommandOptions,
    ContentBlock, CreateOptions, ErrorCode, HttpTransportOptions, LocalRuntimeConfig, ModelProfile,
    ModelProfileId, ModelProtocol, ModelProviderKind, Muzen, MuzenError, OutputContract,
    PutSecretInput, RunLimits, SecretRef, SessionId, SessionSpec, ToolEffect, ToolGrant,
    ToolProvider, ToolProviderId, WorkspaceBase, WorkspaceSpec,
};

const MODEL_ID: &str = "default";
const BUILTIN_PROVIDER_ID: &str = "builtin";
const LOCAL_TOOLS_PROVIDER_ID: &str = "local_tools";
const DEFAULT_MAX_INPUT_TOKENS: u64 = 128_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4_096;

/// Instructions accepted by [`AgentBuilder::instructions`].
pub enum AgentInstructions {
    Blocks(Vec<ContentBlock>),
}

impl From<String> for AgentInstructions {
    fn from(text: String) -> Self {
        Self::Blocks(vec![ContentBlock::Text { text }])
    }
}

impl From<&str> for AgentInstructions {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl From<ContentBlock> for AgentInstructions {
    fn from(block: ContentBlock) -> Self {
        Self::Blocks(vec![block])
    }
}

impl From<Vec<ContentBlock>> for AgentInstructions {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Self::Blocks(blocks)
    }
}

impl<const N: usize> From<[ContentBlock; N]> for AgentInstructions {
    fn from(blocks: [ContentBlock; N]) -> Self {
        Self::Blocks(Vec::from(blocks))
    }
}

/// Values accepted as facade run input.
pub trait IntoAgentInput {
    fn into_agent_input(self) -> AgentInput;
}

impl IntoAgentInput for AgentInput {
    fn into_agent_input(self) -> AgentInput {
        self
    }
}

impl IntoAgentInput for String {
    fn into_agent_input(self) -> AgentInput {
        AgentInput {
            content: vec![ContentBlock::Text { text: self }],
        }
    }
}

impl IntoAgentInput for &str {
    fn into_agent_input(self) -> AgentInput {
        self.to_owned().into_agent_input()
    }
}

impl IntoAgentInput for ContentBlock {
    fn into_agent_input(self) -> AgentInput {
        AgentInput {
            content: vec![self],
        }
    }
}

impl IntoAgentInput for Vec<ContentBlock> {
    fn into_agent_input(self) -> AgentInput {
        AgentInput { content: self }
    }
}

/// Builder for [`Agent`]. All facade options are validated together by [`build`](Self::build).
pub struct AgentBuilder {
    instructions: Option<Vec<ContentBlock>>,
    model: Option<String>,
    output_schema: Option<Value>,
    can_spawn: bool,
    can_message: bool,
    tools: Option<Vec<Tool>>,
    spec: Option<SessionSpec>,
    client: Option<Muzen>,
    transport: String,
    api_key: Option<String>,
    base_url: Option<String>,
    bearer_token: Option<String>,
    model_base_url: Option<String>,
    temperature: Option<f64>,
    max_output_tokens: Option<u64>,
    max_total_tokens: Option<u64>,
    deadline_ms: Option<u64>,
    budget: Option<AgentBudget>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            instructions: None,
            model: None,
            output_schema: None,
            can_spawn: false,
            can_message: false,
            tools: None,
            spec: None,
            client: None,
            transport: "local".to_owned(),
            api_key: None,
            base_url: None,
            bearer_token: None,
            model_base_url: None,
            temperature: None,
            max_output_tokens: None,
            max_total_tokens: None,
            deadline_ms: None,
            budget: None,
        }
    }
}

impl AgentBuilder {
    pub fn instructions(mut self, instructions: impl Into<AgentInstructions>) -> Self {
        let AgentInstructions::Blocks(blocks) = instructions.into();
        self.instructions = Some(blocks);
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the explicit JSON Schema used for structured output.
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Alias for [`Self::output_schema`] matching the Python facade option name.
    pub fn output(self, schema: Value) -> Self {
        self.output_schema(schema)
    }

    pub fn can_spawn(mut self, enabled: bool) -> Self {
        self.can_spawn = enabled;
        self
    }

    pub fn can_message(mut self, enabled: bool) -> Self {
        self.can_message = enabled;
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools = Some(tools.into_iter().collect());
        self
    }

    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.get_or_insert_with(Vec::new).push(tool);
        self
    }

    pub fn spec(mut self, spec: SessionSpec) -> Self {
        self.spec = Some(spec);
        self
    }

    pub fn client(mut self, client: Muzen) -> Self {
        self.client = Some(client);
        self
    }

    /// Selects `local` (the default) or `http` transport.
    pub fn transport(mut self, transport: impl Into<String>) -> Self {
        self.transport = transport.into();
        self
    }

    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Bearer token presented to the Muzen service on the `http` transport.
    /// Matches Python's `bearer_token` and TypeScript's `bearerToken`.
    pub fn bearer_token(mut self, bearer_token: impl Into<String>) -> Self {
        self.bearer_token = Some(bearer_token.into());
        self
    }

    /// Overrides the model endpoint independently of [`Self::base_url`],
    /// which identifies the Muzen service on the `http` transport. Matches
    /// Python's `model_base_url` and TypeScript's `modelBaseUrl`.
    pub fn model_base_url(mut self, model_base_url: impl Into<String>) -> Self {
        self.model_base_url = Some(model_base_url.into());
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn max_output_tokens(mut self, tokens: u64) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    pub fn max_total_tokens(mut self, tokens: u64) -> Self {
        self.max_total_tokens = Some(tokens);
        self
    }

    pub fn deadline_ms(mut self, deadline_ms: u64) -> Self {
        self.deadline_ms = Some(deadline_ms);
        self
    }

    pub fn budget(mut self, budget: AgentBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    pub fn build(self) -> Result<Agent, MuzenError> {
        if !matches!(self.transport.as_str(), "local" | "http") {
            return Err(invalid("transport", "must be 'local' or 'http'"));
        }
        let configured_tools = self.tools.clone().unwrap_or_default();
        let has_tools = !configured_tools.is_empty();
        if self.transport == "http" && self.client.is_none() && self.base_url.is_none() {
            return Err(invalid("base_url", "is required for HTTP transport"));
        }
        if let Some(tools) = &self.tools {
            let mut names = BTreeSet::new();
            if tools.iter().any(|tool| !names.insert(tool.name())) {
                return Err(invalid("tools", "must have unique function names"));
            }
            // The provider layer maps builtin agent.spawn/agent.message to the
            // model-visible names agent_spawn/agent_message and routes those
            // names back to the builtins, so a user tool with either name
            // would be hijacked rather than called.
            if (self.can_spawn && names.contains("agent_spawn"))
                || (self.can_message && names.contains("agent_message"))
            {
                return Err(invalid("tools", "must have unique function names"));
            }
        }

        let default_limits = RunLimits {
            max_active_agents: NonZeroU32::new(4).expect("non-zero constant"),
            max_agents: NonZeroU32::new(16).expect("non-zero constant"),
            max_depth: 3,
            max_input_bytes: NonZeroU64::new(1_048_576).expect("non-zero constant"),
            max_total_tokens: optional_non_zero("max_total_tokens", self.max_total_tokens)?,
            max_total_tool_calls: None,
            deadline_ms: optional_non_zero("deadline_ms", self.deadline_ms)?,
        };

        let (spec_template, api_key, needs_secret, has_output, temp_dir) =
            if let Some(spec) = self.spec {
                let conflicting = self.instructions.is_some()
                    || self.model.is_some()
                    || self.output_schema.is_some()
                    || self.can_spawn
                    || self.can_message
                    || self.api_key.is_some()
                    || self.model_base_url.is_some()
                    || self.temperature.is_some()
                    || self.max_output_tokens.is_some()
                    || self.budget.is_some()
                    || self.tools.is_some();
                if conflicting {
                    return Err(invalid(
                        "spec",
                        "cannot be combined with facade authoring options",
                    ));
                }
                let has_output = spec.agent.output.is_some();
                (spec, None, false, has_output, None)
            } else {
                let instructions = self
                    .instructions
                    .ok_or_else(|| invalid("instructions", "is required"))?;
                validate_instructions(&instructions)?;
                let model = self.model.ok_or_else(|| invalid("model", "is required"))?;
                let settings = model_settings(&model)?;
                let api_key = resolve_api_key(self.api_key, settings.environment)?;
                let max_output_tokens = match self.max_output_tokens {
                    Some(0) => return Err(invalid("max_output_tokens", "must be positive")),
                    Some(value) => value,
                    None => DEFAULT_MAX_OUTPUT_TOKENS,
                };
                let temp_dir = tempfile::Builder::new()
                    .prefix("muzen-agent-")
                    .tempdir()
                    .map_err(|error| {
                        MuzenError::internal(format!(
                            "failed to create temporary agent workspace: {error}"
                        ))
                    })?;
                let tools = self.tools.as_deref().unwrap_or_default();
                let mut grants = Vec::new();
                if self.can_spawn {
                    grants.push(ToolGrant {
                        provider: tool_provider_id(BUILTIN_PROVIDER_ID),
                        tool: "agent.spawn".to_owned(),
                        description: None,
                        input_schema: None,
                        effects: vec![ToolEffect::AgentSpawn],
                        max_calls: None,
                    });
                }
                if self.can_message {
                    grants.push(ToolGrant {
                        provider: tool_provider_id(BUILTIN_PROVIDER_ID),
                        tool: "agent.message".to_owned(),
                        description: None,
                        input_schema: None,
                        effects: vec![ToolEffect::AgentMessage],
                        max_calls: None,
                    });
                }
                grants.extend(tools.iter().map(|tool| ToolGrant {
                    provider: tool_provider_id(LOCAL_TOOLS_PROVIDER_ID),
                    tool: tool.name().to_owned(),
                    description:
                        (!tool.description().is_empty()).then(|| tool.description().to_owned()),
                    input_schema: Some(tool.input_schema().clone()),
                    effects: Vec::new(),
                    max_calls: None,
                }));
                let mut providers = Vec::new();
                if self.can_spawn || self.can_message {
                    providers.push(ToolProvider::Builtin {
                        id: tool_provider_id(BUILTIN_PROVIDER_ID),
                    });
                }
                if has_tools && self.transport == "http" {
                    providers.push(ToolProvider::Client {
                        id: tool_provider_id(LOCAL_TOOLS_PROVIDER_ID),
                        timeout_ms: None,
                    });
                }
                let has_output = self.output_schema.is_some();
                let spec = SessionSpec {
                    agent: AgentDefinition {
                        name: AgentName::new("agent").expect("valid constant"),
                        instructions,
                        model: ModelProfileId::new(MODEL_ID).expect("valid constant"),
                        tools: grants,
                        budget: self.budget,
                        output: self
                            .output_schema
                            .map(|schema| OutputContract { schema, name: None }),
                        metadata: BTreeMap::new(),
                    },
                    models: vec![ModelProfile {
                        id: ModelProfileId::new(MODEL_ID).expect("valid constant"),
                        provider: settings.provider,
                        protocol: settings.protocol,
                        model: settings.name,
                        base_url: self.model_base_url.clone().or_else(|| {
                            (self.transport == "local")
                                .then(|| self.base_url.clone())
                                .flatten()
                        }),
                        credential: SecretRef::new("pending").expect("valid constant"),
                        max_input_tokens: NonZeroU64::new(DEFAULT_MAX_INPUT_TOKENS)
                            .expect("non-zero constant"),
                        max_output_tokens: NonZeroU64::new(max_output_tokens)
                            .expect("validated positive"),
                        temperature: self.temperature,
                        top_p: None,
                    }],
                    tool_providers: providers,
                    workspace: WorkspaceSpec {
                        base: WorkspaceBase::Path {
                            root: temp_dir.path().to_path_buf(),
                        },
                        limits: None,
                    },
                    session_budget: None,
                    metadata: BTreeMap::new(),
                };
                (spec, Some(api_key), true, has_output, Some(temp_dir))
            };

        let tool_server = (self.transport == "local" && has_tools)
            .then(|| LoopbackToolServer::new(configured_tools.clone()))
            .transpose()?;
        let client_tools = if self.transport == "http" {
            Arc::new(configured_tools)
        } else {
            Arc::new(Vec::new())
        };
        let service_base_url = (self.transport == "http")
            .then_some(self.base_url)
            .flatten();
        let bearer_token = (self.transport == "http")
            .then_some(self.bearer_token)
            .flatten();
        Ok(Agent {
            client: self.client,
            transport: self.transport,
            service_base_url,
            bearer_token,
            secret_ref: None,
            closed: false,
            api_key,
            needs_secret,
            has_output,
            spec_template,
            default_limits,
            tool_server,
            client_tools,
            temp_dir,
        })
    }
}

/// Ergonomic authoring facade over [`Muzen`] sessions and runs.
pub struct Agent {
    client: Option<Muzen>,
    transport: String,
    service_base_url: Option<String>,
    bearer_token: Option<String>,
    secret_ref: Option<SecretRef>,
    closed: bool,
    api_key: Option<String>,
    needs_secret: bool,
    has_output: bool,
    spec_template: SessionSpec,
    default_limits: RunLimits,
    tool_server: Option<LoopbackToolServer>,
    client_tools: Arc<Vec<Tool>>,
    temp_dir: Option<tempfile::TempDir>,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        instructions: impl Into<AgentInstructions>,
        model: impl Into<String>,
    ) -> AgentBuilder {
        AgentBuilder::default()
            .instructions(instructions)
            .model(model)
    }

    /// Establishes the selected transport and starts local tools, if any.
    pub async fn connect(&mut self) -> Result<(), MuzenError> {
        let _ = self.connection().await?;
        Ok(())
    }

    pub async fn run(&mut self, input: impl IntoAgentInput) -> Result<AgentResult, MuzenError> {
        self.run_with_limits(input, self.default_limits.clone())
            .await
    }

    pub async fn run_with_limits(
        &mut self,
        input: impl IntoAgentInput,
        limits: RunLimits,
    ) -> Result<AgentResult, MuzenError> {
        let (client, spec) = self.ready().await?;
        let session = client
            .create_session(spec, CreateOptions::default())
            .await?;
        let result = run_in_session(
            &session,
            &client,
            Arc::clone(&self.client_tools),
            input.into_agent_input(),
            limits,
            self.has_output,
        )
        .await;
        archive_best_effort(&session).await;
        result
    }

    /// Opens one reusable session. Call [`AgentConversation::close`] to archive it promptly.
    pub async fn session(&mut self) -> Result<AgentConversation<'_>, MuzenError> {
        let (client, spec) = self.ready().await?;
        let session = client
            .create_session(spec, CreateOptions::default())
            .await?;
        let limits = self.default_limits.clone();
        let has_output = self.has_output;
        let client_tools = Arc::clone(&self.client_tools);
        Ok(AgentConversation {
            _agent: self,
            client,
            session: Some(session),
            limits,
            has_output,
            client_tools,
        })
    }

    /// Idempotently closes the client, tool server, and temporary workspace.
    pub async fn close(&mut self) -> Result<(), MuzenError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let client_result = match self.client.take() {
            Some(client) => client.close().await,
            None => Ok(()),
        };
        if let Some(server) = &self.tool_server {
            server.close().await;
        }
        self.temp_dir.take();
        client_result
    }

    async fn connection(&mut self) -> Result<Muzen, MuzenError> {
        if self.closed {
            return Err(MuzenError::new(ErrorCode::Unavailable, "Agent is closed"));
        }
        if let Some(server) = &self.tool_server {
            server.start()?;
        }
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        let client = if self.transport == "http" {
            Muzen::http(
                self.service_base_url.as_deref().unwrap_or_default(),
                HttpTransportOptions {
                    bearer_token: self.bearer_token.clone(),
                },
            )?
        } else {
            let model_url = self
                .spec_template
                .models
                .first()
                .and_then(|profile| profile.base_url.as_deref());
            let allow_loopback = self.tool_server.is_some() || is_loopback_url(model_url);
            Muzen::local(
                LocalRuntimeConfig::memory_with_model_router().with_loopback_http(allow_loopback),
            )
            .await?
        };
        self.client = Some(client.clone());
        Ok(client)
    }

    async fn ready(&mut self) -> Result<(Muzen, SessionSpec), MuzenError> {
        let client = self.connection().await?;
        let mut spec = self.spec_template.clone();
        if let Some(server) = &self.tool_server {
            let provider = ToolProvider::McpHttp {
                id: tool_provider_id(LOCAL_TOOLS_PROVIDER_ID),
                url: server.url()?,
                credential: None,
                headers: BTreeMap::new(),
            };
            spec.tool_providers
                .retain(|item| item.id().as_str() != LOCAL_TOOLS_PROVIDER_ID);
            spec.tool_providers.push(provider);
        }
        if self.needs_secret {
            if self.secret_ref.is_none() {
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(self.api_key.as_deref().unwrap_or_default());
                self.secret_ref = Some(
                    client
                        .put_secret(PutSecretInput {
                            value: encoded,
                            idempotency_key: None,
                        })
                        .await?,
                );
            }
            let secret = self.secret_ref.clone().expect("secret was initialized");
            for profile in &mut spec.models {
                if profile.id.as_str() == MODEL_ID {
                    profile.credential = secret.clone();
                }
            }
        }
        Ok((client, spec))
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let client = self.client.take();
        let tool_server = self.tool_server.take();
        let temp_dir = self.temp_dir.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(client) = client {
                    let _ = client.close().await;
                }
                if let Some(tool_server) = tool_server {
                    tool_server.close().await;
                }
                drop(temp_dir);
            });
        }
        // Without a Tokio runtime, dropping the server still signals graceful shutdown and the
        // temporary directory is removed normally with the captured values.
    }
}

/// One persistent session created by [`Agent::session`].
pub struct AgentConversation<'a> {
    _agent: &'a mut Agent,
    client: Muzen,
    session: Option<AgentSession>,
    limits: RunLimits,
    has_output: bool,
    client_tools: Arc<Vec<Tool>>,
}

impl AgentConversation<'_> {
    pub fn session_id(&self) -> &SessionId {
        self.session.as_ref().expect("conversation is open").id()
    }

    pub async fn run(&self, input: impl IntoAgentInput) -> Result<AgentResult, MuzenError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| MuzenError::new(ErrorCode::Conflict, "Agent conversation is closed"))?;
        run_in_session(
            session,
            &self.client,
            Arc::clone(&self.client_tools),
            input.into_agent_input(),
            self.limits.clone(),
            self.has_output,
        )
        .await
    }

    pub async fn run_with_limits(
        &self,
        input: impl IntoAgentInput,
        limits: RunLimits,
    ) -> Result<AgentResult, MuzenError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| MuzenError::new(ErrorCode::Conflict, "Agent conversation is closed"))?;
        run_in_session(
            session,
            &self.client,
            Arc::clone(&self.client_tools),
            input.into_agent_input(),
            limits,
            self.has_output,
        )
        .await
    }

    pub async fn close(mut self) {
        if let Some(session) = self.session.take() {
            archive_best_effort(&session).await;
        }
    }
}

impl Drop for AgentConversation<'_> {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                archive_best_effort(&session).await;
            });
        }
    }
}

async fn archive_best_effort(session: &AgentSession) {
    let _ = session.archive(CommandOptions::default()).await;
}

struct ModelSettings {
    provider: ModelProviderKind,
    protocol: ModelProtocol,
    name: String,
    environment: &'static str,
}

fn model_settings(model: &str) -> Result<ModelSettings, MuzenError> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return Err(invalid("model", "must not be empty"));
    }
    let mut name = trimmed;
    let mut override_provider = None;
    if let Some((prefix, candidate)) = trimmed.split_once(':') {
        if matches!(prefix, "anthropic" | "openai") {
            override_provider = Some(prefix);
            name = candidate;
            if name.is_empty() {
                return Err(invalid(
                    "model",
                    "must include a name after the provider prefix",
                ));
            }
        }
    }
    let anthropic = override_provider == Some("anthropic")
        || (override_provider.is_none() && name.to_ascii_lowercase().starts_with("claude"));
    if anthropic {
        Ok(ModelSettings {
            provider: ModelProviderKind::Anthropic,
            protocol: ModelProtocol::Messages,
            name: name.to_owned(),
            environment: "ANTHROPIC_API_KEY",
        })
    } else {
        Ok(ModelSettings {
            provider: ModelProviderKind::OpenaiCompatible,
            protocol: ModelProtocol::ChatCompletions,
            name: name.to_owned(),
            environment: "OPENAI_API_KEY",
        })
    }
}

fn resolve_api_key(
    explicit: Option<String>,
    environment: &'static str,
) -> Result<String, MuzenError> {
    explicit
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var(environment)
                .ok()
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| {
            invalid(
                "api_key",
                &format!("is required when {environment} is not set"),
            )
        })
}

fn validate_instructions(instructions: &[ContentBlock]) -> Result<(), MuzenError> {
    if instructions.is_empty() {
        return Err(invalid(
            "instructions",
            "must contain at least one content block",
        ));
    }
    if instructions
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text } if text.trim().is_empty()))
    {
        return Err(invalid("instructions", "text blocks must not be empty"));
    }
    Ok(())
}

fn optional_non_zero(path: &str, value: Option<u64>) -> Result<Option<NonZeroU64>, MuzenError> {
    value
        .map(|value| NonZeroU64::new(value).ok_or_else(|| invalid(path, "must be positive")))
        .transpose()
}

fn is_loopback_url(value: Option<&str>) -> bool {
    value
        .and_then(|value| reqwest::Url::parse(value).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

fn tool_provider_id(value: &str) -> ToolProviderId {
    ToolProviderId::new(value).expect("valid tool provider constant")
}

pub(super) fn invalid(path: &str, message: &str) -> MuzenError {
    SpecValidationError {
        path: path.to_owned(),
        message: format!("{path} {message}"),
    }
    .into()
}
