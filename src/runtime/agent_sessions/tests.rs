use super::*;
use crate::contracts::{AgentBudget, Role};
use serde_json::json;

#[test]
fn final_text_scope_withholds_tools_but_preserves_response_format() {
    let scope = SessionScope::review_read_only(
        SessionId("direct".to_string()),
        Role::Generalist,
        "return findings",
        AgentBudget::planned_baseline(),
    )
    .with_response_format(ModelResponseFormat::json_schema(
        "direct_findings",
        json!({
            "type": "object",
            "required": ["findings"],
            "properties": {
                "findings": {"type": "array"}
            }
        }),
    ));
    assert!(!scope.capabilities.tool_grants.is_empty());

    let final_scope = final_text_scope(&scope);

    assert!(final_scope.capabilities.tool_grants.is_empty());
    assert_eq!(
        final_scope
            .response_format
            .as_ref()
            .map(|format| format.name.as_str()),
        Some("direct_findings")
    );
    assert!(!scope.capabilities.tool_grants.is_empty());
}

#[test]
fn tool_turn_scope_preserves_tools_but_withholds_response_format() {
    let scope = SessionScope::review_read_only(
        SessionId("direct".to_string()),
        Role::Generalist,
        "explore first",
        AgentBudget::planned_baseline(),
    )
    .with_response_format(ModelResponseFormat::json_schema(
        "direct_findings",
        json!({
            "type": "object",
            "required": ["findings"],
            "properties": {
                "findings": {"type": "array"}
            }
        }),
    ));

    let tool_scope = tool_turn_scope(&scope);

    assert!(!tool_scope.capabilities.tool_grants.is_empty());
    assert!(tool_scope.response_format.is_none());
    assert!(scope.response_format.is_some());
}
