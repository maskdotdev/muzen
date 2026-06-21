use super::*;
use crate::reviewer_kernel::review_contract::BudgetSource;
use crate::runner_protocol::types::RunAgentBudgetParams;
use crate::runner_protocol::RUNNER_PROTOCOL_VERSION;

#[test]
fn default_review_uses_one_orchestrator() {
    let session = crate::review_planning::default_review_orchestrator_session(
        Vec::new(),
        AgentBudget::planned_baseline(),
    );
    let scope = session.into_session_scope();

    assert_eq!(scope.id.0, "review-orchestrator");
    assert_eq!(scope.role, Role::Generalist);
}

#[test]
fn run_level_budget_caps_default_autonomous_orchestrator() {
    let repo = tempfile::tempdir().expect("temp repo");
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .expect("fixture file");
    let plan = plan_run_start(
        RunStartParams {
            protocol_version: Some(RUNNER_PROTOCOL_VERSION.to_string()),
            run_id: Some("budgeted-autonomous".to_string()),
            repo: Some(repo.path().to_path_buf()),
            source: None,
            source_provider: None,
            changed_files: vec!["Cargo.toml".to_string()],
            metadata: Default::default(),
            change: None,
            instructions: Vec::new(),
            sessions: Vec::new(),
            budget: Some(RunAgentBudgetParams {
                max_turns: 3,
                max_tool_calls: 2,
                max_prompt_tokens: 12_000,
                max_output_tokens: 1_000,
            }),
            limits: None,
            model: None,
            tools: Vec::new(),
            heartbeat: None,
            context_engine: None,
        },
        None,
    )
    .expect("plan run");
    let scope = plan.spec.sessions[0].clone().into_session_scope();

    assert_eq!(scope.id.0, "review-orchestrator");
    assert_eq!(scope.budget.max_turns, 3);
    assert_eq!(scope.budget.max_tool_calls, 2);
    assert_eq!(scope.budget.budget_source, BudgetSource::CallerHardCap);
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
