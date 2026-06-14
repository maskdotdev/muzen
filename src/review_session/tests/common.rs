use super::super::*;
use crate::util::timestamp_utc;
use std::collections::BTreeMap;

pub(super) fn completed_result(review_id: &ReviewSessionId, summary: &str) -> ReviewResult {
    ReviewResult {
        review_id: review_id.clone(),
        session_id: review_id.clone(),
        status: ReviewStatus::Completed,
        conclusion: ReviewConclusion::Approved,
        summary: summary.to_string(),
        findings: Vec::new(),
        coverage: ReviewCoverage {
            files_considered: 0,
            files_reviewed: 0,
            files_skipped: 0,
        },
        metadata: BTreeMap::new(),
    }
}

pub(super) fn model_profile_input(model: &str) -> ModelProfileInput {
    ModelProfileInput {
        provider: ModelProviderKind::Openai,
        model: model.to_string(),
        secret_ref: Some("vault://models/default".to_string()),
        base_url: None,
        routing: BTreeMap::new(),
    }
}

pub(super) fn provider_profile_input(installation: &str) -> ProviderProfileInput {
    ProviderProfileInput {
        provider: SourceProviderKind::Github,
        secret_ref: Some("vault://providers/github".to_string()),
        base_url: Some("https://api.github.com".to_string()),
        routing: BTreeMap::from([("installation".to_string(), installation.to_string())]),
    }
}

pub(super) fn queued_record(
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

pub(super) fn queued_record_with_keys(
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

pub(super) fn profile_ref(id: &str) -> ProfileVersionRef {
    ProfileVersionRef {
        id: id.to_string(),
        version: "1".to_string(),
        secret_ref: None,
    }
}
