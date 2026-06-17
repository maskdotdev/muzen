use super::*;

fn profile_params(id: &str, base_url: Option<&str>) -> RunModelProfileParams {
    RunModelProfileParams {
        id: id.to_string(),
        provider: "openai".to_string(),
        model: "test-model".to_string(),
        credential: None,
        base_url: base_url.map(ToString::to_string),
        api_protocol: None,
        max_input_tokens: None,
        max_output_tokens: None,
        temperature: None,
        top_p: None,
    }
}

fn model_params(profiles: Vec<RunModelProfileParams>) -> RunModelParams {
    RunModelParams {
        callback: false,
        default_model_profile_id: None,
        model_profiles: profiles,
    }
}

#[test]
fn agreeing_profile_base_urls_do_not_become_the_run_default() {
    let model = model_params(vec![
        profile_params("a", Some("https://proxy.internal/v1")),
        profile_params("b", None),
        profile_params("c", Some("https://proxy.internal/v1")),
    ]);
    assert_eq!(
        hosted_model_default_base_url(&model),
        hosted_model_default_base_url(&model_params(Vec::new()))
    );
}

#[test]
fn mixed_profile_base_urls_are_allowed_and_do_not_change_global_default() {
    let model = model_params(vec![
        profile_params("a", Some("https://proxy.internal/v1")),
        profile_params("b", Some("http://127.0.0.1:8000/v1")),
    ]);
    let default = hosted_model_default_base_url(&model);
    assert_eq!(
        default,
        hosted_model_default_base_url(&model_params(Vec::new()))
    );
}
