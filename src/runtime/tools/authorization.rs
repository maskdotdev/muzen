use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;

use crate::runtime::contracts::{
    stable_id, SessionId, ToolEffects, ToolErrorCode, ToolGrant, ToolId, ToolInvocation,
    ToolProviderId,
};

use super::registry::ToolDefinition;

#[derive(Debug, Default)]
pub(crate) struct ToolAuthorizer {
    calls_by_session_tool: DashMap<String, AtomicUsize>,
}

impl ToolAuthorizer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn authorize(
        &self,
        invocation: &ToolInvocation,
        definition: &ToolDefinition,
    ) -> Result<(), ToolAuthorizationError> {
        let Some(grant) = invocation.capabilities.tool_grants.get(&invocation.tool_id) else {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool is not allowed for this session",
            ));
        };
        if !grant.allow {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool is not allowed for this session",
            ));
        }
        if !invocation
            .capabilities
            .runtime_authority
            .allows_provider(&definition.provider_id)
        {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool provider is not allowed for this session",
            ));
        }
        if !invocation
            .capabilities
            .runtime_authority
            .allows_provider_resources(&definition.provider_id, &definition.provider_resources)
        {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool provider resource is not allowed for this session",
            ));
        }
        if !effects_allow(definition.effects, grant.effects_allowed) {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool effects exceed this session capability grant",
            ));
        }
        if definition.effects.network_read
            && !invocation.capabilities.runtime_authority.network_read
        {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool network read is not allowed for this session",
            ));
        }
        if definition.effects.host_read && !invocation.capabilities.runtime_authority.host_read {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool host read is not allowed for this session",
            ));
        }
        if definition.effects.scratch_read
            && !invocation.capabilities.runtime_authority.scratch_read
        {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool scratch read is not allowed for this session",
            ));
        }
        if definition.effects.scratch_write
            && !invocation.capabilities.runtime_authority.scratch_write
        {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool scratch write is not allowed for this session",
            ));
        }
        if definition.effects.external_side_effect
            && !invocation
                .capabilities
                .runtime_authority
                .external_side_effect
        {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool external side effect is not allowed for this session",
            ));
        }
        if definition.effects.artifact_read
            && !invocation.capabilities.artifact_access.read_redacted
        {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool artifact read is not allowed for this session",
            ));
        }
        if definition.effects.artifact_write && !invocation.capabilities.artifact_access.write {
            return Err(denied(
                ToolErrorCode::ToolNotAllowed,
                "tool artifact write is not allowed for this session",
            ));
        }
        self.enforce_call_limit(invocation, grant)?;
        if invocation.builtin_name.is_none()
            && definition.provider_id == ToolProviderId::in_process()
            && definition.handler.as_ref().is_none()
        {
            return Err(denied(
                ToolErrorCode::UnknownTool,
                "custom tool has no registered handler",
            ));
        }
        Ok(())
    }

    fn enforce_call_limit(
        &self,
        invocation: &ToolInvocation,
        grant: &ToolGrant,
    ) -> Result<(), ToolAuthorizationError> {
        let Some(max_calls) = grant.max_calls else {
            return Ok(());
        };
        let key = call_limit_key(&invocation.session_id, &invocation.tool_id);
        let counter = self
            .calls_by_session_tool
            .entry(key)
            .or_insert_with(|| AtomicUsize::new(0));
        if counter.fetch_add(1, Ordering::Relaxed) >= max_calls as usize {
            return Err(denied(
                ToolErrorCode::BudgetExceeded,
                "tool capability call limit exceeded",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct ToolAuthorizationError {
    pub(crate) code: ToolErrorCode,
    pub(crate) message: &'static str,
}

fn denied(code: ToolErrorCode, message: &'static str) -> ToolAuthorizationError {
    ToolAuthorizationError { code, message }
}

fn call_limit_key(session_id: &SessionId, tool_id: &ToolId) -> String {
    stable_id(&[&session_id.0, tool_id.as_str()])
}

fn effects_allow(required: ToolEffects, allowed: ToolEffects) -> bool {
    (!required.repo_read || allowed.repo_read)
        && (!required.artifact_read || allowed.artifact_read)
        && (!required.artifact_write || allowed.artifact_write)
        && (!required.network_read || allowed.network_read)
        && (!required.host_read || allowed.host_read)
        && (!required.scratch_read || allowed.scratch_read)
        && (!required.scratch_write || allowed.scratch_write)
        && (!required.external_side_effect || allowed.external_side_effect)
}
