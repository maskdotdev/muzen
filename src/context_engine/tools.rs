use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::runtime::contracts::{LimitInfo, RuntimeError, RuntimeResult, ToolEffects, ToolId};
use crate::runtime::tools::registry::{
    CustomToolContext, CustomToolHandler, CustomToolOutput, ToolRegistry,
};

use super::{
    ContextEngine, ContextPackPurpose, ContextQuery, ContextQueryKind, ContextQueryLimits,
};

pub const CONTEXT_SEARCH_TEXT_TOOL_ID: &str = "context.search_text";
pub const CONTEXT_READ_SPAN_TOOL_ID: &str = "context.read_span";
pub const CONTEXT_EXPLAIN_PACK_TOOL_ID: &str = "context.explain_pack";
pub const CONTEXT_RELATED_TESTS_TOOL_ID: &str = "context.related_tests";
pub const CONTEXT_RELATED_SYMBOLS_TOOL_ID: &str = "context.related_symbols";
pub const CONTEXT_TICKET_REQUIREMENTS_TOOL_ID: &str = "context.ticket_requirements";
pub const CONTEXT_HISTORY_SIMILAR_TOOL_ID: &str = "context.history_similar";
pub const CONTEXT_CROSS_REPO_CONTRACTS_TOOL_ID: &str = "context.cross_repo_contracts";
pub const CONTEXT_SUFFICIENCY_CHECK_TOOL_ID: &str = "context.sufficiency_check";

pub fn context_tool_ids() -> RuntimeResult<Vec<ToolId>> {
    Ok(vec![
        ToolId::parse(CONTEXT_SEARCH_TEXT_TOOL_ID)?,
        ToolId::parse(CONTEXT_READ_SPAN_TOOL_ID)?,
        ToolId::parse(CONTEXT_EXPLAIN_PACK_TOOL_ID)?,
        ToolId::parse(CONTEXT_RELATED_TESTS_TOOL_ID)?,
        ToolId::parse(CONTEXT_RELATED_SYMBOLS_TOOL_ID)?,
        ToolId::parse(CONTEXT_TICKET_REQUIREMENTS_TOOL_ID)?,
        ToolId::parse(CONTEXT_HISTORY_SIMILAR_TOOL_ID)?,
        ToolId::parse(CONTEXT_CROSS_REPO_CONTRACTS_TOOL_ID)?,
        ToolId::parse(CONTEXT_SUFFICIENCY_CHECK_TOOL_ID)?,
    ])
}

pub fn register_context_tools(
    registry: &mut ToolRegistry,
    engine: Arc<dyn ContextEngine>,
) -> RuntimeResult<()> {
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_SEARCH_TEXT_TOOL_ID)?,
        "Search indexed context evidence for literal terms separated by |.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "maxResults": {"type": "integer", "minimum": 1}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        true,
        context_tool_effects_for_kind(ContextQueryKind::SearchText),
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::SearchText,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_READ_SPAN_TOOL_ID)?,
        "Read an exact line span from indexed snapshot evidence.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "startLine": {"type": "integer", "minimum": 1},
                "endLine": {"type": "integer", "minimum": 1}
            },
            "required": ["path", "startLine", "endLine"],
            "additionalProperties": false
        }),
        true,
        context_tool_effects_for_kind(ContextQueryKind::ReadSpan),
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::ReadSpan,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_EXPLAIN_PACK_TOOL_ID)?,
        "Explain why a context pack included or omitted evidence.",
        json!({
            "type": "object",
            "properties": {
                "packId": {"type": "string"},
                "includeOmitted": {"type": "boolean"}
            },
            "required": ["packId"],
            "additionalProperties": false
        }),
        true,
        ToolEffects {
            repo_read: false,
            artifact_read: true,
            artifact_write: false,
            network_read: false,
            host_read: false,
            scratch_read: false,
            scratch_write: false,
            external_side_effect: false,
        },
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::ExplainPack,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_RELATED_TESTS_TOOL_ID)?,
        "Find indexed test evidence related to a repo path.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "maxResults": {"type": "integer", "minimum": 1}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        true,
        context_tool_effects_for_kind(ContextQueryKind::RelatedTests),
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::RelatedTests,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_RELATED_SYMBOLS_TOOL_ID)?,
        "Find indexed symbols and direct importers related to a repo path.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "symbol": {"type": "string"},
                "maxResults": {"type": "integer", "minimum": 1}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        true,
        context_tool_effects_for_kind(ContextQueryKind::RelatedSymbols),
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::RelatedSymbols,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_TICKET_REQUIREMENTS_TOOL_ID)?,
        "Find host-provided ticket requirements and acceptance criteria for this review.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "maxResults": {"type": "integer", "minimum": 1}
            },
            "additionalProperties": false
        }),
        true,
        context_tool_effects_for_kind(ContextQueryKind::TicketRequirements),
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::TicketRequirements,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_HISTORY_SIMILAR_TOOL_ID)?,
        "Find approved prior context learnings similar to this review question.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "maxResults": {"type": "integer", "minimum": 1}
            },
            "additionalProperties": false
        }),
        true,
        context_tool_effects_for_kind(ContextQueryKind::HistorySimilar),
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::HistorySimilar,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_CROSS_REPO_CONTRACTS_TOOL_ID)?,
        "Find host-provided cross-repository contract evidence or explain missing capability.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "maxResults": {"type": "integer", "minimum": 1}
            },
            "additionalProperties": false
        }),
        true,
        context_tool_effects_for_kind(ContextQueryKind::CrossRepoContracts),
        Arc::new(ContextToolHandler {
            engine: Arc::clone(&engine),
            kind: ContextQueryKind::CrossRepoContracts,
        }),
    )?;
    registry.register_custom_with_effects(
        ToolId::parse(CONTEXT_SUFFICIENCY_CHECK_TOOL_ID)?,
        "Check whether current context evidence is sufficient for a claim.",
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "currentEvidence": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            },
            "required": ["question"],
            "additionalProperties": false
        }),
        false,
        context_tool_effects_for_kind(ContextQueryKind::SufficiencyCheck),
        Arc::new(ContextToolHandler {
            engine,
            kind: ContextQueryKind::SufficiencyCheck,
        }),
    )
}

pub(crate) fn context_tool_effects_for_id(tool_id: &ToolId) -> ToolEffects {
    let kind = match tool_id.as_str() {
        CONTEXT_SEARCH_TEXT_TOOL_ID => ContextQueryKind::SearchText,
        CONTEXT_READ_SPAN_TOOL_ID => ContextQueryKind::ReadSpan,
        CONTEXT_EXPLAIN_PACK_TOOL_ID => ContextQueryKind::ExplainPack,
        CONTEXT_RELATED_TESTS_TOOL_ID => ContextQueryKind::RelatedTests,
        CONTEXT_RELATED_SYMBOLS_TOOL_ID => ContextQueryKind::RelatedSymbols,
        CONTEXT_TICKET_REQUIREMENTS_TOOL_ID => ContextQueryKind::TicketRequirements,
        CONTEXT_HISTORY_SIMILAR_TOOL_ID => ContextQueryKind::HistorySimilar,
        CONTEXT_CROSS_REPO_CONTRACTS_TOOL_ID => ContextQueryKind::CrossRepoContracts,
        CONTEXT_SUFFICIENCY_CHECK_TOOL_ID => ContextQueryKind::SufficiencyCheck,
        _ => return ToolEffects::default(),
    };
    context_tool_effects_for_kind(kind)
}

fn context_tool_effects_for_kind(kind: ContextQueryKind) -> ToolEffects {
    let (repo_read, artifact_read) = match kind {
        ContextQueryKind::ExplainPack => (false, true),
        _ => (true, true),
    };
    ToolEffects {
        repo_read,
        artifact_read,
        artifact_write: false,
        network_read: false,
        host_read: false,
        scratch_read: false,
        scratch_write: false,
        external_side_effect: false,
    }
}

struct ContextToolHandler {
    engine: Arc<dyn ContextEngine>,
    kind: ContextQueryKind,
}

#[async_trait]
impl CustomToolHandler for ContextToolHandler {
    async fn execute(
        &self,
        context: CustomToolContext,
        args: Value,
        cancel: CancellationToken,
    ) -> RuntimeResult<CustomToolOutput> {
        let current_evidence = args
            .get("currentEvidence")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|id| crate::runtime::contracts::EvidenceId(id.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let max_results = args
            .get("maxResults")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or_else(|| self.engine.config().max_query_results);
        let result = self
            .engine
            .query(
                ContextQuery {
                    run_id: None,
                    snapshot_id: context.snapshot_id,
                    session_id: Some(context.session_id),
                    purpose: Some(ContextPackPurpose::GeneralReview),
                    kind: self.kind,
                    arguments: args,
                    current_evidence,
                    limits: ContextQueryLimits {
                        max_results,
                        max_tokens: self.engine.config().max_pack_tokens,
                    },
                },
                cancel,
            )
            .await?;
        Ok(CustomToolOutput {
            data: Some(serde_json::to_value(result).map_err(|error| {
                RuntimeError::InvalidInput(format!("failed to serialize context result: {error}"))
            })?),
            artifact: None,
            limits: LimitInfo::default(),
        })
    }
}
