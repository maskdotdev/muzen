use super::*;

#[test]
fn default_review_uses_one_orchestrator() {
    let sessions = default_review_orchestrator_session();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "review-orchestrator");
    assert_eq!(sessions[0].role, Role::Generalist);
}

#[test]
fn parses_explicit_review_mode() {
    let mode = parse_run_mode(Some("review")).expect("mode should parse");

    assert_eq!(mode, RunMode::Review);
}

#[test]
fn rejects_unknown_mode() {
    let error = parse_run_mode(Some("swarm")).expect_err("mode should be rejected");

    assert!(error.to_string().contains("swarm"));
}

#[test]
fn defaults_large_reviews_to_eight_active_sessions() {
    assert_eq!(
        default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, None),
        8
    );
}

#[test]
fn keeps_small_review_default_session_parallelism() {
    assert_eq!(
        default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD, None),
        2
    );
    assert_eq!(default_max_active_sessions(0, 1, None), 4);
}

#[test]
fn explicit_max_active_sessions_overrides_large_review_default() {
    assert_eq!(
        default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, Some(3)),
        3
    );
    assert_eq!(
        default_max_active_sessions(2, LARGE_REVIEW_BATCH_THRESHOLD + 1, Some(0)),
        1
    );
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
