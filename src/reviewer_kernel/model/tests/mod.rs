use super::*;

use crate::reviewer_kernel::tool_engine::{CustomToolHandler, CustomToolOptions, CustomToolOutput};

struct StaticCredentialResolver;

impl CredentialResolver for StaticCredentialResolver {
    fn resolve_credential(&self, _credential_ref: &str) -> RuntimeResult<String> {
        Ok("test-key".to_string())
    }
}

fn profile_with_base_url(id: &str, base_url: Option<&str>) -> ModelProfileRefV1 {
    ModelProfileRefV1 {
        id: id.to_string(),
        provider_kind: ProviderKind::OpenaiCompatible,
        api_protocol: ModelApiProtocol::Responses,
        provider_profile_id: "openai".to_string(),
        credential_ref: "env:TEST_KEY".to_string(),
        model: "test-model".to_string(),
        base_url: base_url.map(ToString::to_string),
        max_input_tokens: 32_000,
        max_output_tokens: 1_024,
        temperature: None,
        top_p: None,
    }
}

#[test]
fn profile_base_url_prefers_profile_endpoint_over_default() {
    let default = "https://api.openai.com/v1";
    let configured = profile_with_base_url("a", Some(" https://vllm.internal/v1 "));
    assert_eq!(
        profile_base_url(&configured, default),
        "https://vllm.internal/v1"
    );
    let unset = profile_with_base_url("b", None);
    assert_eq!(profile_base_url(&unset, default), default);
    let blank = profile_with_base_url("c", Some("  "));
    assert_eq!(profile_base_url(&blank, default), default);
}

#[test]
fn router_accepts_profiles_with_mixed_base_urls() {
    let profiles = vec![
        profile_with_base_url("proxy", Some("https://proxy.internal/v1")),
        profile_with_base_url("local-vllm", Some("http://127.0.0.1:8000/v1")),
        profile_with_base_url("hosted-default", None),
    ];
    let router = ProfileModelRouter::from_profiles(
        &profiles,
        "hosted-default".to_string(),
        "https://api.openai.com/v1".to_string(),
        Arc::new(ModelLimiter::new_with_per_key(4, 4)),
        Arc::new(ToolRegistry::review_defaults().expect("registry")),
        Arc::new(ReviewerPolicy::new()),
        Arc::new(StaticCredentialResolver),
    )
    .expect("profiles with mixed base urls build one router");
    for id in ["proxy", "local-vllm", "hosted-default"] {
        assert!(router.clients.contains_key(id), "missing client for {id}");
    }
}

#[test]
fn insufficient_quota_provider_error_is_not_retryable() {
    let message = provider_error_message(
        r#"{
              "error": {
                "message": "You exceeded your current quota.",
                "type": "insufficient_quota",
                "param": null,
                "code": "insufficient_quota"
              }
            }"#
        .to_string(),
    );

    assert!(is_non_retryable_provider_quota_error(&message));
}

#[test]
fn responses_parse_tool_calls_through_model_alias_table() {
    let (registry, internal_tool, model_alias) = aliased_registry();
    let responses_turn = parse_responses_response(
        ResponsesResponse {
            output: vec![json!({
                "type": "function_call",
                "call_id": "call_responses",
                "name": model_alias.as_str(),
                "arguments": "{\"value\":\"ok\"}",
                "status": "completed"
            })],
            output_text: None,
            usage: Some(ResponsesUsage {
                input_tokens: Some(7),
                output_tokens: Some(4),
                total_tokens: Some(11),
            }),
        },
        &registry,
    )
    .expect("responses turn");
    assert_tool_call_turn(responses_turn, &internal_tool, "call_responses", 7, 4, 11);
}

#[test]
fn responses_request_body_includes_structured_text_format() {
    let registry = ToolRegistry::review_defaults().expect("registry");
    let profile = profile_with_base_url("responses", None);
    let scope = test_model_scope("responses", 64).with_response_format(test_response_format());
    let body = responses_request_body(
        &profile,
        &ReviewerPolicy::new(),
        &registry,
        &scope,
        &[ConversationItem::User {
            content: "Return JSON.".to_string(),
        }],
    )
    .expect("request body");

    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["name"], "test_result");
    assert_eq!(body["text"]["format"]["strict"], true);
    assert_eq!(body["text"]["format"]["schema"]["required"][0], "summary");
}

#[test]
fn responses_request_body_adds_input_for_system_only_transcripts() {
    let registry = ToolRegistry::review_defaults().expect("registry");
    let profile = profile_with_base_url("responses", None);
    let scope = test_model_scope("responses", 64);
    let body = responses_request_body(
        &profile,
        &ReviewerPolicy::new(),
        &registry,
        &scope,
        &[ConversationItem::System {
            content: "Review the change.".to_string(),
        }],
    )
    .expect("request body");

    assert_eq!(body["instructions"], "Review the change.");
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["input"][0]["content"][0]["text"], "Begin the task.");
}

#[test]
fn model_limiter_isolates_distinct_credential_keys() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let limiter = ModelLimiter::new_with_per_key(2, 1);
        let first_key_a = limiter.acquire_for_key("credential-a").await.unwrap();

        let second_key_a = tokio::time::timeout(
            Duration::from_millis(5),
            limiter.acquire_for_key("credential-a"),
        )
        .await;
        assert!(second_key_a.is_err());

        let key_b = tokio::time::timeout(
            Duration::from_millis(50),
            limiter.acquire_for_key("credential-b"),
        )
        .await
        .expect("credential-b should not share credential-a limiter")
        .unwrap();

        drop(key_b);
        drop(first_key_a);
    });
}

#[test]
fn model_limiter_isolates_provider_profile_key_and_session_buckets() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let limiter = ModelLimiter::new_with_buckets(4, 4, 1, 4, 1);
        let session_a = SessionId("session-a".to_string());
        let session_b = SessionId("session-b".to_string());
        let profile_a = limiter
            .acquire_for_model("provider", "profile-a", "credential-a", &session_a)
            .await
            .unwrap();

        let same_session = tokio::time::timeout(
            Duration::from_millis(5),
            limiter.acquire_for_model("provider", "profile-b", "credential-b", &session_a),
        )
        .await;
        assert!(same_session.is_err());

        let same_profile = tokio::time::timeout(
            Duration::from_millis(5),
            limiter.acquire_for_model("provider", "profile-a", "credential-b", &session_b),
        )
        .await;
        assert!(same_profile.is_err());

        let unrelated_profile = tokio::time::timeout(
            Duration::from_millis(50),
            limiter.acquire_for_model("provider", "profile-b", "credential-b", &session_b),
        )
        .await
        .expect("unrelated profile/session should not share saturated buckets")
        .unwrap();

        drop(unrelated_profile);
        drop(profile_a);
    });
}

fn test_model_scope(profile_id: &str, max_output_tokens: u32) -> SessionScope {
    SessionScope {
        id: SessionId(format!("test-model-{profile_id}")),
        role: crate::reviewer_kernel::review_contract::Role::Generalist,
        objective: "test model request".to_string(),
        instructions: Vec::new(),
        snapshot_id: None,
        model_profile_id: Some(profile_id.to_string()),
        response_format: None,
        capabilities: CapabilitySet::review_read_only(),
        budget: crate::reviewer_kernel::review_contract::AgentBudget {
            max_turns: 1,
            max_tool_calls: 1,
            max_prompt_tokens: 4_096,
            max_output_tokens: max_output_tokens as u64,
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::RunReserve,
        },
    }
}

fn aliased_registry() -> (ToolRegistry, ToolId, ToolId) {
    let mut registry = ToolRegistry::review_defaults().expect("registry");
    let internal_tool = ToolId::parse("internal_review_tool").unwrap();
    let model_alias = ToolId::parse("provider_review_tool").unwrap();
    registry
        .register_custom_with_alias_and_effects(
            internal_tool.clone(),
            model_alias.clone(),
            "provider-visible alias test tool",
            json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
            }),
            CustomToolOptions::default(),
            Arc::new(NoopCustomTool),
        )
        .unwrap();
    (registry, internal_tool, model_alias)
}

fn test_response_format() -> ModelResponseFormat {
    ModelResponseFormat::json_schema(
        "test_result",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["summary"],
            "properties": {
                "summary": { "type": "string" },
            },
        }),
    )
}

fn assert_tool_call_turn(
    turn: ModelTurn,
    expected_tool: &ToolId,
    expected_call_id: &str,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
) {
    let ModelTurn::ToolCalls { calls, usage } = turn else {
        panic!("expected tool-call turn");
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(&calls[0].name, expected_tool);
    assert_eq!(calls[0].call_id.0, expected_call_id);
    assert_eq!(calls[0].raw_arguments, r#"{"value":"ok"}"#);
    assert_eq!(usage.input_tokens, input_tokens);
    assert_eq!(usage.output_tokens, output_tokens);
    assert_eq!(usage.total_tokens, total_tokens);
}

struct NoopCustomTool;

#[async_trait::async_trait]
impl CustomToolHandler for NoopCustomTool {
    async fn execute(
        &self,
        _context: crate::reviewer_kernel::tool_engine::CustomToolContext,
        _args: Value,
        _cancel: CancellationToken,
    ) -> RuntimeResult<CustomToolOutput> {
        Ok(CustomToolOutput::default())
    }
}
