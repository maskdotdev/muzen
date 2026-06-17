use super::super::*;
use super::common::options_for_files;
use std::sync::Arc;

#[tokio::test]
async fn muzen_executes_local_review_session_and_waits_for_result() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let muzen = Muzen::new();

    let review = muzen
        .review_with_options(
            ReviewSource::local(repo.path()),
            options_for_files(["Cargo.toml"]),
        )
        .await
        .unwrap();
    let result = review.wait().unwrap();
    let event_types = review
        .events()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();

    assert_eq!(review.id().as_str(), "review-1");
    assert_eq!(review.status(), ReviewStatus::Completed);
    assert_eq!(result.status, ReviewStatus::Completed);
    assert!(result.summary.contains("Review completed"));
    assert!(event_types.contains(&ReviewEventType::SessionCompleted));
}

#[tokio::test]
async fn review_subscribe_replays_recorded_events() {
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
    let mut replayed = Vec::new();

    review.subscribe(|event| replayed.push(event.event_type));

    assert_eq!(replayed.len(), review.event_records().len());
    assert!(replayed.contains(&ReviewEventType::SessionStarted));
}

#[tokio::test]
async fn review_refresh_returns_snapshot_without_runner_details() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let review = Muzen::new()
        .review_with_options(
            ReviewSource::local(repo.path()),
            options_for_files(["README.md"]),
        )
        .await
        .unwrap();

    let snapshot = review.refresh();

    assert_eq!(snapshot.id.as_str(), "review-1");
    assert_eq!(snapshot.status, ReviewStatus::Completed);
    assert!(snapshot.result.is_some());
}

#[tokio::test]
async fn review_exports_and_reads_redacted_artifacts() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let review = Muzen::new()
        .review_with_options(
            ReviewSource::local(repo.path()),
            options_for_files(["Cargo.toml"]),
        )
        .await
        .unwrap();

    let exported = review
        .export_artifacts(ReviewArtifactExportOptions::default())
        .unwrap();
    let artifact = review
        .read_artifact(
            &exported.artifacts[0].artifact_id,
            ReviewArtifactReadOptions::default(),
        )
        .unwrap();

    assert!(exported.artifact_count > 0);
    assert_eq!(artifact.artifact_id, exported.artifacts[0].artifact_id);
    assert!(!artifact.content.is_empty());
}

#[tokio::test]
async fn review_artifact_export_enforces_limits() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let review = Muzen::new()
        .review_with_options(
            ReviewSource::local(repo.path()),
            options_for_files(["README.md"]),
        )
        .await
        .unwrap();

    let error = review
        .export_artifacts(ReviewArtifactExportOptions {
            max_artifacts: Some(0),
            ..ReviewArtifactExportOptions::default()
        })
        .unwrap_err();

    assert!(matches!(
        error,
        ReviewSessionError::ArtifactLimitExceeded { .. }
    ));
}

#[tokio::test]
async fn muzen_reuses_existing_session_for_source_dedupe() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let muzen = Muzen::with_store(store.clone());
    let options = ReviewOptions {
        dedupe: DedupePolicy::Source,
        ..ReviewOptions::default()
    };

    let first = muzen
        .review_with_options(
            ReviewSource::local(repo.path()),
            ReviewOptions {
                scope: options_for_files(["README.md"]).scope,
                ..options.clone()
            },
        )
        .await
        .unwrap();
    let second = muzen
        .review_with_options(
            ReviewSource::local(repo.path()),
            ReviewOptions {
                scope: options_for_files(["README.md"]).scope,
                ..options
            },
        )
        .await
        .unwrap();
    let record = store.get(first.id()).await.unwrap().unwrap();
    let expected_dedupe_key = format!("source:local:{}", repo.path().display());

    assert_eq!(first.id(), second.id());
    assert_eq!(
        record.dedupe_key.as_deref(),
        Some(expected_dedupe_key.as_str())
    );
}

#[tokio::test]
async fn project_schedule_review_persists_queued_record_with_options() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let project = Muzen::with_store(store.clone()).project("acme");

    let review = project
        .schedule_review_with_options(
            ReviewSource::local(repo.path()),
            ReviewOptions {
                user_id: Some("user-a".to_string()),
                dedupe: DedupePolicy::Source,
                scope: options_for_files(["README.md"]).scope,
                ..ReviewOptions::default()
            },
        )
        .await
        .unwrap();
    let record = store.get(review.id()).await.unwrap().unwrap();

    assert_eq!(review.status(), ReviewStatus::Queued);
    assert!(matches!(
        review.wait().unwrap_err(),
        ReviewSessionError::ResultUnavailable { .. }
    ));
    assert_eq!(record.project_id.as_deref(), Some("acme"));
    assert_eq!(record.user_id.as_deref(), Some("user-a"));
    assert_eq!(record.options.user_id.as_deref(), Some("user-a"));
    assert_eq!(record.status, ReviewStatus::Queued);
    assert!(record.result.is_none());
    assert_eq!(
        record.events.first().map(|event| event.event_type),
        Some(ReviewEventType::SessionQueued)
    );
}
