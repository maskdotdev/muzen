use super::super::*;
use super::common::*;
use crate::reviewer_kernel::system::timestamp_utc;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn review_store_persists_result_events_and_artifacts() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let review = Muzen::with_store(store.clone())
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["Cargo.toml"],
        ))
        .await
        .unwrap();
    let first_cursor = review.event_records()[0].cursor.clone();

    let record = store.get(review.id()).await.unwrap().unwrap();
    let replayed = store
        .events_after(review.id(), Some(&first_cursor))
        .await
        .unwrap();

    assert!(record.result.is_some());
    assert!(!record.events.is_empty());
    assert!(!record.redacted_artifacts.is_empty());
    assert_eq!(replayed.len(), record.events.len() - 1);
    assert_ne!(replayed[0].cursor, first_cursor);
}

#[tokio::test]
async fn review_store_can_append_events_and_update_result() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let review = Muzen::with_store(store.clone())
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
        .await
        .unwrap();
    let result = review.wait().unwrap();
    let extra_event = ReviewEvent {
        cursor: "manual-extra".to_string(),
        event_type: ReviewEventType::RunnerEvent,
        review_id: review.id().clone(),
        timestamp_utc: timestamp_utc(),
        payload: json!({"test": true}),
    };

    store
        .append_events(review.id(), vec![extra_event.clone()])
        .await
        .unwrap();
    store
        .write_result(review.id(), ReviewStatus::Completed, result)
        .await
        .unwrap();
    let record = store.get(review.id()).await.unwrap().unwrap();

    assert_eq!(record.events.last(), Some(&extra_event));
    assert_eq!(record.status, ReviewStatus::Completed);
    assert!(record.result.is_some());
}

#[tokio::test]
async fn review_store_claims_ready_sessions_with_project_concurrency() {
    let store = InMemoryReviewSessionStore::default();
    store
        .insert(queued_record("review-1", Some("acme"), 0))
        .await
        .unwrap();
    store
        .insert(queued_record("review-2", Some("acme"), 0))
        .await
        .unwrap();
    store
        .insert(queued_record("review-3", Some("beta"), 0))
        .await
        .unwrap();

    let claims = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 3,
            lease_seconds: 30,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits {
                max_running_per_project: Some(1),
                ..ReviewWorkerConcurrencyLimits::default()
            },
        })
        .await
        .unwrap();

    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].review_id.as_str(), "review-1");
    assert_eq!(claims[0].attempt, 1);
    assert_eq!(claims[0].lease.expires_at_unix_seconds, 130);
    assert_eq!(claims[1].review_id.as_str(), "review-3");
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-2").unwrap())
            .await
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-1").unwrap())
            .await
            .unwrap()
            .unwrap()
            .events
            .last()
            .map(|event| event.event_type),
        Some(ReviewEventType::SessionClaimed)
    );
}

#[tokio::test]
async fn review_store_extends_and_reclaims_leases() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .await
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();

    let blocked = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(105),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let extended = store
        .extend_lease(
            &review_id,
            ReviewLeaseExtension {
                worker_id: "worker-a".to_string(),
                lease_seconds: 20,
                now_unix_seconds: Some(106),
            },
        )
        .await
        .unwrap();
    let reclaimed = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(127),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();

    assert!(blocked.is_empty());
    assert_eq!(extended.expires_at_unix_seconds, 126);
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].attempt, 2);
    assert_eq!(reclaimed[0].worker_id, "worker-b");
}

#[tokio::test]
async fn review_store_durable_cancellation_clears_lease_and_blocks_claims() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .await
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();

    let cancelled = store
        .request_cancellation(&review_id, ReviewCancelOptions::new("superseded"))
        .await
        .unwrap();
    let later_claims = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(111),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();

    assert_eq!(cancelled.status, ReviewStatus::Cancelled);
    assert!(cancelled.lease.is_none());
    assert_eq!(
        cancelled
            .cancellation
            .as_ref()
            .and_then(|cancellation| cancellation.reason.as_deref()),
        Some("superseded")
    );
    assert_eq!(
        cancelled.events.last().map(|event| event.event_type),
        Some(ReviewEventType::SessionCancelled)
    );
    assert!(later_claims.is_empty());
}

#[tokio::test]
async fn review_store_preserves_cancellation_against_late_execution_result() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .await
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    store
        .request_cancellation(&review_id, ReviewCancelOptions::new("superseded"))
        .await
        .unwrap();
    let late_result = ReviewResult {
        review_id: review_id.clone(),
        session_id: review_id.clone(),
        status: ReviewStatus::Completed,
        conclusion: ReviewConclusion::Approved,
        summary: "late result".to_string(),
        findings: Vec::new(),
        coverage: ReviewCoverage {
            files_considered: 0,
            files_reviewed: 0,
            files_skipped: 0,
        },
        metadata: BTreeMap::new(),
    };

    let updated = store
        .write_execution_result(
            &review_id,
            ReviewStatus::Completed,
            late_result,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await
        .unwrap();

    assert_eq!(updated.status, ReviewStatus::Cancelled);
    assert!(updated.result.is_none());
    assert_eq!(
        updated.events.last().map(|event| event.event_type),
        Some(ReviewEventType::SessionCancelled)
    );
}

#[tokio::test]
async fn review_store_records_retry_backoff_and_final_failure() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    let retry_policy = ReviewRetryPolicy {
        max_attempts: 2,
        initial_backoff_seconds: 10,
        max_backoff_seconds: 50,
    };
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .await
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();

    let retry = store
        .record_attempt_failure(
            &review_id,
            ReviewAttemptFailure {
                error: "provider timeout".to_string(),
                retry_policy,
                now_unix_seconds: Some(110),
            },
        )
        .await
        .unwrap();
    let not_ready = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(119),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let second_attempt = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(120),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let failed = store
        .record_attempt_failure(
            &review_id,
            ReviewAttemptFailure {
                error: "provider timeout again".to_string(),
                retry_policy,
                now_unix_seconds: Some(130),
            },
        )
        .await
        .unwrap();

    assert_eq!(retry.status, ReviewStatus::Queued);
    assert_eq!(retry.run_after_unix_seconds, 120);
    assert_eq!(retry.last_error.as_deref(), Some("provider timeout"));
    assert!(not_ready.is_empty());
    assert_eq!(second_attempt[0].attempt, 2);
    assert_eq!(failed.status, ReviewStatus::Failed);
    assert!(failed.lease.is_none());
    assert_eq!(
        failed.events.last().map(|event| event.event_type),
        Some(ReviewEventType::SessionFailed)
    );
}

#[tokio::test]
async fn review_store_enforces_global_running_limit() {
    let store = InMemoryReviewSessionStore::default();
    store
        .insert(queued_record("review-1", Some("acme"), 0))
        .await
        .unwrap();
    store
        .insert(queued_record("review-2", Some("beta"), 0))
        .await
        .unwrap();

    let claims = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 2,
            lease_seconds: 30,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits {
                max_running_global: Some(1),
                ..ReviewWorkerConcurrencyLimits::default()
            },
        })
        .await
        .unwrap();

    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].review_id.as_str(), "review-1");
}

#[tokio::test]
async fn review_store_enforces_user_model_and_provider_running_limits() {
    let store = InMemoryReviewSessionStore::default();
    store
        .insert(queued_record_with_keys(
            "review-1",
            Some("acme"),
            Some("user-a"),
            Some("model-a"),
            Some("provider-a"),
        ))
        .await
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-2",
            Some("beta"),
            Some("user-a"),
            Some("model-b"),
            Some("provider-b"),
        ))
        .await
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-3",
            Some("gamma"),
            Some("user-b"),
            Some("model-a"),
            Some("provider-c"),
        ))
        .await
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-4",
            Some("delta"),
            Some("user-c"),
            Some("model-c"),
            Some("provider-a"),
        ))
        .await
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-5",
            Some("epsilon"),
            Some("user-d"),
            Some("model-d"),
            Some("provider-d"),
        ))
        .await
        .unwrap();

    let claims = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 5,
            lease_seconds: 30,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits {
                max_running_per_user: Some(1),
                max_running_per_model_profile: Some(1),
                max_running_per_provider_profile: Some(1),
                ..ReviewWorkerConcurrencyLimits::default()
            },
        })
        .await
        .unwrap();
    let claimed_ids = claims
        .iter()
        .map(|claim| claim.review_id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(claimed_ids, vec!["review-1", "review-5"]);
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-2").unwrap())
            .await
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-3").unwrap())
            .await
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-4").unwrap())
            .await
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
}

#[tokio::test]
async fn review_session_logs_are_redacted_before_persistence() {
    let raw_secret = "sk-live-raw-secret";
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let project = Muzen::with_store(store.clone()).project("acme");
    let review = project
        .schedule_review(ReviewSource::local_with_changed_files(".", ["Cargo.toml"]))
        .await
        .unwrap();

    store
        .append_logs(
            review.id(),
            vec![ReviewLogEntry::new(
                review.id().clone(),
                ReviewLogStream::Worker,
                format!("resolved credential {raw_secret} for model call"),
            )
            .with_metadata("apiKey", json!(raw_secret))
            .with_metadata(
                "nested",
                json!({
                    "authorization": format!("Bearer {raw_secret}"),
                    "safe": format!("prefix-{raw_secret}-suffix")
                }),
            )],
            ReviewLogRedactionPolicy::new([raw_secret]),
        )
        .await
        .unwrap();
    let logs = store.logs_after(review.id(), None).await.unwrap();
    let record = store.get(review.id()).await.unwrap().unwrap();
    let logs_json = serde_json::to_string(&logs).unwrap();
    let record_json = serde_json::to_string(&record).unwrap();

    assert_eq!(logs[0].cursor, "1");
    assert_eq!(logs[0].review_id, review.id().clone());
    assert!(logs_json.contains("[redacted]"));
    assert!(!logs_json.contains(raw_secret));
    assert!(!record_json.contains(raw_secret));
    assert_eq!(logs[0].metadata["apiKey"], json!("[redacted]"));
    assert_eq!(
        logs[0].metadata["nested"]["authorization"],
        json!("[redacted]")
    );
    assert_eq!(
        logs[0].metadata["nested"]["safe"],
        json!("prefix-[redacted]-suffix")
    );
}

#[tokio::test]
async fn review_session_store_conformance_memory() {
    assert_review_session_store_conformance(Arc::new(InMemoryReviewSessionStore::default())).await;
}

#[tokio::test]
async fn review_session_store_conformance_libsql() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        LibsqlReviewSessionStore::connect(temp.path().join("muzen.db"))
            .await
            .unwrap(),
    );

    assert_review_session_store_conformance(store).await;
}

#[tokio::test]
async fn libsql_store_factory_creates_sqlite_parent_directory() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join(".muzen").join("muzen.db");
    let store_url = format!("sqlite://{}", db_path.display());

    let stores = stores_from_url(&store_url).await.unwrap();
    stores
        .session_store
        .insert(queued_record("review-1", Some("acme"), 0))
        .await
        .unwrap();
    let record = stores
        .session_store
        .get(&ReviewSessionId::new("review-1").unwrap())
        .await
        .unwrap()
        .unwrap();

    assert!(db_path.exists());
    assert_eq!(record.project_id.as_deref(), Some("acme"));
}

#[tokio::test]
async fn libsql_review_store_reopens_persisted_review() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("muzen.db");
    let review_id = ReviewSessionId::new("review-1").unwrap();
    let mut record = queued_record(review_id.as_str(), Some("acme"), 0);
    record.dedupe_key = Some("source:local:.".to_string());
    record.events.push(ReviewEvent {
        cursor: "1".to_string(),
        event_type: ReviewEventType::SessionQueued,
        review_id: review_id.clone(),
        timestamp_utc: timestamp_utc(),
        payload: json!({}),
    });
    record.logs.push(ReviewLogEntry::new(
        review_id.clone(),
        ReviewLogStream::System,
        "queued",
    ));
    record.logs.push(ReviewLogEntry::new(
        review_id.clone(),
        ReviewLogStream::ToolStdout,
        "running",
    ));
    record.redacted_artifacts.push(ReviewArtifact {
        artifact_id: "artifact-1".to_string(),
        bytes: 2,
        content_hash: "hash".to_string(),
        content: "{}".to_string(),
    });
    record.redacted_artifacts.push(ReviewArtifact {
        artifact_id: "artifact-2".to_string(),
        bytes: 4,
        content_hash: "hash-2".to_string(),
        content: "{\"ok\":true}".to_string(),
    });

    let first = LibsqlReviewSessionStore::connect(&db_path).await.unwrap();
    first.insert(record).await.unwrap();
    drop(first);

    let reopened = LibsqlReviewSessionStore::connect(&db_path).await.unwrap();
    let loaded = reopened.get(&review_id).await.unwrap().unwrap();
    let deduped = reopened
        .get_by_dedupe_key("source:local:.")
        .await
        .unwrap()
        .unwrap();
    let duplicate_error = reopened
        .insert({
            let mut duplicate = queued_record("review-2", Some("acme"), 0);
            duplicate.dedupe_key = Some("source:local:.".to_string());
            duplicate
        })
        .await
        .unwrap_err();

    assert_eq!(loaded.events.len(), 1);
    assert_eq!(
        loaded
            .logs
            .iter()
            .map(|log| log.message.as_str())
            .collect::<Vec<_>>(),
        vec!["queued", "running"]
    );
    assert_eq!(
        loaded
            .redacted_artifacts
            .iter()
            .map(|artifact| artifact.artifact_id.as_str())
            .collect::<Vec<_>>(),
        vec!["artifact-1", "artifact-2"]
    );
    assert_eq!(deduped.id, review_id);
    assert!(
        duplicate_error.to_string().contains("dedupe_key")
            || duplicate_error.to_string().contains("UNIQUE")
    );
}

#[tokio::test]
async fn libsql_claim_ready_does_not_double_claim_across_connections() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("muzen.db");
    let first = Arc::new(LibsqlReviewSessionStore::connect(&db_path).await.unwrap());
    let second = Arc::new(LibsqlReviewSessionStore::connect(&db_path).await.unwrap());
    first
        .insert(queued_record("review-1", Some("acme"), 0))
        .await
        .unwrap();

    let first_worker = first.clone();
    let second_worker = second.clone();
    let first_claim = tokio::spawn(async move {
        first_worker
            .claim_ready(ReviewWorkerClaimOptions {
                worker_id: "worker-a".to_string(),
                max_sessions: 1,
                lease_seconds: 30,
                now_unix_seconds: Some(100),
                concurrency: ReviewWorkerConcurrencyLimits::default(),
            })
            .await
            .unwrap()
    });
    let second_claim = tokio::spawn(async move {
        second_worker
            .claim_ready(ReviewWorkerClaimOptions {
                worker_id: "worker-b".to_string(),
                max_sessions: 1,
                lease_seconds: 30,
                now_unix_seconds: Some(100),
                concurrency: ReviewWorkerConcurrencyLimits::default(),
            })
            .await
            .unwrap()
    });

    let first_claim = first_claim.await.unwrap();
    let second_claim = second_claim.await.unwrap();
    let total_claims = first_claim.len() + second_claim.len();
    let record = first
        .get(&ReviewSessionId::new("review-1").unwrap())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(total_claims, 1);
    assert_eq!(record.status, ReviewStatus::Running);
    assert_eq!(record.attempt, 1);
}

async fn assert_review_session_store_conformance(store: Arc<dyn ReviewSessionStore>) {
    let review_id = ReviewSessionId::new("conformance-basic").unwrap();
    let mut record = queued_record(review_id.as_str(), Some("acme"), 0);
    record.dedupe_key = Some("source:conformance-basic".to_string());
    store.insert(record).await.unwrap();

    let loaded = store.get(&review_id).await.unwrap().unwrap();
    let deduped = store
        .get_by_dedupe_key("source:conformance-basic")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.id, review_id);
    assert_eq!(deduped.id, review_id);

    store
        .append_events(
            &review_id,
            vec![
                ReviewEvent {
                    cursor: "1".to_string(),
                    event_type: ReviewEventType::SessionQueued,
                    review_id: review_id.clone(),
                    timestamp_utc: timestamp_utc(),
                    payload: json!({"step": 1}),
                },
                ReviewEvent {
                    cursor: "2".to_string(),
                    event_type: ReviewEventType::RunnerEvent,
                    review_id: review_id.clone(),
                    timestamp_utc: timestamp_utc(),
                    payload: json!({"step": 2}),
                },
            ],
        )
        .await
        .unwrap();
    let events = store.events_after(&review_id, None).await.unwrap();
    let events_after_first = store.events_after(&review_id, Some("1")).await.unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.cursor.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "2"]
    );
    assert_eq!(events_after_first[0].cursor, "2");

    store
        .append_logs(
            &review_id,
            vec![
                ReviewLogEntry::new(review_id.clone(), ReviewLogStream::System, "first"),
                ReviewLogEntry::new(review_id.clone(), ReviewLogStream::ToolStdout, "second"),
            ],
            ReviewLogRedactionPolicy::default(),
        )
        .await
        .unwrap();
    let logs = store.logs_after(&review_id, None).await.unwrap();
    let logs_after_first = store.logs_after(&review_id, Some("1")).await.unwrap();
    assert_eq!(
        logs.iter()
            .map(|log| (log.cursor.as_str(), log.message.as_str()))
            .collect::<Vec<_>>(),
        vec![("1", "first"), ("2", "second")]
    );
    assert_eq!(logs_after_first[0].message, "second");

    store
        .write_result(
            &review_id,
            ReviewStatus::Completed,
            completed_result(&review_id, "conformance complete"),
        )
        .await
        .unwrap();
    let completed = store.get(&review_id).await.unwrap().unwrap();
    assert_eq!(completed.status, ReviewStatus::Completed);
    assert_eq!(
        completed
            .result
            .as_ref()
            .map(|result| result.summary.as_str()),
        Some("conformance complete")
    );

    let lease_id = ReviewSessionId::new("conformance-lease").unwrap();
    store
        .insert(queued_record(lease_id.as_str(), Some("acme"), 0))
        .await
        .unwrap();
    let first_claim = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let blocked_claim = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(105),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let reclaimed = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(111),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    assert_eq!(first_claim[0].attempt, 1);
    assert!(blocked_claim.is_empty());
    assert_eq!(reclaimed[0].attempt, 2);
    store
        .request_cancellation(&lease_id, ReviewCancelOptions::new("conformance done"))
        .await
        .unwrap();

    let retry_id = ReviewSessionId::new("conformance-retry").unwrap();
    store
        .insert(queued_record(retry_id.as_str(), Some("acme"), 0))
        .await
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(200),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let retry = store
        .record_attempt_failure(
            &retry_id,
            ReviewAttemptFailure {
                error: "temporary outage".to_string(),
                retry_policy: ReviewRetryPolicy {
                    max_attempts: 3,
                    initial_backoff_seconds: 10,
                    max_backoff_seconds: 60,
                },
                now_unix_seconds: Some(210),
            },
        )
        .await
        .unwrap();
    let not_ready = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(219),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let retry_claim = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(220),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    assert_eq!(retry.status, ReviewStatus::Queued);
    assert_eq!(retry.run_after_unix_seconds, 220);
    assert!(not_ready.is_empty());
    assert_eq!(retry_claim[0].attempt, 2);
    store
        .request_cancellation(&retry_id, ReviewCancelOptions::new("conformance done"))
        .await
        .unwrap();

    let cancel_id = ReviewSessionId::new("conformance-cancel").unwrap();
    store
        .insert(queued_record(cancel_id.as_str(), Some("acme"), 0))
        .await
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(300),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    let cancelled = store
        .request_cancellation(&cancel_id, ReviewCancelOptions::new("superseded"))
        .await
        .unwrap();
    let late = store
        .write_execution_result(
            &cancel_id,
            ReviewStatus::Completed,
            completed_result(&cancel_id, "late"),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await
        .unwrap();
    let later_claim = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(320),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .await
        .unwrap();
    assert_eq!(cancelled.status, ReviewStatus::Cancelled);
    assert_eq!(late.status, ReviewStatus::Cancelled);
    assert!(late.result.is_none());
    assert!(later_claim.is_empty());
}
