use super::*;

#[test]
fn default_review_uses_one_orchestrator() {
    let session = crate::review_planning::default_review_orchestrator_session(Vec::new());
    let scope = session.into_session_scope();

    assert_eq!(scope.id.0, "review-orchestrator");
    assert_eq!(scope.role, Role::Generalist);
}

#[test]
fn run_session_spec_preserves_response_format() {
    let response_format = crate::reviewer_kernel::kernel_types::ModelResponseFormat::json_schema(
        "direct_findings",
        serde_json::json!({
            "type": "object",
            "required": ["findings"],
            "properties": {
                "findings": {"type": "array"}
            }
        }),
    );
    let spec = run_session_spec(
        RunSessionParams {
            id: "direct".to_string(),
            role: Role::Generalist,
            objective: "return findings".to_string(),
            cwd: None,
            model_profile_id: None,
            response_format: Some(response_format),
            instructions: Vec::new(),
            tool_grants: Vec::new(),
            budget: None,
        },
        &[],
        &[],
    )
    .expect("session spec");

    let scope = spec.into_session_scope();

    assert_eq!(
        scope
            .response_format
            .as_ref()
            .map(|format| format.name.as_str()),
        Some("direct_findings")
    );
}
