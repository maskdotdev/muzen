use super::super::*;
use super::common::*;
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn project_profiles_set_get_list_and_version() {
    let project = Muzen::new().project("acme");

    let first = project
        .set_model_profile(
            "default",
            ModelProfileInput {
                provider: ModelProviderKind::OpenaiCompatible,
                model: "gpt-5".to_string(),
                secret_ref: Some("vault://projects/acme/models/default".to_string()),
                base_url: Some("https://models.example.test".to_string()),
                routing: BTreeMap::from([("region".to_string(), "us-east".to_string())]),
            },
        )
        .await
        .unwrap();
    let second = project
        .set_model_profile(
            "default",
            ModelProfileInput {
                provider: ModelProviderKind::OpenaiCompatible,
                model: "gpt-5.1".to_string(),
                secret_ref: Some("vault://projects/acme/models/default".to_string()),
                base_url: Some("https://models.example.test".to_string()),
                routing: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let provider = project
        .set_provider_profile(
            "github",
            ProviderProfileInput {
                provider: SourceProviderKind::Github,
                secret_ref: Some("vault://projects/acme/providers/github".to_string()),
                base_url: Some("https://api.github.com".to_string()),
                routing: BTreeMap::new(),
            },
        )
        .await
        .unwrap();

    assert_eq!(first.version, "1");
    assert_eq!(second.version, "2");
    assert_eq!(provider.version, "1");
    assert_eq!(
        project
            .get_model_profile("default")
            .await
            .unwrap()
            .unwrap()
            .model,
        "gpt-5.1"
    );
    assert_eq!(project.list_model_profiles().await.unwrap().len(), 1);
    assert_eq!(project.list_provider_profiles().await.unwrap().len(), 1);
}

#[tokio::test]
async fn project_review_captures_model_config_snapshot_without_raw_secret() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let session_store = Arc::new(InMemoryReviewSessionStore::default());
    let profile_store = Arc::new(InMemoryProjectProfileStore::default());
    let muzen = Muzen::with_stores(session_store.clone(), profile_store);
    let project = muzen.project("acme");
    project
        .set_model_profile(
            "default",
            ModelProfileInput {
                provider: ModelProviderKind::OpenaiCompatible,
                model: "gpt-5".to_string(),
                secret_ref: Some("vault://projects/acme/models/default".to_string()),
                base_url: Some("https://models.example.test".to_string()),
                routing: BTreeMap::from([("region".to_string(), "us-east".to_string())]),
            },
        )
        .await
        .unwrap();

    let review = project
        .review_with_options(
            ReviewSource::local(repo.path()),
            options_for_files(["README.md"]),
        )
        .await
        .unwrap();
    let record = session_store.get(review.id()).await.unwrap().unwrap();
    let snapshot = record.config_snapshot.unwrap();
    let serialized = serde_json::to_string(&snapshot).unwrap();

    assert_eq!(
        snapshot
            .model_profile
            .as_ref()
            .map(|profile| profile.version.as_str()),
        Some("1")
    );
    assert_eq!(
        snapshot.routing.get("model.name").map(String::as_str),
        Some("gpt-5")
    );
    assert_eq!(
        snapshot
            .routing
            .get("model.routing.region")
            .map(String::as_str),
        Some("us-east")
    );
    assert!(serialized.contains("vault://projects/acme/models/default"));
    assert!(!serialized.contains("apiKey"));
    assert!(!serialized.contains("token"));
    assert!(!serialized.contains("sk-live"));
}

#[tokio::test]
async fn project_effective_snapshot_includes_source_provider_profile() {
    let project = Muzen::new().project("acme");
    project
        .set_provider_profile(
            "github",
            ProviderProfileInput {
                provider: SourceProviderKind::Github,
                secret_ref: Some("vault://projects/acme/providers/github".to_string()),
                base_url: Some("https://api.github.com".to_string()),
                routing: BTreeMap::from([("installation".to_string(), "123".to_string())]),
            },
        )
        .await
        .unwrap();
    let source = ReviewSource::github_pull_request("maskdotdev", "heimdaal", 123).unwrap();

    let snapshot = project
        .effective_config_snapshot(&source, None)
        .await
        .unwrap();

    assert_eq!(
        snapshot
            .provider_profile
            .as_ref()
            .map(|profile| profile.id.as_str()),
        Some("project:acme/providers/github")
    );
    assert_eq!(
        snapshot.routing.get("provider.kind").map(String::as_str),
        Some("github")
    );
    assert_eq!(
        snapshot
            .routing
            .get("provider.routing.installation")
            .map(String::as_str),
        Some("123")
    );
    assert_eq!(
        snapshot
            .provider_profile
            .as_ref()
            .and_then(|profile| profile.secret_ref.as_deref()),
        Some("vault://projects/acme/providers/github")
    );
}

#[tokio::test]
async fn project_profile_store_conformance_memory() {
    assert_project_profile_store_conformance(Arc::new(InMemoryProjectProfileStore::default()))
        .await;
}

#[tokio::test]
async fn project_profile_store_conformance_libsql() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        LibsqlProjectProfileStore::connect(temp.path().join("muzen.db"))
            .await
            .unwrap(),
    );

    assert_project_profile_store_conformance(store).await;
}

#[tokio::test]
async fn libsql_project_profiles_reopen_with_versions() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("muzen.db");
    let first = LibsqlProjectProfileStore::connect(&db_path).await.unwrap();
    first
        .set_model_profile(
            "acme",
            "default".to_string(),
            ModelProfileInput {
                provider: ModelProviderKind::Openai,
                model: "gpt-5-mini".to_string(),
                secret_ref: Some("vault://model".to_string()),
                base_url: None,
                routing: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let updated = first
        .set_model_profile(
            "acme",
            "default".to_string(),
            ModelProfileInput {
                provider: ModelProviderKind::Openai,
                model: "gpt-5.1".to_string(),
                secret_ref: Some("vault://model".to_string()),
                base_url: None,
                routing: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    drop(first);

    let reopened = LibsqlProjectProfileStore::connect(&db_path).await.unwrap();
    let loaded = reopened
        .get_model_profile("acme", "default")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(updated.version, "2");
    assert_eq!(loaded.model, "gpt-5.1");
    assert_eq!(loaded.version, "2");
}

async fn assert_project_profile_store_conformance(store: Arc<dyn ProjectProfileStore>) {
    let model = store
        .set_model_profile(
            "acme",
            "default".to_string(),
            model_profile_input("gpt-5-mini"),
        )
        .await
        .unwrap();
    let updated_model = store
        .set_model_profile(
            "acme",
            "default".to_string(),
            model_profile_input("gpt-5.1"),
        )
        .await
        .unwrap();
    let loaded_model = store
        .get_model_profile("acme", "default")
        .await
        .unwrap()
        .unwrap();
    let model_list = store.list_model_profiles("acme").await.unwrap();

    assert_eq!(model.version, "1");
    assert_eq!(updated_model.version, "2");
    assert_eq!(loaded_model.model, "gpt-5.1");
    assert_eq!(model_list.len(), 1);

    let provider = store
        .set_provider_profile("acme", "github".to_string(), provider_profile_input("123"))
        .await
        .unwrap();
    let updated_provider = store
        .set_provider_profile("acme", "github".to_string(), provider_profile_input("456"))
        .await
        .unwrap();
    let loaded_provider = store
        .get_provider_profile("acme", "github")
        .await
        .unwrap()
        .unwrap();
    let provider_list = store.list_provider_profiles("acme").await.unwrap();

    assert_eq!(provider.version, "1");
    assert_eq!(updated_provider.version, "2");
    assert_eq!(
        loaded_provider
            .routing
            .get("installation")
            .map(String::as_str),
        Some("456")
    );
    assert_eq!(provider_list.len(), 1);

    let empty_project_error = store
        .set_model_profile("", "default".to_string(), model_profile_input("gpt-5-mini"))
        .await
        .unwrap_err();
    let empty_name_error = store
        .set_provider_profile("acme", " ".to_string(), provider_profile_input("789"))
        .await
        .unwrap_err();

    assert!(empty_project_error
        .to_string()
        .contains("project id cannot be empty"));
    assert!(empty_name_error
        .to_string()
        .contains("profile name cannot be empty"));
}
