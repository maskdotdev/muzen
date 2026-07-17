use serde_json::{json, Value};

use super::validation::{validate_run_spec, validate_session_spec};
use super::{AnswerToolCallInput, AnswerToolCallOutcome, RunSpec, SessionSpec, ToolProvider};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../fixtures/agent-interface-v1.json"))
        .expect("agent contract fixture should be valid JSON")
}

#[test]
fn shared_fixture_deserializes_and_validates() {
    let fixture = fixture();
    let session: SessionSpec = serde_json::from_value(fixture["sessionSpec"].clone())
        .expect("session fixture should match the Rust wire type");
    let run: RunSpec = serde_json::from_value(fixture["runSpec"].clone())
        .expect("run fixture should match the Rust wire type");

    validate_session_spec(&session).expect("session fixture should validate");
    validate_run_spec(&run).expect("run fixture should validate");
}

#[test]
fn unknown_fields_are_rejected() {
    let mut session = fixture()["sessionSpec"].clone();
    session["legacyMode"] = json!(true);
    let error = serde_json::from_value::<SessionSpec>(session).expect_err("unknown field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn credential_headers_require_secret_references() {
    let mut session = fixture()["sessionSpec"].clone();
    session["toolProviders"][1]["headers"]["Authorization"] = json!("Bearer plaintext");
    let session: SessionSpec = serde_json::from_value(session).expect("wire shape is valid");
    let error = validate_session_spec(&session).expect_err("literal credential must fail");
    assert_eq!(error.path, "toolProviders[1].headers.Authorization");
}

#[test]
fn provider_protocol_pairs_are_strict() {
    let mut session = fixture()["sessionSpec"].clone();
    session["models"][0]["protocol"] = json!("messages");
    let session: SessionSpec = serde_json::from_value(session).expect("wire shape is valid");
    let error = validate_session_spec(&session).expect_err("invalid model pair must fail");
    assert_eq!(error.path, "models[0].protocol");
}

#[test]
fn run_limits_are_required_and_consistent() {
    let mut run = fixture()["runSpec"].clone();
    run["limits"]["maxActiveAgents"] = json!(17);
    let run: RunSpec = serde_json::from_value(run).expect("wire shape is valid");
    let error = validate_run_spec(&run).expect_err("invalid concurrency must fail");
    assert_eq!(error.path, "limits.maxActiveAgents");
}

#[test]
fn client_tool_provider_uses_camel_case_timeout_and_allows_the_default() {
    let provider: ToolProvider = serde_json::from_value(json!({
        "kind": "client",
        "id": "sdk"
    }))
    .expect("client provider without timeout");
    assert!(matches!(
        provider,
        ToolProvider::Client {
            timeout_ms: None,
            ..
        }
    ));
    let provider: ToolProvider = serde_json::from_value(json!({
        "kind": "client",
        "id": "sdk",
        "timeoutMs": 120000
    }))
    .expect("client provider timeout");
    assert_eq!(
        serde_json::to_value(provider).expect("serialize provider"),
        json!({ "kind": "client", "id": "sdk", "timeoutMs": 120000 })
    );
}

#[test]
fn client_tool_provider_timeout_is_bounded() {
    let mut session = fixture()["sessionSpec"].clone();
    session["toolProviders"]
        .as_array_mut()
        .expect("providers")
        .push(json!({ "kind": "client", "id": "sdk", "timeoutMs": 3600001 }));
    let session: SessionSpec = serde_json::from_value(session).expect("wire shape is valid");
    let error = validate_session_spec(&session).expect_err("oversized timeout must fail");
    assert_eq!(error.path, "toolProviders[2].timeoutMs");
}

#[test]
fn client_tool_answer_wire_is_strict_and_untagged() {
    let result: AnswerToolCallInput = serde_json::from_value(json!({
        "callId": "call-1",
        "outcome": { "result": { "ok": true } }
    }))
    .expect("result outcome");
    assert!(matches!(
        result.outcome,
        AnswerToolCallOutcome::Result { .. }
    ));
    let error: AnswerToolCallInput = serde_json::from_value(json!({
        "callId": "call-2",
        "outcome": { "error": { "message": "failed", "retryable": true } }
    }))
    .expect("error outcome");
    assert!(matches!(error.outcome, AnswerToolCallOutcome::Error { .. }));
    serde_json::from_value::<AnswerToolCallInput>(json!({
        "callId": "call-3",
        "outcome": {
            "result": null,
            "error": { "message": "ambiguous" }
        }
    }))
    .expect_err("outcome must contain exactly one variant");
}
