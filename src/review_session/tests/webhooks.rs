use super::super::*;
use crate::context_engine::ContextHttpRouterOptions;
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test]
async fn review_http_router_verifies_and_schedules_workspace_github_webhook() {
    let muzen = Muzen::new();
    let router = ReviewHttpRouter::with_options(
        muzen.clone(),
        ReviewHttpRouterOptions {
            github_webhook_secret: Some("secret".to_string()),
            gitlab_webhook_secret: None,
            context: ContextHttpRouterOptions::default(),
        },
    );
    let body = json!({
        "action": "opened",
        "repository": {
            "full_name": "maskdotdev/heimdaal"
        },
        "pull_request": {
            "number": 123,
            "head": {
                "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }
    })
    .to_string();
    let signature = github_webhook_signature("secret", body.as_bytes()).unwrap();
    let request = ReviewHttpRequest::new("POST", "/v1/workspaces/acme/webhooks/github")
        .header("X-GitHub-Event", "pull_request")
        .header("X-GitHub-Delivery", "delivery-1")
        .header("X-Hub-Signature-256", signature)
        .body(body.into_bytes());

    let response = router.handle(request).await;
    let response_body: Value = serde_json::from_str(&response.body).unwrap();
    let record = muzen
        .store
        .get(&ReviewSessionId::new("review-1").unwrap())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.status_code, HTTP_STATUS_ACCEPTED);
    assert_eq!(response_body["type"], json!("review_created"));
    assert_eq!(response_body["deliveryId"], json!("delivery-1"));
    assert_eq!(record.workspace_id.as_deref(), Some("acme"));
    assert_eq!(record.status, ReviewStatus::Queued);
    assert_eq!(record.options.metadata["webhook.provider"], json!("github"));
}

#[tokio::test]
async fn github_webhook_verifies_maps_schedules_and_dedupes_pull_request() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let body = json!({
        "action": "opened",
        "repository": {
            "full_name": "maskdotdev/heimdaal"
        },
        "pull_request": {
            "number": 123,
            "head": {
                "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }
    })
    .to_string();
    let signature = github_webhook_signature("secret", body.as_bytes()).unwrap();
    let headers = WebhookHeaders::from([
        ("X-GitHub-Event", "pull_request"),
        ("X-GitHub-Delivery", "delivery-1"),
        ("X-Hub-Signature-256", signature.as_str()),
    ]);
    let options = WebhookReviewOptions::new(ReviewOptions {
        dedupe: DedupePolicy::Source,
        ..ReviewOptions::default()
    });

    let first = workspace
        .handle_github_webhook(&headers, body.as_bytes(), Some("secret"), options.clone())
        .await
        .unwrap();
    let second = workspace
        .handle_github_webhook(&headers, body.as_bytes(), Some("secret"), options)
        .await
        .unwrap();
    let first_response = first.http_response().unwrap();
    let second_response = second.http_response().unwrap();
    let first_response_body: Value = serde_json::from_str(&first_response.body).unwrap();
    let second_response_body: Value = serde_json::from_str(&second_response.body).unwrap();

    assert_eq!(first_response.status_code, HTTP_STATUS_ACCEPTED);
    assert_eq!(
        first_response.header("Content-Type"),
        Some(CONTENT_TYPE_JSON)
    );
    assert_eq!(first_response_body["type"], json!("review_created"));
    assert_eq!(second_response.status_code, HTTP_STATUS_OK);
    assert_eq!(second_response_body["type"], json!("review_deduped"));

    let review = match first {
        WebhookReviewDelivery::ReviewCreated {
            review,
            delivery_id,
        } => {
            assert_eq!(delivery_id, "delivery-1");
            review
        }
        delivery => panic!("expected review_created, got {delivery:?}"),
    };
    let record = store.get(review.id()).await.unwrap().unwrap();
    assert_eq!(
        record.source,
        ReviewSource::GithubPullRequest {
            owner: "maskdotdev".to_string(),
            repo: "heimdaal".to_string(),
            number: 123
        }
    );
    assert_eq!(record.status, ReviewStatus::Queued);
    assert_eq!(record.options.metadata["webhook.provider"], json!("github"));
    assert_eq!(
        record.options.metadata["webhook.deliveryId"],
        json!("delivery-1")
    );
    assert_eq!(
        record.options.metadata["source.headSha"],
        json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    match second {
        WebhookReviewDelivery::ReviewDeduped {
            review: deduped,
            delivery_id,
        } => {
            assert_eq!(delivery_id, "delivery-1");
            assert_eq!(deduped.id(), review.id());
        }
        delivery => panic!("expected review_deduped, got {delivery:?}"),
    }
}

#[tokio::test]
async fn github_webhook_source_head_dedupe_includes_head_sha() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let options = WebhookReviewOptions::new(ReviewOptions {
        dedupe: DedupePolicy::SourceHead,
        ..ReviewOptions::default()
    });
    let headers = WebhookHeaders::from([
        ("X-GitHub-Event", "pull_request"),
        ("X-GitHub-Delivery", "delivery-1"),
    ]);
    let body_for = |sha: &str| {
        json!({
            "action": "synchronize",
            "repository": {
                "full_name": "maskdotdev/heimdaal"
            },
            "pull_request": {
                "number": 123,
                "head": {
                    "sha": sha
                }
            }
        })
        .to_string()
    };

    let first = workspace
        .handle_github_webhook(
            &headers,
            body_for("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").as_bytes(),
            None,
            options.clone(),
        )
        .await
        .unwrap();
    let duplicate = workspace
        .handle_github_webhook(
            &headers,
            body_for("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").as_bytes(),
            None,
            options.clone(),
        )
        .await
        .unwrap();
    let changed_head = workspace
        .handle_github_webhook(
            &headers,
            body_for("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").as_bytes(),
            None,
            options,
        )
        .await
        .unwrap();

    assert!(matches!(first, WebhookReviewDelivery::ReviewCreated { .. }));
    assert!(matches!(
        duplicate,
        WebhookReviewDelivery::ReviewDeduped { .. }
    ));
    assert!(matches!(
        changed_head,
        WebhookReviewDelivery::ReviewCreated { .. }
    ));
    assert!(store
        .get_by_dedupe_key(
            "source-head:github:maskdotdev/heimdaal#123@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        .await
        .unwrap()
        .is_some());
    assert!(store
        .get_by_dedupe_key(
            "source-head:github:maskdotdev/heimdaal#123@bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn github_webhook_ignores_unsupported_pull_request_action() {
    let workspace = Muzen::new().workspace("acme");
    let body = json!({
        "action": "closed",
        "repository": {
            "full_name": "maskdotdev/heimdaal"
        },
        "pull_request": {
            "number": 123
        }
    })
    .to_string();
    let headers = WebhookHeaders::from([
        ("X-GitHub-Event", "pull_request"),
        ("X-GitHub-Delivery", "delivery-1"),
    ]);

    let delivery = workspace
        .handle_github_webhook(
            &headers,
            body.as_bytes(),
            None,
            WebhookReviewOptions::default(),
        )
        .await
        .unwrap();

    match delivery {
        WebhookReviewDelivery::Ignored {
            reason,
            delivery_id,
        } => {
            assert!(reason.contains("closed"));
            assert_eq!(delivery_id.as_deref(), Some("delivery-1"));
        }
        delivery => panic!("expected ignored, got {delivery:?}"),
    }
}

#[tokio::test]
async fn github_webhook_rejects_invalid_signature() {
    let workspace = Muzen::new().workspace("acme");
    let body = json!({
        "action": "opened",
        "repository": {
            "full_name": "maskdotdev/heimdaal"
        },
        "pull_request": {
            "number": 123
        }
    })
    .to_string();
    let headers = WebhookHeaders::from([
        ("X-GitHub-Event", "pull_request"),
        ("X-GitHub-Delivery", "delivery-1"),
        ("X-Hub-Signature-256", "sha256=invalid"),
    ]);

    let error = workspace
        .handle_github_webhook(
            &headers,
            body.as_bytes(),
            Some("secret"),
            WebhookReviewOptions::default(),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("signature verification failed"));
}

#[tokio::test]
async fn gitlab_webhook_verifies_token_and_maps_merge_request() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let body = json!({
        "object_kind": "merge_request",
        "object_attributes": {
            "iid": 42,
            "action": "update",
            "last_commit": {
                "id": "cccccccccccccccccccccccccccccccccccccccc"
            }
        },
        "project": {
            "path_with_namespace": "platform/reviews/heimdaal"
        }
    })
    .to_string();
    let headers = WebhookHeaders::from([
        ("X-GitLab-Event", "Merge Request Hook"),
        ("X-GitLab-Token", "secret"),
        ("X-GitLab-Event-UUID", "delivery-2"),
    ]);

    let delivery = workspace
        .handle_gitlab_webhook(
            &headers,
            body.as_bytes(),
            Some("secret"),
            WebhookReviewOptions::default(),
        )
        .await
        .unwrap();
    let review = match delivery {
        WebhookReviewDelivery::ReviewCreated {
            review,
            delivery_id,
        } => {
            assert_eq!(delivery_id, "delivery-2");
            review
        }
        delivery => panic!("expected review_created, got {delivery:?}"),
    };
    let record = store.get(review.id()).await.unwrap().unwrap();

    assert_eq!(
        record.source,
        ReviewSource::GitlabMergeRequest {
            owner: "platform/reviews".to_string(),
            repo: "heimdaal".to_string(),
            number: 42
        }
    );
    assert_eq!(record.options.metadata["webhook.provider"], json!("gitlab"));
    assert_eq!(record.options.metadata["webhook.action"], json!("update"));
    assert_eq!(
        record.options.metadata["source.headSha"],
        json!("cccccccccccccccccccccccccccccccccccccccc")
    );
}

#[tokio::test]
async fn gitlab_webhook_rejects_invalid_token() {
    let workspace = Muzen::new().workspace("acme");
    let body = json!({
        "object_kind": "merge_request",
        "object_attributes": {
            "iid": 42,
            "action": "update"
        },
        "project": {
            "path_with_namespace": "platform/reviews/heimdaal"
        }
    })
    .to_string();
    let headers = WebhookHeaders::from([
        ("X-GitLab-Event", "Merge Request Hook"),
        ("X-GitLab-Token", "wrong"),
    ]);

    let error = workspace
        .handle_gitlab_webhook(
            &headers,
            body.as_bytes(),
            Some("secret"),
            WebhookReviewOptions::default(),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("token verification failed"));
}
