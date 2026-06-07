use super::*;
use crate::contracts::Role;
use crate::runner::{
    RunnerFinding, RunnerFindingLocation, RunnerRunResult, RunnerRunSummary, RunnerSnapshotSummary,
    RUNNER_PROTOCOL_VERSION,
};
use crate::util::timestamp_utc;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

#[test]
fn parses_github_source_shorthand() {
    let source = ReviewSource::from_str("github:maskdotdev/heimdaal#123").unwrap();

    assert_eq!(
        source,
        ReviewSource::GithubPullRequest {
            owner: "maskdotdev".to_string(),
            repo: "heimdaal".to_string(),
            number: 123
        }
    );
    assert_eq!(source.source_key(), "github:maskdotdev/heimdaal#123");
}

#[test]
fn parses_gitlab_source_shorthand_with_nested_owner() {
    let source = ReviewSource::from_str("gitlab:platform/reviews/heimdaal!42").unwrap();

    assert_eq!(
        source,
        ReviewSource::GitlabMergeRequest {
            owner: "platform/reviews".to_string(),
            repo: "heimdaal".to_string(),
            number: 42
        }
    );
    assert_eq!(source.source_key(), "gitlab:platform/reviews/heimdaal!42");
}

#[test]
fn parses_raw_snapshot_source_shorthand() {
    let source = ReviewSource::from_str("raw_snapshot:/tmp/muzen-snapshot").unwrap();

    assert_eq!(source, ReviewSource::raw_snapshot("/tmp/muzen-snapshot"));
    assert_eq!(source.source_key(), "raw_snapshot:/tmp/muzen-snapshot");
}

#[test]
fn builds_non_git_provider_sources() {
    let perforce = ReviewSource::perforce_changelist("perforce.example:1666", "12345").unwrap();
    let custom = ReviewSource::custom("acme", "review-123").unwrap();

    assert_eq!(
        perforce.source_key(),
        "perforce:perforce.example:1666@12345"
    );
    assert_eq!(custom.source_key(), "custom:acme:review-123");
}

#[test]
fn rejects_invalid_source_shorthand() {
    let error = ReviewSource::from_str("github:maskdotdev/heimdaal").unwrap_err();

    assert!(error
        .to_string()
        .contains("missing `#` review number delimiter"));
}






#[test]
fn muzen_executes_local_review_session_and_waits_for_result() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let muzen = Muzen::new();

    let review = muzen
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["Cargo.toml"],
        ))
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

#[test]
fn review_subscribe_replays_recorded_events() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let muzen = Muzen::new();
    let review = muzen
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
        .unwrap();
    let mut replayed = Vec::new();

    review.subscribe(|event| replayed.push(event.event_type));

    assert_eq!(replayed.len(), review.event_records().len());
    assert!(replayed.contains(&ReviewEventType::SessionStarted));
}

#[test]
fn review_refresh_returns_snapshot_without_runner_details() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let review = Muzen::new()
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
        .unwrap();

    let snapshot = review.refresh();

    assert_eq!(snapshot.id.as_str(), "review-1");
    assert_eq!(snapshot.status, ReviewStatus::Completed);
    assert!(snapshot.result.is_some());
}

#[test]
fn review_exports_and_reads_redacted_artifacts() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    let review = Muzen::new()
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["Cargo.toml"],
        ))
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

#[test]
fn review_artifact_export_enforces_limits() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let review = Muzen::new()
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
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

#[test]
fn muzen_reuses_existing_session_for_source_dedupe() {
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
            ReviewSource::local_with_changed_files(repo.path(), ["README.md"]),
            options.clone(),
        )
        .unwrap();
    let second = muzen
        .review_with_options(
            ReviewSource::local_with_changed_files(repo.path(), ["README.md"]),
            options,
        )
        .unwrap();
    let record = store.get(first.id()).unwrap().unwrap();
    let expected_dedupe_key = format!("source:local:{}", repo.path().display());

    assert_eq!(first.id(), second.id());
    assert_eq!(
        record.dedupe_key.as_deref(),
        Some(expected_dedupe_key.as_str())
    );
}

#[test]
fn review_store_persists_result_events_and_artifacts() {
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
        .unwrap();
    let first_cursor = review.event_records()[0].cursor.clone();

    let record = store.get(review.id()).unwrap().unwrap();
    let replayed = store
        .events_after(review.id(), Some(&first_cursor))
        .unwrap();

    assert!(record.result.is_some());
    assert!(!record.events.is_empty());
    assert!(!record.redacted_artifacts.is_empty());
    assert_eq!(replayed.len(), record.events.len() - 1);
    assert_ne!(replayed[0].cursor, first_cursor);
}

#[test]
fn review_store_can_append_events_and_update_result() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let review = Muzen::with_store(store.clone())
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
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
        .unwrap();
    store
        .write_result(review.id(), ReviewStatus::Completed, result)
        .unwrap();
    let record = store.get(review.id()).unwrap().unwrap();

    assert_eq!(record.events.last(), Some(&extra_event));
    assert_eq!(record.status, ReviewStatus::Completed);
    assert!(record.result.is_some());
}

#[test]
fn review_store_claims_ready_sessions_with_workspace_concurrency() {
    let store = InMemoryReviewSessionStore::default();
    store
        .insert(queued_record("review-1", Some("acme"), 0))
        .unwrap();
    store
        .insert(queued_record("review-2", Some("acme"), 0))
        .unwrap();
    store
        .insert(queued_record("review-3", Some("beta"), 0))
        .unwrap();

    let claims = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 3,
            lease_seconds: 30,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits {
                max_running_per_workspace: Some(1),
                ..ReviewWorkerConcurrencyLimits::default()
            },
        })
        .unwrap();

    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].review_id.as_str(), "review-1");
    assert_eq!(claims[0].attempt, 1);
    assert_eq!(claims[0].lease.expires_at_unix_seconds, 130);
    assert_eq!(claims[1].review_id.as_str(), "review-3");
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-2").unwrap())
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-1").unwrap())
            .unwrap()
            .unwrap()
            .events
            .last()
            .map(|event| event.event_type),
        Some(ReviewEventType::SessionClaimed)
    );
}

#[test]
fn review_store_extends_and_reclaims_leases() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .unwrap();

    let blocked = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(105),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
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
        .unwrap();
    let reclaimed = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(127),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .unwrap();

    assert!(blocked.is_empty());
    assert_eq!(extended.expires_at_unix_seconds, 126);
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].attempt, 2);
    assert_eq!(reclaimed[0].worker_id, "worker-b");
}

#[test]
fn review_store_durable_cancellation_clears_lease_and_blocks_claims() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .unwrap();

    let cancelled = store
        .request_cancellation(&review_id, ReviewCancelOptions::new("superseded"))
        .unwrap();
    let later_claims = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-b".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(111),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
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

#[test]
fn review_store_preserves_cancellation_against_late_execution_result() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .unwrap();
    store
        .request_cancellation(&review_id, ReviewCancelOptions::new("superseded"))
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
        .unwrap();

    assert_eq!(updated.status, ReviewStatus::Cancelled);
    assert!(updated.result.is_none());
    assert_eq!(
        updated.events.last().map(|event| event.event_type),
        Some(ReviewEventType::SessionCancelled)
    );
}

#[test]
fn review_store_records_retry_backoff_and_final_failure() {
    let store = InMemoryReviewSessionStore::default();
    let review_id = ReviewSessionId::new("review-1").unwrap();
    let retry_policy = ReviewRetryPolicy {
        max_attempts: 2,
        initial_backoff_seconds: 10,
        max_backoff_seconds: 50,
    };
    store
        .insert(queued_record(review_id.as_str(), Some("acme"), 0))
        .unwrap();
    store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(100),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
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
        .unwrap();
    let not_ready = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(119),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
        .unwrap();
    let second_attempt = store
        .claim_ready(ReviewWorkerClaimOptions {
            worker_id: "worker-a".to_string(),
            max_sessions: 1,
            lease_seconds: 10,
            now_unix_seconds: Some(120),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
        })
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


#[test]
fn review_store_enforces_global_running_limit() {
    let store = InMemoryReviewSessionStore::default();
    store
        .insert(queued_record("review-1", Some("acme"), 0))
        .unwrap();
    store
        .insert(queued_record("review-2", Some("beta"), 0))
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
        .unwrap();

    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].review_id.as_str(), "review-1");
}

#[test]
fn review_store_enforces_user_model_and_provider_running_limits() {
    let store = InMemoryReviewSessionStore::default();
    store
        .insert(queued_record_with_keys(
            "review-1",
            Some("acme"),
            Some("user-a"),
            Some("model-a"),
            Some("provider-a"),
        ))
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-2",
            Some("beta"),
            Some("user-a"),
            Some("model-b"),
            Some("provider-b"),
        ))
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-3",
            Some("gamma"),
            Some("user-b"),
            Some("model-a"),
            Some("provider-c"),
        ))
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-4",
            Some("delta"),
            Some("user-c"),
            Some("model-c"),
            Some("provider-a"),
        ))
        .unwrap();
    store
        .insert(queued_record_with_keys(
            "review-5",
            Some("epsilon"),
            Some("user-d"),
            Some("model-d"),
            Some("provider-d"),
        ))
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
        .unwrap();
    let claimed_ids = claims
        .iter()
        .map(|claim| claim.review_id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(claimed_ids, vec!["review-1", "review-5"]);
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-2").unwrap())
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-3").unwrap())
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
    assert_eq!(
        store
            .get(&ReviewSessionId::new("review-4").unwrap())
            .unwrap()
            .unwrap()
            .status,
        ReviewStatus::Queued
    );
}

#[test]
fn workspace_schedule_review_persists_queued_record_with_options() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");

    let review = workspace
        .schedule_review_with_options(
            ReviewSource::local_with_changed_files(repo.path(), ["README.md"]),
            ReviewOptions {
                user_id: Some("user-a".to_string()),
                dedupe: DedupePolicy::Source,
                ..ReviewOptions::default()
            },
        )
        .unwrap();
    let record = store.get(review.id()).unwrap().unwrap();

    assert_eq!(review.status(), ReviewStatus::Queued);
    assert!(matches!(
        review.wait().unwrap_err(),
        ReviewSessionError::ResultUnavailable { .. }
    ));
    assert_eq!(record.workspace_id.as_deref(), Some("acme"));
    assert_eq!(record.user_id.as_deref(), Some("user-a"));
    assert_eq!(record.options.user_id.as_deref(), Some("user-a"));
    assert_eq!(record.status, ReviewStatus::Queued);
    assert!(record.result.is_none());
    assert_eq!(
        record.events.first().map(|event| event.event_type),
        Some(ReviewEventType::SessionQueued)
    );
}

#[test]
fn review_worker_executes_claimed_local_review_and_persists_result() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let review = workspace
        .schedule_review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
        .unwrap();
    let worker = ReviewWorker::new("worker-a", store.clone(), HostConfiguration::default());

    let run = worker.run_once(1).unwrap();
    let record = store.get(review.id()).unwrap().unwrap();
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

#[test]
fn review_worker_records_final_failure_for_execution_error() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let repo = tempfile::tempdir().expect("temp repo");
    let review = workspace
        .schedule_review(ReviewSource::local(repo.path()))
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

    let run = worker.run_once(1).unwrap();
    let record = store.get(review.id()).unwrap().unwrap();

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

#[test]
fn workspace_profiles_set_get_list_and_version() {
    let workspace = Muzen::new().workspace("acme");

    let first = workspace
        .set_model_profile(
            "default",
            ModelProfileInput {
                provider: ModelProviderKind::OpenaiCompatible,
                model: "gpt-5".to_string(),
                secret_ref: Some("vault://workspaces/acme/models/default".to_string()),
                base_url: Some("https://models.example.test".to_string()),
                routing: BTreeMap::from([("region".to_string(), "us-east".to_string())]),
            },
        )
        .unwrap();
    let second = workspace
        .set_model_profile(
            "default",
            ModelProfileInput {
                provider: ModelProviderKind::OpenaiCompatible,
                model: "gpt-5.1".to_string(),
                secret_ref: Some("vault://workspaces/acme/models/default".to_string()),
                base_url: Some("https://models.example.test".to_string()),
                routing: BTreeMap::new(),
            },
        )
        .unwrap();
    let provider = workspace
        .set_provider_profile(
            "github",
            ProviderProfileInput {
                provider: SourceProviderKind::Github,
                secret_ref: Some("vault://workspaces/acme/providers/github".to_string()),
                base_url: Some("https://api.github.com".to_string()),
                routing: BTreeMap::new(),
            },
        )
        .unwrap();

    assert_eq!(first.version, "1");
    assert_eq!(second.version, "2");
    assert_eq!(provider.version, "1");
    assert_eq!(
        workspace
            .get_model_profile("default")
            .unwrap()
            .unwrap()
            .model,
        "gpt-5.1"
    );
    assert_eq!(workspace.list_model_profiles().unwrap().len(), 1);
    assert_eq!(workspace.list_provider_profiles().unwrap().len(), 1);
}

#[test]
fn workspace_review_captures_model_config_snapshot_without_raw_secret() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let session_store = Arc::new(InMemoryReviewSessionStore::default());
    let profile_store = Arc::new(InMemoryWorkspaceProfileStore::default());
    let muzen = Muzen::with_stores(session_store.clone(), profile_store);
    let workspace = muzen.workspace("acme");
    workspace
        .set_model_profile(
            "default",
            ModelProfileInput {
                provider: ModelProviderKind::OpenaiCompatible,
                model: "gpt-5".to_string(),
                secret_ref: Some("vault://workspaces/acme/models/default".to_string()),
                base_url: Some("https://models.example.test".to_string()),
                routing: BTreeMap::from([("region".to_string(), "us-east".to_string())]),
            },
        )
        .unwrap();

    let review = workspace
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
        .unwrap();
    let record = session_store.get(review.id()).unwrap().unwrap();
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
    assert!(serialized.contains("vault://workspaces/acme/models/default"));
    assert!(!serialized.contains("apiKey"));
    assert!(!serialized.contains("token"));
    assert!(!serialized.contains("sk-live"));
}


#[test]
fn review_session_logs_are_redacted_before_persistence() {
    let raw_secret = "sk-live-raw-secret";
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store.clone()).workspace("acme");
    let review = workspace
        .schedule_review(ReviewSource::local_with_changed_files(".", ["Cargo.toml"]))
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
        .unwrap();
    let logs = store.logs_after(review.id(), None).unwrap();
    let record = store.get(review.id()).unwrap().unwrap();
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

#[test]
fn review_events_response_replays_json_from_store() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store).workspace("acme");
    let review = workspace
        .schedule_review(ReviewSource::local_with_changed_files(".", ["Cargo.toml"]))
        .unwrap();

    let response = workspace.review_events_response(review.id(), None).unwrap();
    let payload: Value = serde_json::from_str(&response.body).unwrap();

    assert_eq!(response.status_code, HTTP_STATUS_OK);
    assert_eq!(response.header("Content-Type"), Some(CONTENT_TYPE_JSON));
    assert_eq!(payload["events"][0]["cursor"], json!("1"));
    assert_eq!(payload["events"][0]["type"], json!("session.queued"));
    assert_eq!(
        payload["events"][0]["reviewId"],
        json!(review.id().as_str())
    );

    let after_response = workspace
        .review_events_response(review.id(), Some("1"))
        .unwrap();
    let after_payload: Value = serde_json::from_str(&after_response.body).unwrap();

    assert_eq!(after_payload["events"].as_array().unwrap().len(), 0);
}

#[test]
fn review_events_sse_response_renders_service_side_event_stream() {
    let store = Arc::new(InMemoryReviewSessionStore::default());
    let workspace = Muzen::with_store(store).workspace("acme");
    let review = workspace
        .schedule_review(ReviewSource::local_with_changed_files(".", ["Cargo.toml"]))
        .unwrap();

    let response = workspace
        .review_events_sse_response(review.id(), None)
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

#[test]
fn review_http_router_schedules_root_review_and_replays_events() {
    let muzen = Muzen::new();
    let router = ReviewHttpRouter::new(muzen.clone());
    let create_request = ReviewHttpRequest::new("POST", "/v1/reviews")
        .json(&json!({
            "source": {
                "type": "local",
                "repo": ".",
                "changedFiles": ["Cargo.toml"]
            },
            "options": {
                "dedupe": "source"
            }
        }))
        .unwrap();

    let create_response = router.handle(create_request);
    let create_body: Value = serde_json::from_str(&create_response.body).unwrap();
    let get_response = router.handle(ReviewHttpRequest::new("GET", "/v1/reviews/review-1"));
    let get_body: Value = serde_json::from_str(&get_response.body).unwrap();
    let events_response = router.handle(ReviewHttpRequest::new(
        "GET",
        "/v1/reviews/review-1/events?after=1",
    ));
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
        .unwrap()
        .unwrap();
    assert_eq!(record.status, ReviewStatus::Queued);
    assert!(record.result.is_none());
}

#[test]
fn review_http_router_serves_results_and_artifacts_from_store() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
    let muzen = Muzen::new();
    let review = muzen
        .review(ReviewSource::local_with_changed_files(
            repo.path(),
            ["README.md"],
        ))
        .unwrap();
    let router = ReviewHttpRouter::new(muzen);

    let result_response = router.handle(ReviewHttpRequest::new(
        "GET",
        format!("/v1/reviews/{}/result", review.id()).as_str(),
    ));
    let export_response = router.handle(ReviewHttpRequest::new(
        "POST",
        format!("/v1/reviews/{}/artifacts/export", review.id()).as_str(),
    ));
    let result_body: Value = serde_json::from_str(&result_response.body).unwrap();
    let export_body: Value = serde_json::from_str(&export_response.body).unwrap();
    let artifact_id = export_body["artifacts"][0]["artifactId"]
        .as_str()
        .unwrap()
        .to_string();
    let artifact_response = router.handle(ReviewHttpRequest::new(
        "GET",
        format!(
            "/v1/reviews/{}/artifacts/{}?view=redacted",
            review.id(),
            artifact_id
        )
        .as_str(),
    ));
    let artifact_body: Value = serde_json::from_str(&artifact_response.body).unwrap();

    assert_eq!(result_response.status_code, HTTP_STATUS_OK);
    assert_eq!(result_body["result"]["status"], json!("completed"));
    assert_eq!(export_response.status_code, HTTP_STATUS_OK);
    assert!(export_body["artifactCount"].as_u64().unwrap() > 0);
    assert_eq!(artifact_response.status_code, HTTP_STATUS_OK);
    assert_eq!(artifact_body["artifact"]["artifactId"], json!(artifact_id));
}

#[test]
fn review_http_router_handles_workspace_profile_routes() {
    let router = ReviewHttpRouter::new(Muzen::new());
    let put_model = ReviewHttpRequest::new("PUT", "/v1/workspaces/acme/models/default")
        .json(&ModelProfileInput {
            provider: ModelProviderKind::OpenaiCompatible,
            model: "gpt-5".to_string(),
            secret_ref: Some("vault://workspaces/acme/models/default".to_string()),
            base_url: Some("https://models.example.test".to_string()),
            routing: BTreeMap::from([("region".to_string(), "us-east".to_string())]),
        })
        .unwrap();
    let put_provider = ReviewHttpRequest::new("PUT", "/v1/workspaces/acme/providers/github")
        .json(&ProviderProfileInput {
            provider: SourceProviderKind::Github,
            secret_ref: Some("vault://workspaces/acme/providers/github".to_string()),
            base_url: Some("https://api.github.com".to_string()),
            routing: BTreeMap::new(),
        })
        .unwrap();

    let model_response = router.handle(put_model);
    let provider_response = router.handle(put_provider);
    let models_response =
        router.handle(ReviewHttpRequest::new("GET", "/v1/workspaces/acme/models"));
    let provider_body: Value = serde_json::from_str(&provider_response.body).unwrap();
    let models_body: Value = serde_json::from_str(&models_response.body).unwrap();
    let missing_response = router.handle(ReviewHttpRequest::new(
        "GET",
        "/v1/workspaces/acme/models/missing",
    ));

    assert_eq!(model_response.status_code, HTTP_STATUS_OK);
    assert_eq!(provider_response.status_code, HTTP_STATUS_OK);
    assert_eq!(
        provider_body["profile"]["secretRef"],
        json!("vault://workspaces/acme/providers/github")
    );
    assert_eq!(models_response.status_code, HTTP_STATUS_OK);
    assert_eq!(models_body["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(missing_response.status_code, HTTP_STATUS_NO_CONTENT);
    assert!(missing_response.body.is_empty());
}

#[test]
fn review_http_router_verifies_and_schedules_workspace_github_webhook() {
    let muzen = Muzen::new();
    let router = ReviewHttpRouter::with_options(
        muzen.clone(),
        ReviewHttpRouterOptions {
            github_webhook_secret: Some("secret".to_string()),
            gitlab_webhook_secret: None,
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

    let response = router.handle(request);
    let response_body: Value = serde_json::from_str(&response.body).unwrap();
    let record = muzen
        .store
        .get(&ReviewSessionId::new("review-1").unwrap())
        .unwrap()
        .unwrap();

    assert_eq!(response.status_code, HTTP_STATUS_ACCEPTED);
    assert_eq!(response_body["type"], json!("review_created"));
    assert_eq!(response_body["deliveryId"], json!("delivery-1"));
    assert_eq!(record.workspace_id.as_deref(), Some("acme"));
    assert_eq!(record.status, ReviewStatus::Queued);
    assert_eq!(record.options.metadata["webhook.provider"], json!("github"));
}

#[test]
fn github_webhook_verifies_maps_schedules_and_dedupes_pull_request() {
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
        .unwrap();
    let second = workspace
        .handle_github_webhook(&headers, body.as_bytes(), Some("secret"), options)
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
    let record = store.get(review.id()).unwrap().unwrap();
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

#[test]
fn github_webhook_source_head_dedupe_includes_head_sha() {
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
        .unwrap();
    let duplicate = workspace
        .handle_github_webhook(
            &headers,
            body_for("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").as_bytes(),
            None,
            options.clone(),
        )
        .unwrap();
    let changed_head = workspace
        .handle_github_webhook(
            &headers,
            body_for("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").as_bytes(),
            None,
            options,
        )
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
        .unwrap()
        .is_some());
    assert!(store
        .get_by_dedupe_key(
            "source-head:github:maskdotdev/heimdaal#123@bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )
        .unwrap()
        .is_some());
}

#[test]
fn github_webhook_ignores_unsupported_pull_request_action() {
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

#[test]
fn github_webhook_rejects_invalid_signature() {
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
        .unwrap_err();

    assert!(error.to_string().contains("signature verification failed"));
}

#[test]
fn gitlab_webhook_verifies_token_and_maps_merge_request() {
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
    let record = store.get(review.id()).unwrap().unwrap();

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

#[test]
fn gitlab_webhook_rejects_invalid_token() {
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
        .unwrap_err();

    assert!(error.to_string().contains("token verification failed"));
}

#[test]
fn workspace_effective_snapshot_includes_source_provider_profile() {
    let workspace = Muzen::new().workspace("acme");
    workspace
        .set_provider_profile(
            "github",
            ProviderProfileInput {
                provider: SourceProviderKind::Github,
                secret_ref: Some("vault://workspaces/acme/providers/github".to_string()),
                base_url: Some("https://api.github.com".to_string()),
                routing: BTreeMap::from([("installation".to_string(), "123".to_string())]),
            },
        )
        .unwrap();
    let source = ReviewSource::github_pull_request("maskdotdev", "heimdaal", 123).unwrap();

    let snapshot = workspace.effective_config_snapshot(&source, None).unwrap();

    assert_eq!(
        snapshot
            .provider_profile
            .as_ref()
            .map(|profile| profile.id.as_str()),
        Some("workspace:acme/providers/github")
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
        Some("vault://workspaces/acme/providers/github")
    );
}

fn queued_record(
    id: &str,
    workspace_id: Option<&str>,
    run_after_unix_seconds: u64,
) -> ReviewSessionRecord {
    let review_id = ReviewSessionId::new(id).unwrap();
    ReviewSessionRecord {
        id: review_id,
        workspace_id: workspace_id.map(str::to_string),
        user_id: None,
        status: ReviewStatus::Queued,
        source: ReviewSource::local("."),
        options: ReviewOptions::default(),
        result: None,
        events: Vec::new(),
        logs: Vec::new(),
        redacted_artifacts: Vec::new(),
        raw_artifacts: Vec::new(),
        config_snapshot: None,
        attempt: 0,
        run_after_unix_seconds,
        lease: None,
        cancellation: None,
        last_error: None,
        dedupe_key: None,
        created_at_utc: timestamp_utc(),
        updated_at_utc: timestamp_utc(),
    }
}

fn queued_record_with_keys(
    id: &str,
    workspace_id: Option<&str>,
    user_id: Option<&str>,
    model_profile_id: Option<&str>,
    provider_profile_id: Option<&str>,
) -> ReviewSessionRecord {
    let mut record = queued_record(id, workspace_id, 0);
    record.user_id = user_id.map(str::to_string);
    record.config_snapshot = Some(EffectiveConfigSnapshot {
        model_profile: model_profile_id.map(profile_ref),
        provider_profile: provider_profile_id.map(profile_ref),
        routing: BTreeMap::new(),
    });
    record
}

fn profile_ref(id: &str) -> ProfileVersionRef {
    ProfileVersionRef {
        id: id.to_string(),
        version: "1".to_string(),
        secret_ref: None,
    }
}
