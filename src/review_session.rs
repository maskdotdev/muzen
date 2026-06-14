use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use thiserror::Error;

mod config;
mod http;
mod options;
mod outcome;
mod profiles;
mod router;
mod session;
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
    DedupePolicy, EffectiveConfigSnapshot, ProfileVersionRef, ReviewAgentSession, ReviewChangeSpec,
    ReviewChangedFile, ReviewInstruction, ReviewLimits, ReviewOptions, ReviewScope,
    ReviewToolOption,
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
    ProviderProfile, ProviderProfileInput, SourceProviderKind, WorkspaceProfileStore,
};
pub use router::{
    ReviewHttpRequest, ReviewHttpRouteError, ReviewHttpRouter, ReviewHttpRouterOptions,
};
pub use session::{CreateReviewSessionInput, ReviewSession};
pub use source::{ReviewSource, ReviewSourceLike};
pub use store::{
    stores_from_url, InMemoryReviewSessionStore, LibsqlReviewSessionStore,
    LibsqlWorkspaceProfileStore, MuzenStoreBundle, ReviewAttemptFailure, ReviewCancellationRecord,
    ReviewLeaseExtension, ReviewLogEntry, ReviewLogRedactionPolicy, ReviewLogStream,
    ReviewRetryPolicy, ReviewSessionRecord, ReviewSessionStore, ReviewWorkerClaim,
    ReviewWorkerClaimOptions, ReviewWorkerConcurrencyLimits, ReviewWorkerLease,
    DEFAULT_MUZEN_STORE_URL, MUZEN_STORE_URL_ENV,
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

    pub async fn review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.create_review_session(CreateReviewSessionInput::new(source)?)
            .await
    }

    pub async fn review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.create_review_session(CreateReviewSessionInput::with_options(source, options)?)
            .await
    }

    pub async fn schedule_review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.schedule_review_with_options(source, ReviewOptions::default())
            .await
    }

    pub async fn schedule_review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let source = source.into().resolve()?;
        let input = CreateReviewSessionInput { source, options };
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key).await? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::queued(self.next_review_id(), input);
        self.store
            .insert(review.to_record(dedupe_key, None))
            .await?;
        Ok(review)
    }

    pub fn worker(
        &self,
        worker_id: impl Into<String>,
        host_config: HostConfiguration,
    ) -> ReviewWorker {
        ReviewWorker::new(worker_id, self.store.clone(), host_config)
    }

    pub async fn create_review_session(
        &self,
        input: CreateReviewSessionInput,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key).await? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::execute_local_async(self.next_review_id(), input).await?;
        self.store
            .insert(review.to_record(dedupe_key, None))
            .await?;
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

    pub async fn review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.review_with_options(source, ReviewOptions::default())
            .await
    }

    pub async fn review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        mut options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let source = source.into().resolve()?;
        options.config_snapshot = Some(
            self.effective_config_snapshot(&source, options.model.as_deref())
                .await?,
        );
        let input = CreateReviewSessionInput { source, options };
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key).await? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::execute_local_async(self.next_review_id(), input).await?;
        self.store
            .insert(review.to_record(dedupe_key, Some(self.id.clone())))
            .await?;
        Ok(review)
    }

    pub async fn schedule_review(
        &self,
        source: impl Into<ReviewSourceLike>,
    ) -> Result<ReviewSession, ReviewSessionError> {
        self.schedule_review_with_options(source, ReviewOptions::default())
            .await
    }

    pub async fn schedule_review_with_options(
        &self,
        source: impl Into<ReviewSourceLike>,
        mut options: ReviewOptions,
    ) -> Result<ReviewSession, ReviewSessionError> {
        let source = source.into().resolve()?;
        options.config_snapshot = Some(
            self.effective_config_snapshot(&source, options.model.as_deref())
                .await?,
        );
        let input = CreateReviewSessionInput { source, options };
        let dedupe_key = input.options.dedupe_key(&input.source);
        if let Some(dedupe_key) = &dedupe_key {
            if let Some(record) = self.store.get_by_dedupe_key(dedupe_key).await? {
                return Ok(ReviewSession::from_record(record));
            }
        }
        let review = ReviewSession::queued(self.next_review_id(), input);
        self.store
            .insert(review.to_record(dedupe_key, Some(self.id.clone())))
            .await?;
        Ok(review)
    }

    pub async fn effective_config_snapshot(
        &self,
        source: &ReviewSource,
        model: Option<&str>,
    ) -> Result<EffectiveConfigSnapshot, ReviewSessionError> {
        let model_profile = self
            .selected_model_profile(model)
            .await?
            .map(|profile| profile.version_ref());
        let provider_profile = self
            .source_provider_profile(source)
            .await?
            .map(|profile| profile.version_ref());
        let mut routing = BTreeMap::new();
        if let Some(profile) = self.selected_model_profile(model).await? {
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
        if let Some(profile) = self.source_provider_profile(source).await? {
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

    pub async fn set_model_profile(
        &self,
        name: impl Into<String>,
        input: ModelProfileInput,
    ) -> Result<ModelProfile, ReviewSessionError> {
        self.profile_store
            .set_model_profile(&self.id, name.into(), input)
            .await
    }

    pub async fn get_model_profile(
        &self,
        name: &str,
    ) -> Result<Option<ModelProfile>, ReviewSessionError> {
        self.profile_store.get_model_profile(&self.id, name).await
    }

    pub async fn list_model_profiles(&self) -> Result<Vec<ModelProfile>, ReviewSessionError> {
        self.profile_store.list_model_profiles(&self.id).await
    }

    pub async fn set_provider_profile(
        &self,
        name: impl Into<String>,
        input: ProviderProfileInput,
    ) -> Result<ProviderProfile, ReviewSessionError> {
        self.profile_store
            .set_provider_profile(&self.id, name.into(), input)
            .await
    }

    pub async fn get_provider_profile(
        &self,
        name: &str,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError> {
        self.profile_store
            .get_provider_profile(&self.id, name)
            .await
    }

    pub async fn list_provider_profiles(&self) -> Result<Vec<ProviderProfile>, ReviewSessionError> {
        self.profile_store.list_provider_profiles(&self.id).await
    }

    fn next_review_id(&self) -> ReviewSessionId {
        let id = self.next_review_id.fetch_add(1, Ordering::SeqCst);
        ReviewSessionId(format!("review-{id}"))
    }

    async fn selected_model_profile(
        &self,
        model: Option<&str>,
    ) -> Result<Option<ModelProfile>, ReviewSessionError> {
        let name = model.unwrap_or("default");
        self.profile_store.get_model_profile(&self.id, name).await
    }

    async fn source_provider_profile(
        &self,
        source: &ReviewSource,
    ) -> Result<Option<ProviderProfile>, ReviewSessionError> {
        let name = match source {
            ReviewSource::GithubPullRequest { .. } => "github",
            ReviewSource::GitlabMergeRequest { .. } => "gitlab",
            ReviewSource::PerforceChangelist { .. } => "perforce",
            ReviewSource::Custom { provider, .. } => provider.as_str(),
            ReviewSource::Local { .. } | ReviewSource::RawSnapshot { .. } => return Ok(None),
        };
        self.profile_store
            .get_provider_profile(&self.id, name)
            .await
    }
}

#[cfg(test)]
mod tests;
