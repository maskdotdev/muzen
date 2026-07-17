use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;

use reqwest::Url;

use super::output_schema::validate_schema_definition;

use super::{
    AgentDefinition, AgentInput, ContentBlock, ModelProfile, ModelProtocol, ModelProviderKind,
    PutSecretInput, RunRoot, RunSpec, SendCommand, SessionSpec, SpawnCommand, ToolEffect,
    ToolProvider, WorkspaceBase,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecValidationError {
    pub path: String,
    pub message: String,
}

impl SpecValidationError {
    fn at(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

pub(crate) fn validate_session_spec(spec: &SessionSpec) -> Result<(), SpecValidationError> {
    validate_agent_definition(&spec.agent, "agent")?;

    let mut model_ids = BTreeSet::new();
    for (index, model) in spec.models.iter().enumerate() {
        let path = format!("models[{index}]");
        if !model_ids.insert(model.id.as_str()) {
            return Err(SpecValidationError::at(
                format!("{path}.id"),
                "model profile id must be unique",
            ));
        }
        validate_model(model, &path)?;
    }
    if !model_ids.contains(spec.agent.model.as_str()) {
        return Err(SpecValidationError::at(
            "agent.model",
            "agent model must reference a model profile in the session",
        ));
    }

    let mut provider_ids = BTreeSet::new();
    for (index, provider) in spec.tool_providers.iter().enumerate() {
        let path = format!("toolProviders[{index}]");
        if !provider_ids.insert(provider.id().as_str()) {
            return Err(SpecValidationError::at(
                format!("{path}.id"),
                "tool provider id must be unique",
            ));
        }
        validate_tool_provider(provider, &path)?;
    }

    for (index, grant) in spec.agent.tools.iter().enumerate() {
        let path = format!("agent.tools[{index}]");
        if !provider_ids.contains(grant.provider.as_str()) {
            return Err(SpecValidationError::at(
                format!("{path}.provider"),
                "tool grant must reference a provider in the session",
            ));
        }
        if grant.tool.trim().is_empty() {
            return Err(SpecValidationError::at(
                format!("{path}.tool"),
                "tool name must not be empty",
            ));
        }
        if let Some(ToolProvider::McpHttp { .. }) = spec
            .tool_providers
            .iter()
            .find(|provider| provider.id() == &grant.provider)
        {
            if grant.effects.iter().any(|effect| {
                matches!(
                    effect,
                    ToolEffect::WorkspaceRead | ToolEffect::WorkspaceWrite
                )
            }) {
                return Err(SpecValidationError::at(
                    format!("{path}.effects"),
                    "MCP HTTP tools cannot receive workspace effects",
                ));
            }
        }
    }

    match &spec.workspace.base {
        WorkspaceBase::Path { root } if !root.is_absolute() => Err(SpecValidationError::at(
            "workspace.base.root",
            "path workspace roots must be absolute",
        )),
        WorkspaceBase::Path { root } if root.to_str().is_none() => Err(SpecValidationError::at(
            "workspace.base.root",
            "path workspace roots must be valid UTF-8",
        )),
        WorkspaceBase::Git { url, revision, .. } => {
            validate_network_url(url, "workspace.base.url", true)?;
            if !matches!(revision.len(), 40 | 64)
                || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(SpecValidationError::at(
                    "workspace.base.revision",
                    "git revision must be a full 40- or 64-character commit id",
                ));
            }
            Ok(())
        }
        WorkspaceBase::Snapshot { id } if id.trim().is_empty() => Err(SpecValidationError::at(
            "workspace.base.id",
            "snapshot id must not be empty",
        )),
        _ => Ok(()),
    }
}

pub(crate) fn validate_run_spec(spec: &RunSpec) -> Result<(), SpecValidationError> {
    if spec.roots.is_empty() {
        return Err(SpecValidationError::at(
            "roots",
            "a run requires at least one root",
        ));
    }
    if spec.limits.max_active_agents > spec.limits.max_agents {
        return Err(SpecValidationError::at(
            "limits.maxActiveAgents",
            "maxActiveAgents cannot exceed maxAgents",
        ));
    }
    if spec.roots.len() > spec.limits.max_agents.get() as usize {
        return Err(SpecValidationError::at(
            "roots",
            "root count cannot exceed maxAgents",
        ));
    }

    let mut decoded_input_bytes = 0_u64;
    let mut new_root_idempotency_keys = BTreeSet::new();
    for (index, root) in spec.roots.iter().enumerate() {
        let (input, new_session) = match root {
            RunRoot::Existing(root) => (&root.input, None),
            RunRoot::New(root) => {
                if let Some(key) = &root.idempotency_key {
                    if !new_root_idempotency_keys.insert(key.as_str()) {
                        return Err(SpecValidationError::at(
                            format!("roots[{index}].idempotencyKey"),
                            "new root idempotency keys must be unique within a run",
                        ));
                    }
                }
                (&root.input, Some(&root.session))
            }
        };
        validate_input(input, &format!("roots[{index}].input"))?;
        decoded_input_bytes = decoded_input_bytes
            .checked_add(decoded_input_bytes_for(
                input,
                &format!("roots[{index}].input"),
            )?)
            .ok_or_else(|| SpecValidationError::at("roots", "input byte count overflow"))?;
        if let Some(session) = new_session {
            validate_session_spec(session)?;
        }
    }
    if decoded_input_bytes > spec.limits.max_input_bytes.get() {
        return Err(SpecValidationError::at(
            "limits.maxInputBytes",
            "root inputs exceed maxInputBytes",
        ));
    }
    Ok(())
}

pub(crate) fn validate_secret_input(input: &PutSecretInput) -> Result<(), SpecValidationError> {
    decoded_base64_len(&input.value)
        .map(|_| ())
        .map_err(|_| SpecValidationError::at("value", "secret value must be valid padded base64"))
}

pub(crate) fn validate_send_command(command: &SendCommand) -> Result<(), SpecValidationError> {
    validate_input(&command.input, "input")
}

pub(crate) fn validate_spawn_command(command: &SpawnCommand) -> Result<(), SpecValidationError> {
    validate_agent_definition(&command.agent, "agent")?;
    validate_input(&command.input, "input")
}

pub(crate) fn decoded_input_bytes(input: &AgentInput) -> Result<u64, SpecValidationError> {
    decoded_input_bytes_for(input, "input")
}

fn validate_agent_definition(
    definition: &AgentDefinition,
    path: &str,
) -> Result<(), SpecValidationError> {
    if definition.instructions.is_empty() {
        return Err(SpecValidationError::at(
            format!("{path}.instructions"),
            "agent instructions must not be empty",
        ));
    }
    for (index, block) in definition.instructions.iter().enumerate() {
        validate_content_block(block, &format!("{path}.instructions[{index}]"))?;
    }
    if let Some(output) = &definition.output {
        validate_schema_definition(&output.schema, &format!("{path}.output.schema"))
            .map_err(|error| SpecValidationError::at(error.path, error.message))?;
    }
    Ok(())
}

fn validate_model(model: &ModelProfile, path: &str) -> Result<(), SpecValidationError> {
    if model.model.trim().is_empty() {
        return Err(SpecValidationError::at(
            format!("{path}.model"),
            "model name must not be empty",
        ));
    }
    let compatible = matches!(
        (model.provider, model.protocol),
        (
            ModelProviderKind::OpenaiCompatible,
            ModelProtocol::Responses | ModelProtocol::ChatCompletions
        ) | (ModelProviderKind::Anthropic, ModelProtocol::Messages)
    );
    if !compatible {
        return Err(SpecValidationError::at(
            format!("{path}.protocol"),
            "model protocol is incompatible with its provider",
        ));
    }
    if let Some(base_url) = &model.base_url {
        validate_network_url(base_url, &format!("{path}.baseUrl"), false)?;
    }
    if let Some(temperature) = model.temperature {
        if !temperature.is_finite() {
            return Err(SpecValidationError::at(
                format!("{path}.temperature"),
                "temperature must be finite",
            ));
        }
    }
    if let Some(top_p) = model.top_p {
        if !top_p.is_finite() || !(0.0..=1.0).contains(&top_p) {
            return Err(SpecValidationError::at(
                format!("{path}.topP"),
                "topP must be finite and between 0 and 1",
            ));
        }
    }
    Ok(())
}

fn validate_tool_provider(provider: &ToolProvider, path: &str) -> Result<(), SpecValidationError> {
    if let ToolProvider::Client {
        timeout_ms: Some(timeout_ms),
        ..
    } = provider
    {
        if timeout_ms.get() > 3_600_000 {
            return Err(SpecValidationError::at(
                format!("{path}.timeoutMs"),
                "client tool timeoutMs must not exceed 3600000",
            ));
        }
    }
    let ToolProvider::McpHttp { url, headers, .. } = provider else {
        return Ok(());
    };
    validate_network_url(url, &format!("{path}.url"), false)?;
    for (name, value) in headers {
        let credential_header = matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "proxy-authorization" | "cookie" | "x-api-key"
        );
        if credential_header && matches!(value, super::HeaderValue::Literal(_)) {
            return Err(SpecValidationError::at(
                format!("{path}.headers.{name}"),
                "credential header values must use a SecretRef",
            ));
        }
    }
    Ok(())
}

fn validate_network_url(
    value: &str,
    path: &str,
    allow_ssh: bool,
) -> Result<(), SpecValidationError> {
    if allow_ssh && value.starts_with("ssh://") {
        return Url::parse(value)
            .map(|_| ())
            .map_err(|_| SpecValidationError::at(path, "URL is invalid"));
    }
    let url = Url::parse(value).map_err(|_| SpecValidationError::at(path, "URL is invalid"))?;
    if url.scheme() == "https" || url.scheme() == "http" && is_loopback_host(&url) {
        return Ok(());
    }
    Err(SpecValidationError::at(
        path,
        "URL must use HTTPS, except for an adapter-approved loopback HTTP endpoint",
    ))
}

fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn validate_input(input: &AgentInput, path: &str) -> Result<(), SpecValidationError> {
    if input.content.is_empty() {
        return Err(SpecValidationError::at(
            format!("{path}.content"),
            "agent input must contain at least one block",
        ));
    }
    for (index, block) in input.content.iter().enumerate() {
        validate_content_block(block, &format!("{path}.content[{index}]"))?;
    }
    Ok(())
}

fn validate_content_block(block: &ContentBlock, path: &str) -> Result<(), SpecValidationError> {
    match block {
        ContentBlock::Text { text } if text.is_empty() => Err(SpecValidationError::at(
            format!("{path}.text"),
            "text content must not be empty",
        )),
        ContentBlock::Image {
            media_type, data, ..
        } => {
            if media_type.trim().is_empty() {
                return Err(SpecValidationError::at(
                    format!("{path}.mediaType"),
                    "image media type must not be empty",
                ));
            }
            decoded_base64_len(data).map(|_| ()).map_err(|_| {
                SpecValidationError::at(
                    format!("{path}.data"),
                    "image data must be valid padded base64",
                )
            })
        }
        _ => Ok(()),
    }
}

fn decoded_input_bytes_for(input: &AgentInput, path: &str) -> Result<u64, SpecValidationError> {
    let mut total = 0_u64;
    for (index, block) in input.content.iter().enumerate() {
        let bytes = match block {
            ContentBlock::Text { text } => text.len() as u64,
            ContentBlock::Artifact { .. } => 0,
            ContentBlock::Image { data, .. } => decoded_base64_len(data).map_err(|_| {
                SpecValidationError::at(
                    format!("{path}.content[{index}].data"),
                    "image data must be valid padded base64",
                )
            })?,
        };
        total = total
            .checked_add(bytes)
            .ok_or_else(|| SpecValidationError::at(path, "input byte count overflow"))?;
    }
    Ok(total)
}

fn decoded_base64_len(value: &str) -> io::Result<u64> {
    let mut decoder = base64::read::DecoderReader::new(
        value.as_bytes(),
        &base64::engine::general_purpose::STANDARD,
    );
    io::copy(&mut decoder, &mut io::sink())
}
