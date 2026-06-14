use super::super::*;
use std::sync::Arc;

#[tokio::test]
async fn review_worker_executes_claimed_local_review_and_persists_result() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let review = workspace
        .schedule_review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
        .await
        .unwrap();
    let worker = ReviewWorker::new("worker-a", store.clone(), HostConfiguration::default());

    let run = worker.run_once(1).await.unwrap();
    let record = store.get(review.id()).await.unwrap().unwrap();
    let event_types = record
        .events
        .iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();

    assert_eq!(run.claimed, 1);
    assert_eq!(run.completed, 1);
    assert_eq!(record.status, ReviewStatus::Completed);
    assert!(record.result.is_some());
    assert!(!record.redacted_artifacts.is_empty());
    assert!(record.lease.is_none());
    assert!(event_types.contains(&ReviewEventType::SessionQueued));
    assert!(event_types.contains(&ReviewEventType::SessionClaimed));
    assert!(event_types.contains(&ReviewEventType::SessionCompleted));
    assert_eq!(
        record
            .events
            .iter()
            .map(|event| event.cursor.clone())
            .collect::<Vec<_>>(),
        (1..=record.events.len())
            .map(|cursor| cursor.to_string())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn review_worker_records_final_failure_for_execution_error() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let repo = tempfile::tempdir().expect("temp repo");
    let review = workspace
        .schedule_review(ReviewSource::local(repo.path()))
        .await
        .unwrap();
    let worker = ReviewWorker::new(
        "worker-a",
        store.clone(),
        HostConfiguration {
            scheduling: HostSchedulingConfiguration {
                default_retry_policy: ReviewRetryPolicy {
                    max_attempts: 1,
                    initial_backoff_seconds: 1,
                    max_backoff_seconds: 1,
                },
                ..HostSchedulingConfiguration::default()
            },
        },
    );

    let run = worker.run_once(1).await.unwrap();
    let record = store.get(review.id()).await.unwrap().unwrap();

    assert_eq!(run.claimed, 1);
    assert_eq!(run.failed, 1);
    assert_eq!(record.status, ReviewStatus::Failed);
    assert!(record.lease.is_none());
    assert!(record.last_error.as_deref().is_some_and(|error| {
        error.contains(
            "run requires at least one changed file that exists in the materialized worktree",
        )
    }));
    assert_eq!(
        record.events.last().map(|event| event.event_type),
        Some(ReviewEventType::SessionFailed)
    );
}
