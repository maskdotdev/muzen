use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use crate::contracts::{ModelApiProtocol, ModelProfileRefV1, ProviderKind, ToolCallingMode};
use crate::reviewer::adapters::runtime;
use crate::reviewer::run::RunBuilder;
use crate::reviewer::tools::ReviewToolRegistry;
use crate::runtime::contracts::RuntimeError;
use crate::runtime::model::{
    CredentialResolver, EnvCredentialResolver, ModelLimiter, ProfileModelRouter,
};
use crate::runtime::policy::ReviewerPolicy;
use crate::runtime::tools::ToolRegistry as RuntimeToolRegistry;

#[cfg(test)]
use super::adapters::TestRunnerModel;
use super::adapters::{CallbackReviewModel, CallbackReviewTool};
use super::planning::{parse_provider_resources, parse_tool_effects};
use super::transport::RunnerCallbackTransport;
use super::types::{
    RunModelCredentialParams, RunModelParams, RunModelProfileParams, RunToolParams,
    RunnerSecretResolveParams, RunnerSecretResolveResult,
};
use super::RUNNER_PROTOCOL_VERSION;

pub(crate) struct RunnerWiring {
    pub(crate) tool_registry: Arc<RuntimeToolRegistry>,
    pub(crate) reviewer_policy: Arc<ReviewerPolicy>,
}

impl RunnerWiring {
    pub(crate) fn new(
        run_id: &str,
        tools: &[RunToolParams],
        transport: Option<Arc<dyn RunnerCallbackTransport>>,
    ) -> Result<Self> {
        Ok(Self {
            tool_registry: runner_tool_registry(run_id, tools, transport)?,
            reviewer_policy: Arc::new(ReviewerPolicy::new()),
        })
    }

    pub(crate) fn wire_model(
        &self,
        mut builder: RunBuilder,
        run_id: &str,
        model: &RunModelParams,
        max_active_sessions: usize,
        transport: Option<Arc<dyn RunnerCallbackTransport>>,
        #[cfg(test)] target_path: String,
    ) -> Result<RunBuilder> {
        if model.callback {
            let transport = transport
                .ok_or_else(|| anyhow::anyhow!("callback model requires interactive stdio"))?;
            builder = builder.review_model(Arc::new(CallbackReviewModel::new(
                run_id.to_string(),
                transport,
            )));
        } else if !model.model_profiles.is_empty() {
            let router = hosted_model_router(
                model,
                max_active_sessions,
                Arc::clone(&self.tool_registry),
                Arc::clone(&self.reviewer_policy),
                transport,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            builder = builder.model_router(Arc::new(router));
        } else {
            #[cfg(test)]
            {
                builder = builder.review_model(Arc::new(TestRunnerModel::new(
                    target_path,
                    "TODO|fn|class|export|pub".to_string(),
                )));
            }
            #[cfg(not(test))]
            anyhow::bail!("run model must be callback or hosted provider model");
        }
        Ok(builder
            .shared_tool_registry(Arc::clone(&self.tool_registry))
            .reviewer_policy(Arc::clone(&self.reviewer_policy)))
    }
}

fn runner_tool_registry(
    run_id: &str,
    tools: &[RunToolParams],
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
) -> Result<Arc<RuntimeToolRegistry>> {
    let mut registry = ReviewToolRegistry::review_defaults()
        .map_err(|error| anyhow::anyhow!("failed to create review tool registry: {error}"))?;
    if !tools.is_empty() {
        let transport =
            transport.ok_or_else(|| anyhow::anyhow!("callback tools require interactive stdio"))?;
        for tool in tools {
            let provider_resources = parse_provider_resources(&tool.provider_resources)?;
            let effects = parse_tool_effects(&tool.effects)?;
            registry
                .register_scoped_tool_with_effects(
                    &tool.id,
                    tool.description.clone(),
                    tool.parameters.clone(),
                    tool.cacheable,
                    provider_resources,
                    effects,
                    Arc::new(CallbackReviewTool::new(
                        run_id.to_string(),
                        transport.clone(),
                    )),
                )
                .map_err(|error| {
                    anyhow::anyhow!("failed to register SDK tool {}: {error}", tool.id)
                })?;
        }
    }
    Ok(Arc::new(registry.into_tool_registry()))
}

fn hosted_model_router(
    model: &RunModelParams,
    max_active_sessions: usize,
    tool_registry: Arc<RuntimeToolRegistry>,
    reviewer_policy: Arc<ReviewerPolicy>,
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
) -> runtime::RuntimeResult<ProfileModelRouter> {
    let profiles = model
        .model_profiles
        .iter()
        .map(model_profile_ref)
        .collect::<runtime::RuntimeResult<Vec<_>>>()?;
    let default_profile_id = model
        .default_model_profile_id
        .clone()
        .or_else(|| profiles.first().map(|profile| profile.id.clone()))
        .ok_or_else(|| RuntimeError::InvalidInput("hosted model requires a profile".to_string()))?;
    let base_url = hosted_model_base_url(model, &default_profile_id)?;
    ProfileModelRouter::from_profiles(
        &profiles,
        default_profile_id,
        base_url,
        Arc::new(ModelLimiter::new_with_per_key(
            max_active_sessions.max(1),
            max_active_sessions.max(1),
        )),
        tool_registry,
        reviewer_policy,
        Arc::new(RunnerCredentialResolver::new(transport)),
    )
}

fn hosted_model_base_url(
    model: &RunModelParams,
    default_profile_id: &str,
) -> runtime::RuntimeResult<String> {
    let mut configured_base_url: Option<&str> = None;
    for profile in &model.model_profiles {
        let Some(base_url) = profile
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if let Some(existing) = configured_base_url {
            if existing != base_url {
                return Err(RuntimeError::InvalidInput(
                    "runner v1 hosted model profiles must share one baseUrl".to_string(),
                ));
            }
        }
        configured_base_url = Some(base_url);
    }
    Ok(configured_base_url
        .map(ToString::to_string)
        .or_else(|| {
            model
                .model_profiles
                .iter()
                .find(|profile| profile.id == default_profile_id)
                .and_then(|profile| profile.base_url.clone())
        })
        .or_else(|| {
            std::env::var("OAI_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .ok()
        })
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string()))
}

fn model_profile_ref(params: &RunModelProfileParams) -> runtime::RuntimeResult<ModelProfileRefV1> {
    let provider_kind = match params.provider.as_str() {
        "openai" | "openai_compatible" => ProviderKind::OpenaiCompatible,
        unknown => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported model provider {unknown}"
            )))
        }
    };
    let api_protocol = match params.api_protocol.as_deref().unwrap_or("responses") {
        "responses" => ModelApiProtocol::Responses,
        "chat_completions" => ModelApiProtocol::ChatCompletions,
        unknown => {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported model apiProtocol {unknown}"
            )))
        }
    };
    Ok(ModelProfileRefV1 {
        id: params.id.clone(),
        provider_kind,
        api_protocol,
        provider_profile_id: params.provider.clone(),
        credential_ref: credential_ref(params.credential.as_ref())?,
        model: params.model.clone(),
        max_input_tokens: params.max_input_tokens.unwrap_or(128_000),
        max_output_tokens: params.max_output_tokens.unwrap_or(8_000),
        tool_calling_mode: ToolCallingMode::Auto,
        temperature: params.temperature,
        top_p: params.top_p,
    })
}

fn credential_ref(credential: Option<&RunModelCredentialParams>) -> runtime::RuntimeResult<String> {
    let Some(credential) = credential else {
        return Ok("env:OPENAI_API_KEY".to_string());
    };
    match (
        credential
            .env
            .as_deref()
            .filter(|value| !value.trim().is_empty()),
        credential
            .secret_ref
            .as_deref()
            .filter(|value| !value.trim().is_empty()),
    ) {
        (Some(env), None) => Ok(format!("env:{}", env.trim())),
        (None, Some(secret_ref)) => Ok(format!("secret:{}", secret_ref.trim())),
        _ => Err(RuntimeError::InvalidInput(
            "model credential must be exactly one of env or secretRef".to_string(),
        )),
    }
}

struct RunnerCredentialResolver {
    transport: Option<Arc<dyn RunnerCallbackTransport>>,
    env: EnvCredentialResolver,
}

impl RunnerCredentialResolver {
    fn new(transport: Option<Arc<dyn RunnerCallbackTransport>>) -> Self {
        Self {
            transport,
            env: EnvCredentialResolver,
        }
    }
}

impl CredentialResolver for RunnerCredentialResolver {
    fn resolve_credential(&self, credential_ref: &str) -> runtime::RuntimeResult<String> {
        let Some(secret_ref) = credential_ref.strip_prefix("secret:") else {
            return self.env.resolve_credential(credential_ref);
        };
        let transport = self.transport.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "model credential secretRef requires interactive stdio".to_string(),
            )
        })?;
        let params = RunnerSecretResolveParams {
            protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
            ref_name: secret_ref.to_string(),
        };
        let value = transport
            .request("secret.resolve", json!(params))
            .map_err(|_| {
                RuntimeError::InvalidInput("model credential is unavailable".to_string())
            })?;
        let result = serde_json::from_value::<RunnerSecretResolveResult>(value)
            .map_err(|_| RuntimeError::InvalidInput("invalid secret.resolve result".to_string()))?;
        if result.value.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "model credential is unavailable".to_string(),
            ));
        }
        Ok(result.value)
    }
}
