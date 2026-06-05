use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::runner::{
    execute_run_start, RunStartParams, RunnerArtifactView, RUNNER_PROTOCOL_VERSION,
};
use crate::util::timestamp_utc;

mod config;
mod http;
mod options;
mod outcome;
mod profiles;
mod router;
mod source;
mod store;
mod webhooks;
mod worker;

pub use config::{HostConfiguration, HostSchedulingConfiguration, SchedulingFairnessStrategy};
pub use http::{
    ReviewHttpResponse, ReviewSseFrame, ReviewSseStream, CONTENT_TYPE_EVENT_STREAM,
    CONTENT_TYPE_JSON, CONTENT_TYPE_TEXT, HTTP_STATUS_ACCEPTED, HTTP_STATUS_BAD_REQUEST,
    HTTP_STATUS_METHOD_NOT_ALLOWED, HTTP_STATUS_NOT_FOUND, HTTP_STATUS_NO_CONTENT, HTTP_STATUS_OK,
};
pub use options::{
    DedupePolicy, EffectiveConfigSnapshot, ProfileVersionRef, ReviewAgentSession, ReviewLimits,
    ReviewOptions, ReviewScope,
};
pub use outcome::{
    ReviewArtifact, ReviewArtifactExport, ReviewArtifactExportOptions, ReviewArtifactReadOptions,
    ReviewArtifactView, ReviewCancelOptions, ReviewConclusion, ReviewCoverage, ReviewEvent,
    ReviewEventType, ReviewFinding, ReviewFindingCategory, ReviewFindingLocation,
    ReviewFindingSeverity, ReviewResult, ReviewSessionId, ReviewSessionSnapshot, ReviewStatus,
    ReviewSuggestedFix,
};
pub use profiles::{
    InMemoryWorkspaceProfileStore, ModelProfile, ModelProfileInput, ModelProviderKind,
    PostgresWorkspaceProfileStore, ProviderProfile, ProviderProfileInput, SourceProviderKind,
    WorkspaceProfileStore,
};
pub use router::{
    ReviewHttpRequest, ReviewHttpRouteError, ReviewHttpRouter, ReviewHttpRouterOptions,
};
pub use source::{ReviewSource, ReviewSourceLike};
pub use store::{
    InMemoryReviewSessionStore, PostgresReviewSessionStore, ReviewAttemptFailure,
    ReviewCancellationRecord, ReviewLeaseExtension, ReviewLogEntry, ReviewLogRedactionPolicy,
    ReviewLogStream, ReviewRetryPolicy, ReviewSessionRecord, ReviewSessionStore, ReviewWorkerClaim,
    ReviewWorkerClaimOptions, ReviewWorkerConcurrencyLimits, ReviewWorkerLease,
};
pub use webhooks::{
    github_webhook_signature, map_github_webhook_source, map_gitlab_webhook_source,
    verify_github_webhook_signature, verify_gitlab_webhook_token, WebhookHeaders,
    WebhookMappedSource, WebhookReviewDelivery, WebhookReviewOptions,
};
pub use worker::{ReviewWorker, ReviewWorkerRun};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReviewSessionError {
    #[error("invalid review source `{input}`: {reason}")]
    InvalidSource { input: String, reason: String },
    #[error("review session id cannot be empty")]
    EmptyReviewSessionId,
    #[error("runner failed to execute review session: {0}")]
    Runner(String),
    #[error("review session `{review_id}` has no final result yet")]
    ResultUnavailable { review_id: ReviewSessionId },
    #[error("unknown artifact id `{artifact_id}` for review session `{review_id}`")]
    UnknownArtifactId {
        review_id: ReviewSessionId,
        artifact_id: String,
    },
    #[error("artifact export limit exceeded: {kind}")]
    ArtifactLimitExceeded { kind: String },
    #[error("review session store error: {0}")]
    Store(String),
    #[error("workspace profile error: {0}")]
    Profile(String),
    #[error("webhook error: {0}")]
    Webhook(String),
    #[error("review HTTP response error: {0}")]
    Http(String),
}

#[derive(Clone)]
pub struct Muzen {
    next_review_id: Arc<AtomicU64>,
    store: Arc<dyn ReviewSessionStore>,
    profile_store: Arc<dyn WorkspaceProfileStore>,
}

impl std::fmt::Debug for Muzen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Muzen")
            .field(
                "next_review_id",
                &self.next_review_id.load(Ordering::SeqCst),
            )
            .finish_non_exhaustive()
    }
}

impl Muzen {
    pub fn new() -> Self {
        Self::with_stores(
            Arc::new(InMemoryReviewSessionStore::default()),
            Arc::new(InMemoryWorkspaceProfileStore::default()),
        )
    }

    pub fn with_store(store: Arc<dyn ReviewSessionStore>) -> Self {
        Self::with_stores(store, Arc::new(InMemoryWorkspaceProfileStore::default()))
    }

    pub fn with_stores(
        store: Arc<dyn ReviewSessionStore>,
        profile_store: Arc<dyn WorkspaceProfileStore>,
    ) -> Self {
        Self {
            next_review_id: Arc::new(AtomicU64::new(1)),
            store,
            profile_store,
        }
    }

    pub fn review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.create_review_session(CreateReviewSessionInput::new(source)?)
    }

    pub fn review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.create_review_session(CreateReviewSessionInput::with_options(source, options)?)
    }

    pub fn schedule_review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.schedule_review_with_options(source, ReviewOptions::default())
    }

    pub fn schedule_review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let source = source.into().resolve()?;
        let input = CreateReviewSessionInput { source, options };
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key)? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::queued(self.next_review_id(), input);
        self.store.insert(review.to_record(dedupe_key, None))?;
        Ok(review)
    }

    pub fn worker(
        &self,
        worker_id: impl Into<String>,
        host_config: HostConfiguration,
    ) -> ReviewWorker {
        ReviewWorker::new(worker_id, self.store.clone(), host_config)
    }

    pub fn create_review_session(
        &self,
        input: CreateReviewSessionInput,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key)? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::execute_local(self.next_review_id(), input)?;
        self.store.insert(review.to_record(dedupe_key, None))?;
        Ok(review)
    }

    pub fn workspace(&self, id: impl Into<String>) -> MuzenWorkspace {
        MuzenWorkspace {
            id: id.into(),
            next_review_id: self.next_review_id.clone(),
            store: self.store.clone(),
            profile_store: self.profile_store.clone(),
        }
    }

    fn next_review_id(&self) -> ReviewSessionId {
        let id = self.next_review_id.fetch_add(1, Ordering::SeqCst);
        ReviewSessionId(format!("review-{id}"))
    }
}

impl Default for Muzen {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct MuzenWorkspace {
    id: String,
    next_review_id: Arc<AtomicU64>,
    store: Arc<dyn ReviewSessionStore>,
    profile_store: Arc<dyn WorkspaceProfileStore>,
}

impl std::fmt::Debug for MuzenWorkspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MuzenWorkspace")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl MuzenWorkspace {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.review_with_options(source, ReviewOptions::default())
    }

    pub fn review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        mut options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let source = source.into().resolve()?;
        options.config_snapshot =
            Some(self.effective_config_snapshot(&source, options.model.as_deref())?);
        let input = CreateReviewSessionInput { source, options };
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key)? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::execute_local(self.next_review_id(), input)?;
        self.store
            .insert(review.to_record(dedupe_key, Some(self.id.clone())))?;
        Ok(review)
    }

    pub fn schedule_review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.schedule_review_with_options(source, ReviewOptions::default())
    }

    pub fn schedule_review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        mut options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let source = source.into().resolve()?;
        options.config_snapshot =
            Some(self.effective_config_snapshot(&source, options.model.as_deref())?);
        let input = CreateReviewSessionInput { source, options };
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key)? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::queued(self.next_review_id(), input);
        self.store
            .insert(review.to_record(dedupe_key, Some(self.id.clone())))?;
        Ok(review)
    }

    pub fn effective_config_snapshot(
        &self,
        source: &ReviewSource,
        model: Option<&str>,
    ) -> Result<EffectiveConfigSnapshot, ReviewSessionError> {
        let model_profile = self
            .selected_model_profile(model)?
            .map(|profile| profile.version_ref());
        let provider_profile = self
            .source_provider_profile(source)?
            .map(|profile| profile.version_ref());
        let mut routing = BTreeMap::new();
        if let Some(profile) = self.selected_model_profile(model)? {
            routing.insert(
                "model.provider".to_string(),
                profile.provider.as_str().to_string(),
            );
            routing.insert("model.name".to_string(), profile.model);
            if let Some(base_url) = profile.base_url {
                routing.insert("model.baseUrl".to_string(), base_url);
            }
            for (key, value) in profile.routing {
                routing.insert(format!("model.routing.{key}"), value);
            }
        }
        if let Some(profile) = self.source_provider_profile(source)? {
            routing.insert(
                "provider.kind".to_string(),
                profile.provider.as_str().to_string(),
            );
            if let Some(base_url) = profile.base_url {
                routing.insert("provider.baseUrl".to_string(), base_url);
            }
            for (key, value) in profile.routing {
                routing.insert(format!("provider.routing.{key}"), value);
            }
        }
        Ok(EffectiveConfigSnapshot {
            model_profile,
            provider_profile,
            routing,
        })
    }

    pub fn set_model_profile(
        &self,
        name: impl Into<String>,
        input: ModelProfileInput,
    ) -> Result<ModelProfile, ReviewSessionError> {
        self.profile_store
            .set_model_profile(&self.id, name.into(), input)
    }

    pub fn get_model_profile(
        &self,
        name: &str,
    ) -> Result<Option<ModelProfile>, ReviewSessionError> {
        self.profile_store.get_model_profile(&self.id, name)
    }

    pub fn list_model_profiles(&self) -> Result<Vec<ModelProfile>, ReviewSessionError> {
        self.profile_store.list_model_profiles(&self.id)
    }

    pub fn set_provider_profile(
        &self,
        name: impl Into<String>,
        input: ProviderProfileInput,
    ) -> Result<ProviderProfile, ReviewSessionError> {
        self.profile_store
            .set_provider_profile(&self.id, name.into(), input)
    }

    pub fn get_provider_profile(
        &self,
        name: &str,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError> {
        self.profile_store.get_provider_profile(&self.id, name)
    }

    pub fn list_provider_profiles(&self) -> Result<Vec<ProviderProfile>, ReviewSessionError> {
        self.profile_store.list_provider_profiles(&self.id)
    }

    fn next_review_id(&self) -> ReviewSessionId {
        let id = self.next_review_id.fetch_add(1, Ordering::SeqCst);
        ReviewSessionId(format!("review-{id}"))
    }

    fn selected_model_profile(
        &self,
        model: Option<&str>,
    ) -> Result<Option<ModelProfile>, ReviewSessionError> {
        let name = model.unwrap_or("default");
        self.profile_store.get_model_profile(&self.id, name)
    }

    fn source_provider_profile(
        &self,
        source: &ReviewSource,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError> {
        let name = match source {
            ReviewSource::GithubPullRequest { .. } => "github",
            ReviewSource::GitlabMergeRequest { .. } => "gitlab",
            ReviewSource::Local { .. } => return Ok(None),
        };
        self.profile_store.get_provider_profile(&self.id, name)
    }
}

#[derive(Debug, Clone)]
pub struct ReviewSession {
    id: ReviewSessionId,
    status: ReviewStatus,
    source: ReviewSource,
    options: ReviewOptions,
    user_id: Option<String>,
    events: Vec<ReviewEvent>,
    result: Option<ReviewResult>,
    redacted_artifacts: Vec<ReviewArtifact>,
    raw_artifacts: Vec<ReviewArtifact>,
    config_snapshot: Option<EffectiveConfigSnapshot>,
}

impl ReviewSession {
    fn execute_local(
        id: ReviewSessionId,
        input: CreateReviewSessionInput,
    ) -> Result<Self, ReviewSessionError> {
        let source = input.source.clone();
        let options = input.options.clone();
        let config_snapshot = input.options.config_snapshot.clone();
        let user_id = input.options.user_id.clone();
        let start = input.into_runner_start(&id)?;
        let executed = execute_run_start(start, None)
            .map_err(|error| ReviewSessionError::Runner(error.to_string()))?;
        let result = ReviewResult::from_runner_result(id.clone(), &source, executed.result);
        let status = result.status;
        let events = executed
            .events
            .into_iter()
            .map(ReviewEvent::from_internal_record)
            .collect();
        let redacted_artifacts = executed
            .stored
            .artifacts(RunnerArtifactView::Redacted)
            .iter()
            .map(ReviewArtifact::from_runner_artifact)
            .collect();
        let raw_artifacts = executed
            .stored
            .artifacts(RunnerArtifactView::Raw)
            .iter()
            .map(ReviewArtifact::from_runner_artifact)
            .collect();
        Ok(Self {
            id,
            status,
            source,
            options,
            user_id,
            events,
            result: Some(result),
            redacted_artifacts,
            raw_artifacts,
            config_snapshot,
        })
    }

    fn queued(id: ReviewSessionId, input: CreateReviewSessionInput) -> Self {
        let source = input.source;
        let options = input.options;
        let user_id = options.user_id.clone();
        let config_snapshot = options.config_snapshot.clone();
        let now = timestamp_utc();
        Self {
            id: id.clone(),
            status: ReviewStatus::Queued,
            source,
            options,
            user_id,
            events: vec![ReviewEvent {
                cursor: "1".to_string(),
                event_type: ReviewEventType::SessionQueued,
                review_id: id,
                timestamp_utc: now,
                payload: json!({}),
            }],
            result: None,
            redacted_artifacts: Vec::new(),
            raw_artifacts: Vec::new(),
            config_snapshot,
        }
    }

    fn from_record(record: ReviewSessionRecord) -> Self {
        Self {
            id: record.id,
            status: record.status,
            source: record.source,
            options: record.options,
            user_id: record.user_id,
            events: record.events,
            result: record.result,
            redacted_artifacts: record.redacted_artifacts,
            raw_artifacts: record.raw_artifacts,
            config_snapshot: record.config_snapshot,
        }
    }

    fn to_record(
        &self,
        dedupe_key: Option<String>,
        workspace_id: Option<String>,
    ) -> ReviewSessionRecord {
        ReviewSessionRecord {
            id: self.id.clone(),
            workspace_id,
            user_id: self.user_id.clone(),
            status: self.status,
            source: self.source.clone(),
            options: self.options.clone(),
            result: self.result.clone(),
            events: self.events.clone(),
            logs: Vec::new(),
            redacted_artifacts: self.redacted_artifacts.clone(),
            raw_artifacts: self.raw_artifacts.clone(),
            config_snapshot: self.config_snapshot.clone(),
            attempt: 0,
            run_after_unix_seconds: 0,
            lease: None,
            cancellation: None,
            last_error: None,
            dedupe_key,
            created_at_utc: timestamp_utc(),
            updated_at_utc: timestamp_utc(),
        }
    }

    pub fn id(&self) -> &ReviewSessionId {
        &self.id
    }

    pub fn status(&self) -> ReviewStatus {
        self.status
    }

    pub fn source(&self) -> &ReviewSource {
        &self.source
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn config_snapshot(&self) -> Option<&EffectiveConfigSnapshot> {
        self.config_snapshot.as_ref()
    }

    pub fn subscribe(&self, mut listener: impl FnMut(&ReviewEvent)) {
        for event in &self.events {
            listener(event);
        }
    }

    pub fn events(&self) -> impl Iterator<Item = &ReviewEvent> {
        self.events.iter()
    }

    pub fn event_records(&self) -> &[ReviewEvent] {
        &self.events
    }

    pub fn wait(&self) -> Result<ReviewResult, ReviewSessionError> {
        self.result
            .clone()
            .ok_or_else(|| ReviewSessionError::ResultUnavailable {
                review_id: self.id.clone(),
            })
    }

    pub fn result(&self) -> Option<&ReviewResult> {
        self.result.as_ref()
    }

    pub fn read_artifact(
        &self,
        artifact_id: &str,
        options: ReviewArtifactReadOptions,
    ) -> Result<ReviewArtifact, ReviewSessionError> {
        self.artifacts_for_view(options.view)
            .iter()
            .find(|artifact| artifact.artifact_id == artifact_id)
            .cloned()
            .ok_or_else(|| ReviewSessionError::UnknownArtifactId {
                review_id: self.id.clone(),
                artifact_id: artifact_id.to_string(),
            })
    }

    pub fn export_artifacts(
        &self,
        options: ReviewArtifactExportOptions,
    ) -> Result<ReviewArtifactExport, ReviewSessionError> {
        let mut artifacts = self.artifacts_for_view(options.view).to_vec();
        if !options.artifact_ids.is_empty() {
            artifacts.retain(|artifact| {
                options
                    .artifact_ids
                    .iter()
                    .any(|artifact_id| artifact_id == &artifact.artifact_id)
            });
        }
        let total_bytes = artifacts
            .iter()
            .map(|artifact| artifact.bytes)
            .sum::<usize>();
        if options
            .max_artifacts
            .is_some_and(|max_artifacts| artifacts.len() > max_artifacts)
        {
            return Err(ReviewSessionError::ArtifactLimitExceeded {
                kind: "artifact_count".to_string(),
            });
        }
        if options
            .max_bytes
            .is_some_and(|max_bytes| total_bytes > max_bytes)
        {
            return Err(ReviewSessionError::ArtifactLimitExceeded {
                kind: "artifact_bytes".to_string(),
            });
        }
        Ok(ReviewArtifactExport {
            view: options.view,
            artifact_count: artifacts.len(),
            total_bytes,
            artifacts,
        })
    }

    pub fn cancel(
        &mut self,
        options: impl Into<ReviewCancelOptions>,
    ) -> Result<(), ReviewSessionError> {
        if self.status.is_terminal() {
            return Ok(());
        }
        let options = options.into();
        self.status = ReviewStatus::Cancelled;
        self.events.push(ReviewEvent {
            cursor: (self.events.len() + 1).to_string(),
            event_type: ReviewEventType::SessionCancelled,
            review_id: self.id.clone(),
            timestamp_utc: timestamp_utc(),
            payload: json!({ "reason": options.reason }),
        });
        Ok(())
    }

    pub fn refresh(&self) -> ReviewSessionSnapshot {
        ReviewSessionSnapshot {
            id: self.id.clone(),
            status: self.status,
            source: self.source.clone(),
            result: self.result.clone(),
        }
    }

    fn artifacts_for_view(&self, view: ReviewArtifactView) -> &[ReviewArtifact] {
        match view {
            ReviewArtifactView::Redacted => &self.redacted_artifacts,
            ReviewArtifactView::Raw => &self.raw_artifacts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReviewSessionInput {
    pub source: ReviewSource,
    #[serde(default)]
    pub options: ReviewOptions,
}

impl CreateReviewSessionInput {
    pub fn new(source: impl Into<ReviewSourceLike>) -> Result<Self, ReviewSessionError> {
        Ok(Self {
            source: source.into().resolve()?,
            options: ReviewOptions::default(),
        })
    }

    pub fn with_options(
        source: impl Into<ReviewSourceLike>,
        options: ReviewOptions,
    ) -> Result<Self, ReviewSessionError> {
        Ok(Self {
            source: source.into().resolve()?,
            options,
        })
    }

    pub fn into_runner_start(
        self,
        review_id: &ReviewSessionId,
    ) -> Result<RunStartParams, ReviewSessionError> {
        let changed_files = self.source.runner_changed_files(&self.options.scope);
        let repo = self.source.local_repo().map(Path::to_path_buf);
        let source_provider = self.options.runner_source_provider();
        Ok(RunStartParams {
            protocol_version: Some(RUNNER_PROTOCOL_VERSION.to_string()),
            run_id: Some(review_id.as_str().to_string()),
            repo,
            source: Some(self.source),
            source_provider,
            changed_files,
            sessions: self.options.runner_sessions(),
            limits: self.options.limits.map(ReviewLimits::into_runner_limits),
            model: None,
            tools: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::Role;
    use crate::runner::{RunnerFinding, RunnerRunResult, RunnerRunSummary, RunnerSnapshotSummary};
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
    fn rejects_invalid_source_shorthand() {
        let error = ReviewSource::from_str("github:maskdotdev/heimdaal").unwrap_err();

        assert!(error
            .to_string()
            .contains("missing `#` review number delimiter"));
    }

    #[test]
    fn maps_local_review_input_to_runner_start_params() {
        let input = CreateReviewSessionInput::with_options(
            ReviewSource::local_with_changed_files(".", ["Cargo.toml"]),
            ReviewOptions {
                model: Some("default".to_string()),
                sessions: vec![ReviewAgentSession::new(
                    "security",
                    Role::Security,
                    "Find security regressions",
                )],
                limits: Some(ReviewLimits {
                    max_active_sessions: Some(1),
                    max_file_bytes: Some(4096),
                    max_search_matches: Some(12),
                }),
                ..ReviewOptions::default()
            },
        )
        .unwrap();
        let review_id = ReviewSessionId::new("review-1").unwrap();

        let start = input.into_runner_start(&review_id).unwrap();

        assert_eq!(
            start.protocol_version.as_deref(),
            Some(RUNNER_PROTOCOL_VERSION)
        );
        assert_eq!(start.run_id.as_deref(), Some("review-1"));
        assert_eq!(start.repo.as_deref(), Some(Path::new(".")));
        assert_eq!(
            start.source,
            Some(ReviewSource::local_with_changed_files(".", ["Cargo.toml"]))
        );
        assert_eq!(start.changed_files, vec!["Cargo.toml"]);
        assert_eq!(start.sessions.len(), 1);
        assert_eq!(start.sessions[0].id, "security");
        assert_eq!(
            start.sessions[0].model_profile_id.as_deref(),
            Some("default")
        );
        assert_eq!(
            start
                .limits
                .as_ref()
                .and_then(|limits| limits.max_file_bytes),
            Some(4096)
        );
    }

    #[test]
    fn maps_provider_source_to_runner_materialization_params() {
        let input = CreateReviewSessionInput::new("github:maskdotdev/heimdaal#123").unwrap();
        let review_id = ReviewSessionId::new("review-1").unwrap();

        let start = input.into_runner_start(&review_id).unwrap();

        assert_eq!(start.repo, None);
        assert_eq!(
            start.source,
            Some(ReviewSource::github_pull_request("maskdotdev", "heimdaal", 123).unwrap())
        );
        assert!(start.changed_files.is_empty());
    }

    #[test]
    fn maps_runner_result_to_review_result() {
        let review_id = ReviewSessionId::new("review-1").unwrap();
        let source = ReviewSource::local(".");
        let result = RunnerRunResult {
            protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
            run_id: "review-1".to_string(),
            status: "completed".to_string(),
            summary: RunnerRunSummary {
                sessions: 2,
                completed_sessions: 2,
                model_calls: 3,
                tool_calls: 9,
                findings: 1,
                publishable_findings: 1,
                elapsed_ms: 120,
                input_tokens: 100,
                output_tokens: 20,
                total_tokens: 120,
                artifacts: 2,
                artifact_bytes: 42,
                snapshot_count: 1,
            },
            findings: vec![RunnerFinding {
                id: "finding-1".to_string(),
                title: "Unsafe unwrap".to_string(),
                claim: "The code can panic.".to_string(),
                evidence_count: 1,
                publishable: true,
            }],
            snapshots: vec![RunnerSnapshotSummary {
                snapshot_id: "snapshot-1".to_string(),
                files: 10,
                changed_files: 2,
                captured_files: 8,
                captured_bytes: 1000,
            }],
        };

        let review = ReviewResult::from_runner_result(review_id, &source, result);

        assert_eq!(review.status, ReviewStatus::Completed);
        assert_eq!(review.conclusion, ReviewConclusion::ChangesRequested);
        assert_eq!(review.findings[0].severity, ReviewFindingSeverity::Error);
        assert_eq!(review.coverage.files_considered, 10);
        assert_eq!(review.coverage.files_reviewed, 8);
        assert_eq!(review.coverage.files_skipped, 2);
        assert_eq!(review.metadata["runnerRunId"], json!("review-1"));
    }

    #[test]
    fn config_snapshot_serializes_secret_refs_not_secret_values() {
        let snapshot = EffectiveConfigSnapshot {
            model_profile: Some(ProfileVersionRef {
                id: "default".to_string(),
                version: "7".to_string(),
                secret_ref: Some("vault://models/default".to_string()),
            }),
            provider_profile: Some(ProfileVersionRef {
                id: "github".to_string(),
                version: "3".to_string(),
                secret_ref: Some("vault://providers/github".to_string()),
            }),
            routing: BTreeMap::from([(
                "baseUrl".to_string(),
                "https://api.github.com".to_string(),
            )]),
        };

        let serialized = serde_json::to_string(&snapshot).unwrap();

        assert!(serialized.contains("vault://models/default"));
        assert!(!serialized.contains("apiKey"));
        assert!(!serialized.contains("token"));
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
    fn host_scheduling_configuration_builds_worker_claim_options() {
        let config = HostSchedulingConfiguration {
            lease_seconds: 120,
            default_retry_policy: ReviewRetryPolicy {
                max_attempts: 5,
                initial_backoff_seconds: 15,
                max_backoff_seconds: 600,
            },
            concurrency: ReviewWorkerConcurrencyLimits {
                max_running_global: Some(10),
                max_running_per_workspace: Some(3),
                max_running_per_user: Some(2),
                max_running_per_model_profile: Some(4),
                max_running_per_provider_profile: Some(5),
            },
            fairness: SchedulingFairnessStrategy::RoundRobinByWorkspace,
        };

        let options = config.claim_options("worker-a", 7);

        assert_eq!(options.worker_id, "worker-a");
        assert_eq!(options.max_sessions, 7);
        assert_eq!(options.lease_seconds, 120);
        assert_eq!(options.concurrency.max_running_global, Some(10));
        assert_eq!(config.default_retry_policy.initial_backoff_seconds, 15);
        assert_eq!(
            config.fairness,
            SchedulingFairnessStrategy::RoundRobinByWorkspace
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
        assert!(record
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("repo has no obvious text file to review")));
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
    fn durable_record_events_and_result_serialize_without_raw_profile_secret() {
        let raw_secret = "sk-live-raw-secret";
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("README.md"), "fixture repo").unwrap();
        let store = Arc::new(InMemoryReviewSessionStore::default());
        let muzen = Muzen::with_store(store.clone());
        let workspace = muzen.workspace("acme");
        workspace
            .set_model_profile(
                "default",
                ModelProfileInput {
                    provider: ModelProviderKind::OpenaiCompatible,
                    model: "gpt-5".to_string(),
                    secret_ref: Some("vault://workspaces/acme/models/default".to_string()),
                    base_url: Some("https://models.example.test".to_string()),
                    routing: BTreeMap::new(),
                },
            )
            .unwrap();
        let review = workspace
            .schedule_review(ReviewSource::local_with_changed_files(
                repo.path(),
                ["README.md"],
            ))
            .unwrap();
        ReviewWorker::new("worker-a", store.clone(), HostConfiguration::default())
            .run_once(1)
            .unwrap();
        let record = store.get(review.id()).unwrap().unwrap();
        let record_json = serde_json::to_string(&record).unwrap();
        let events_json = serde_json::to_string(&record.events).unwrap();
        let result_json = serde_json::to_string(&record.result).unwrap();

        assert!(record_json.contains("vault://workspaces/acme/models/default"));
        for serialized in [record_json, events_json, result_json] {
            assert!(!serialized.contains(raw_secret));
            assert!(!serialized.contains("apiKey"));
        }
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
        assert!(response.body.contains(
            "data: {\"cursor\":\"1\",\"type\":\"session.queued\",\"reviewId\":\"review-1\""
        ));
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
}
