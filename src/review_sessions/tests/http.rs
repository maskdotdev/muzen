use super::super::*;
use super::common::options_for_files;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn review_events_response_replays_json_from_store() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let project = Muzen::with_store(store).project("acme");
    let review = project
        .schedule_review_with_options(ReviewSource::local("."), options_for_files(["Cargo.toml"]))
        .await
        .unwrap();

    let response = project
        .review_events_response(review.id(), None)
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&response.body).unwrap();

    assert_eq!(response.status_code, HTTP_STATUS_OK);
    assert_eq!(response.header("Content-Type"), Some(CONTENT_TYPE_JSON));
    assert_eq!(payload["events"][0]["cursor"], json!("1"));
    assert_eq!(payload["events"][0]["type"], json!("session.queued"));
    assert_eq!(
        payload["events"][0]["reviewId"],
        json!(review.id().as_str())
    );

    let after_response = project
        .review_events_response(review.id(), Some("1"))
        .await
        .unwrap();
    let after_payload: Value = serde_json::from_str(&after_response.body).unwrap();

    assert_eq!(after_payload["events"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn review_events_sse_response_renders_service_side_event_stream() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let project = Muzen::with_store(store).project("acme");
    let review = project
        .schedule_review_with_options(ReviewSource::local("."), options_for_files(["Cargo.toml"]))
        .await
        .unwrap();

    let response = project
        .review_events_sse_response(review.id(), None)
        .await
        .unwrap();

    assert_eq!(response.status_code, HTTP_STATUS_OK);
    assert_eq!(
        response.header("Content-Type"),
        Some(CONTENT_TYPE_EVENT_STREAM)
    );
    assert_eq!(response.header("Cache-Control"), Some("no-cache"));
    assert_eq!(response.header("X-Accel-Buffering"), Some("no"));
    assert!(response.body.contains("id: 1\n"));
    assert!(response.body.contains("event: session.queued\n"));
    assert!(response
        .body
        .contains("data: {\"cursor\":\"1\",\"type\":\"session.queued\",\"reviewId\":\"review-1\""));
    assert!(response.body.ends_with("\n\n"));
}

#[tokio::test]
async fn review_http_router_schedules_root_review_and_replays_events() {
    let muzen = Muzen::new();
    let router = ReviewHttpRouter::new(muzen.clone());
    let create_request = ReviewHttpRequest::new("POST", "/v1/reviews")
        .json(&json!({
            "source": {
                "type": "local",
                "repo": "."
            },
            "options": {
                "dedupe": "source",
                "change": {
                    "kind": "revision_range",
                    "baseRevision": "base",
                    "headRevision": "head",
                    "changedFiles": [
                        {
                            "path": "Cargo.toml",
                            "status": "modified"
                        }
                    ]
                }
            }
        }))
        .unwrap();

    let create_response = router.handle(create_request).await;
    let create_body: Value = serde_json::from_str(&create_response.body).unwrap();
    let get_response = router
        .handle(ReviewHttpRequest::new("GET", "/v1/reviews/review-1"))
        .await;
    let get_body: Value = serde_json::from_str(&get_response.body).unwrap();
    let events_response = router
        .handle(ReviewHttpRequest::new(
            "GET",
            "/v1/reviews/review-1/events?after=1",
        ))
        .await;
    let events_body: Value = serde_json::from_str(&events_response.body).unwrap();

    assert_eq!(create_response.status_code, HTTP_STATUS_ACCEPTED);
    assert_eq!(
        create_response.header("Content-Type"),
        Some(CONTENT_TYPE_JSON)
    );
    assert_eq!(create_body["review"]["id"], json!("review-1"));
    assert_eq!(create_body["review"]["status"], json!("queued"));
    assert_eq!(create_body["review"]["source"]["type"], json!("local"));
    assert_eq!(get_response.status_code, HTTP_STATUS_OK);
    assert_eq!(get_body["review"]["id"], json!("review-1"));
    assert_eq!(events_response.status_code, HTTP_STATUS_OK);
    assert_eq!(events_body["events"].as_array().unwrap().len(), 0);

    let record = muzen
        .store
        .get(&ReviewSessionId::new("review-1").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, ReviewStatus::Queued);
    assert!(record.result.is_none());
}

#[tokio::test]
async fn review_http_router_serves_results_and_artifacts_from_store() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let muzen = Muzen::new();
    let review = muzen
        .review_with_options(
            ReviewSource::local(repo.path()),
            options_for_files(["README.md"]),
        )
        .await
        .unwrap();
    let router = ReviewHttpRouter::new(muzen);

    let result_response = router
        .handle(ReviewHttpRequest::new(
            "GET",
            format!("/v1/reviews/{}/result", review.id()).as_str(),
        ))
        .await;
    let export_response = router
        .handle(ReviewHttpRequest::new(
            "POST",
            format!("/v1/reviews/{}/artifacts/export", review.id()).as_str(),
        ))
        .await;
    let result_body: Value = serde_json::from_str(&result_response.body).unwrap();
    let export_body: Value = serde_json::from_str(&export_response.body).unwrap();
    let artifact_id = export_body["artifacts"][0]["artifactId"]
        .as_str()
        .unwrap()
        .to_string();
    let artifact_response = router
        .handle(ReviewHttpRequest::new(
            "GET",
            format!(
                "/v1/reviews/{}/artifacts/{}?view=redacted",
                review.id(),
                artifact_id
            )
            .as_str(),
        ))
        .await;
    let artifact_body: Value = serde_json::from_str(&artifact_response.body).unwrap();

    assert_eq!(result_response.status_code, HTTP_STATUS_OK);
    assert_eq!(result_body["result"]["status"], json!("completed"));
    assert_eq!(export_response.status_code, HTTP_STATUS_OK);
    assert!(export_body["artifactCount"].as_u64().unwrap() > 0);
    assert_eq!(artifact_response.status_code, HTTP_STATUS_OK);
    assert_eq!(artifact_body["artifact"]["artifactId"], json!(artifact_id));
}

#[tokio::test]
async fn review_http_router_handles_project_profile_routes() {
    let router = ReviewHttpRouter::new(Muzen::new());
    let put_model = ReviewHttpRequest::new("PUT", "/v1/projects/acme/models/default")
        .json(&ModelProfileInput {
            provider: ModelProviderKind::OpenaiCompatible,
            model: "gpt-5".to_string(),
            secret_ref: Some("vault://projects/acme/models/default".to_string()),
            base_url: Some("https://models.example.test".to_string()),
            routing: BTreeMap::from([("region".to_string(), "us-east".to_string())]),
        })
        .unwrap();
    let put_provider = ReviewHttpRequest::new("PUT", "/v1/projects/acme/providers/github")
        .json(&ProviderProfileInput {
            provider: SourceProviderKind::Github,
            secret_ref: Some("vault://projects/acme/providers/github".to_string()),
            base_url: Some("https://api.github.com".to_string()),
            routing: BTreeMap::new(),
        })
        .unwrap();

    let model_response = router.handle(put_model).await;
    let provider_response = router.handle(put_provider).await;
    let models_response = router
        .handle(ReviewHttpRequest::new("GET", "/v1/projects/acme/models"))
        .await;
    let provider_body: Value = serde_json::from_str(&provider_response.body).unwrap();
    let models_body: Value = serde_json::from_str(&models_response.body).unwrap();
    let missing_response = router
        .handle(ReviewHttpRequest::new(
            "GET",
            "/v1/projects/acme/models/missing",
        ))
        .await;

    assert_eq!(model_response.status_code, HTTP_STATUS_OK);
    assert_eq!(provider_response.status_code, HTTP_STATUS_OK);
    assert_eq!(
        provider_body["profile"]["secretRef"],
        json!("vault://projects/acme/providers/github")
    );
    assert_eq!(models_response.status_code, HTTP_STATUS_OK);
    assert_eq!(models_body["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(missing_response.status_code, HTTP_STATUS_NO_CONTENT);
    assert!(missing_response.body.is_empty());
}
