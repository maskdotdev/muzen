use super::super::*;
use crate::remote_http::ContextHttpRouterOptions;
use serde_json::{json, Value};

#[tokio::test]
async fn review_http_router_serves_project_context_routes() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src/auth")).unwrap();
    std::fs::write(
        repo.path().join("src/auth/token.rs"),
        "pub fn authorize_request() {}\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.path().join("tests/auth")).unwrap();
    std::fs::write(
        repo.path().join("tests/auth/token_test.rs"),
        "#[tokio::test]\nfn authorize_request_test() {}\n",
    )
    .unwrap();
    let router = ReviewHttpRouter::new(Muzen::new());
    let source = json!({
        "type": "local",
        "repo": repo.path(),
        "changed_files": ["src/auth/token.rs"]
    });

    let index_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/index")
                .json(&json!({ "source": source }))
                .unwrap(),
        )
        .await;
    let pack_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/packs")
                .json(&json!({
                    "source": source,
                    "purpose": "tests"
                }))
                .unwrap(),
        )
        .await;
    let query_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/query")
                .json(&json!({
                    "source": source,
                    "kind": "related_tests",
                    "arguments": { "path": "src/auth/token.rs" },
                    "limits": { "maxResults": 10, "maxTokens": 1000 }
                }))
                .unwrap(),
        )
        .await;
    let feedback_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/feedback")
                .json(&json!({
                    "source": source,
                    "feedback": "Suppress duplicate generated auth wrapper warning."
                }))
                .unwrap(),
        )
        .await;
    let index_body: Value = serde_json::from_str(&index_response.body).unwrap();
    let pack_body: Value = serde_json::from_str(&pack_response.body).unwrap();
    let query_body: Value = serde_json::from_str(&query_response.body).unwrap();
    let feedback_body: Value = serde_json::from_str(&feedback_response.body).unwrap();
    let snapshot_id = index_body["manifest"]["snapshotId"].as_str().unwrap();
    let learning_id = feedback_body["receipt"]["proposedLearning"]["id"]
        .as_str()
        .unwrap();
    let approval_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/learnings/approve")
                .json(&json!({
                    "snapshotId": snapshot_id,
                    "learningId": learning_id,
                    "approve": true
                }))
                .unwrap(),
        )
        .await;
    let approval_body: Value = serde_json::from_str(&approval_response.body).unwrap();

    assert_eq!(index_response.status_code, HTTP_STATUS_OK);
    assert_eq!(
        index_body["manifest"]["schemaVersion"],
        json!("muzen.context_manifest.v1")
    );
    assert_eq!(pack_response.status_code, HTTP_STATUS_OK);
    assert_eq!(pack_body["pack"]["purpose"], json!("tests"));
    assert!(pack_body["pack"]["evidence"].as_array().unwrap().len() >= 2);
    assert_eq!(query_response.status_code, HTTP_STATUS_OK);
    assert_eq!(query_body["result"]["kind"], json!("related_tests"));
    assert!(query_body["result"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["path"] == json!("tests/auth/token_test.rs")));
    assert_eq!(feedback_response.status_code, HTTP_STATUS_OK);
    assert_eq!(
        feedback_body["receipt"]["proposedLearning"]["status"],
        json!("proposed")
    );
    assert_eq!(approval_response.status_code, HTTP_STATUS_OK);
    assert_eq!(
        approval_body["receipt"]["learning"]["status"],
        json!("approved")
    );
}

#[tokio::test]
async fn review_http_router_persists_project_context_learnings() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let learning_root = tempfile::tempdir().unwrap();
    let router = ReviewHttpRouter::with_options(
        Muzen::new(),
        ReviewHttpRouterOptions {
            github_webhook_secret: None,
            gitlab_webhook_secret: None,
            context: ContextHttpRouterOptions {
                learning_store_root: Some(learning_root.path().to_path_buf()),
                derived_cache_root: None,
            },
        },
    );
    let source = json!({
        "type": "local",
        "repo": repo.path(),
        "changed_files": ["lib.rs"]
    });

    let index_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/index")
                .json(&json!({ "source": source }))
                .unwrap(),
        )
        .await;
    let feedback_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/feedback")
                .json(&json!({
                    "source": source,
                    "feedback": "Remember generated wrappers in this repository are intentional."
                }))
                .unwrap(),
        )
        .await;
    let index_body: Value = serde_json::from_str(&index_response.body).unwrap();
    let feedback_body: Value = serde_json::from_str(&feedback_response.body).unwrap();
    let snapshot_id = index_body["manifest"]["snapshotId"].as_str().unwrap();
    let learning_id = feedback_body["receipt"]["proposedLearning"]["id"]
        .as_str()
        .unwrap();
    let approval_response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/learnings/approve")
                .json(&json!({
                    "snapshotId": snapshot_id,
                    "learningId": learning_id,
                    "approve": true
                }))
                .unwrap(),
        )
        .await;
    assert_eq!(approval_response.status_code, HTTP_STATUS_OK);

    let restarted = ReviewHttpRouter::with_options(
        Muzen::new(),
        ReviewHttpRouterOptions {
            github_webhook_secret: None,
            gitlab_webhook_secret: None,
            context: ContextHttpRouterOptions {
                learning_store_root: Some(learning_root.path().to_path_buf()),
                derived_cache_root: None,
            },
        },
    );
    let history_response = restarted
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/query")
                .json(&json!({
                    "source": source,
                    "kind": "history_similar",
                    "arguments": { "query": "generated wrappers" },
                    "limits": { "maxResults": 10, "maxTokens": 1000 }
                }))
                .unwrap(),
        )
        .await;
    let history_body: Value = serde_json::from_str(&history_response.body).unwrap();

    assert_eq!(history_response.status_code, HTTP_STATUS_OK);
    assert_eq!(
        history_body["result"]["data"]["learnings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(learning_root
        .path()
        .join("acme")
        .join("context-learnings.json")
        .exists());
}

#[tokio::test]
async fn review_http_router_context_cross_repo_contracts_require_grants() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let router = ReviewHttpRouter::new(Muzen::new());
    let source = json!({
        "type": "local",
        "repo": repo.path(),
        "changed_files": ["lib.rs"]
    });
    let candidate = json!({
        "resourceId": "github/acme/mobile",
        "repository": "acme/mobile",
        "summary": "consumer requires expires_at on auth token response",
        "originalUrl": "https://example.invalid/acme/mobile/contracts/auth"
    });

    let denied = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/query")
                .json(&json!({
                    "source": source,
                    "kind": "cross_repo_contracts",
                    "arguments": { "query": "expires_at" },
                    "crossRepoContracts": [candidate],
                    "limits": { "maxResults": 10, "maxTokens": 1000 }
                }))
                .unwrap(),
        )
        .await;
    let denied_body: Value = serde_json::from_str(&denied.body).unwrap();
    assert_eq!(denied.status_code, HTTP_STATUS_OK);
    assert!(denied_body["result"]["evidence"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        denied_body["result"]["data"]["omissions"][0]["deniedCandidates"],
        json!(1)
    );

    let granted = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/query")
                .json(&json!({
                    "source": source,
                    "kind": "cross_repo_contracts",
                    "arguments": { "query": "expires_at" },
                    "crossRepoContracts": [candidate],
                    "allowedCrossRepoResources": ["github/acme/mobile"],
                    "limits": { "maxResults": 10, "maxTokens": 1000 }
                }))
                .unwrap(),
        )
        .await;
    let granted_body: Value = serde_json::from_str(&granted.body).unwrap();
    assert_eq!(granted.status_code, HTTP_STATUS_OK);
    assert_eq!(
        granted_body["result"]["evidence"][0]["source"],
        json!("external")
    );
    assert_eq!(
        granted_body["result"]["evidence"][0]["trust"],
        json!("tool_provider")
    );
}

#[tokio::test]
async fn review_http_router_rejects_provider_context_sources_without_materialization() {
    let router = ReviewHttpRouter::new(Muzen::new());
    let response = router
        .handle(
            ReviewHttpRequest::new("POST", "/v1/projects/acme/context/index")
                .json(&json!({
                    "source": {
                        "type": "github_pull_request",
                        "owner": "maskdotdev",
                        "repo": "muzen",
                        "number": 1
                    }
                }))
                .unwrap(),
        )
        .await;

    assert_eq!(response.status_code, HTTP_STATUS_BAD_REQUEST);
    assert!(response.body.contains("local or raw_snapshot source"));
}
