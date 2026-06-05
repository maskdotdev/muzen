use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::contracts::{AgentBudget, Role};
use crate::reviewer::{
    ReviewEvent as InternalReviewEvent, ReviewEventRecord as InternalReviewEventRecord,
};
use crate::runner::{
    execute_run_start, RunAgentBudgetParams, RunLimitParams, RunSessionParams, RunStartParams,
    RunnerArtifact, RunnerArtifactView, RunnerFinding, RunnerRunResult, RunnerSnapshotSummary,
    RUNNER_PROTOCOL_VERSION,
};
use crate::util::timestamp_utc;

mod config;
mod profiles;
mod store;
mod worker;

pub use config::{HostConfiguration, HostSchedulingConfiguration, SchedulingFairnessStrategy};
pub use profiles::{
    InMemoryWorkspaceProfileStore, ModelProfile, ModelProfileInput, ModelProviderKind,
    ProviderProfile, ProviderProfileInput, SourceProviderKind, WorkspaceProfileStore,
};
pub use store::{
    InMemoryReviewSessionStore, ReviewAttemptFailure, ReviewCancellationRecord,
    ReviewLeaseExtension, ReviewRetryPolicy, ReviewSessionRecord, ReviewSessionStore,
    ReviewWorkerClaim, ReviewWorkerClaimOptions, ReviewWorkerConcurrencyLimits, ReviewWorkerLease,
};
pub use worker::{ReviewWorker, ReviewWorkerRun};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ReviewSessionError {
    #[error("invalid review source `{input}`: {reason}")]
    InvalidSource { input: String, reason: String },
    #[error("review source `{source_key}` cannot run through the local runner until provider materialization exists")]
    UnsupportedSourceForLocalRunner { source_key: String },
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReviewSource {
    Local {
        repo: PathBuf,
        #[serde(default)]
        changed_files: Vec<String>,
    },
    GithubPullRequest {
        owner: String,
        repo: String,
        number: u64,
    },
    GitlabMergeRequest {
        owner: String,
        repo: String,
        number: u64,
    },
}

impl ReviewSource {
    pub fn local(repo: impl Into<PathBuf>) -> Self {
        Self::Local {
            repo: repo.into(),
            changed_files: Vec::new(),
        }
    }

    pub fn local_with_changed_files(
        repo: impl Into<PathBuf>,
        changed_files: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self::Local {
            repo: repo.into(),
            changed_files: changed_files.into_iter().map(Into::into).collect(),
        }
    }

    pub fn github_pull_request(
        owner: impl Into<String>,
        repo: impl Into<String>,
        number: u64,
    ) -> Result<Self, ReviewSessionError> {
        let owner = owner.into();
        let repo = repo.into();
        validate_repo_source_parts("github", &owner, &repo, number)?;
        Ok(Self::GithubPullRequest {
            owner,
            repo,
            number,
        })
    }

    pub fn gitlab_merge_request(
        owner: impl Into<String>,
        repo: impl Into<String>,
        number: u64,
    ) -> Result<Self, ReviewSessionError> {
        let owner = owner.into();
        let repo = repo.into();
        validate_repo_source_parts("gitlab", &owner, &repo, number)?;
        Ok(Self::GitlabMergeRequest {
            owner,
            repo,
            number,
        })
    }

    pub fn source_key(&self) -> String {
        match self {
            Self::Local { repo, .. } => format!("local:{}", repo.display()),
            Self::GithubPullRequest {
                owner,
                repo,
                number,
            } => format!("github:{owner}/{repo}#{number}"),
            Self::GitlabMergeRequest {
                owner,
                repo,
                number,
            } => format!("gitlab:{owner}/{repo}!{number}"),
        }
    }

    pub fn local_repo(&self) -> Option<&Path> {
        match self {
            Self::Local { repo, .. } => Some(repo.as_path()),
            Self::GithubPullRequest { .. } | Self::GitlabMergeRequest { .. } => None,
        }
    }

    fn runner_changed_files(&self, scope: &ReviewScope) -> Vec<String> {
        if !scope.files.is_empty() {
            return scope.files.clone();
        }
        match self {
            Self::Local { changed_files, .. } => changed_files.clone(),
            Self::GithubPullRequest { .. } | Self::GitlabMergeRequest { .. } => Vec::new(),
        }
    }
}

impl FromStr for ReviewSource {
    type Err = ReviewSessionError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = input.strip_prefix("github:") {
            let (owner, repo, number) = parse_repo_change(input, rest, '#')?;
            return Self::github_pull_request(owner, repo, number);
        }
        if let Some(rest) = input.strip_prefix("gitlab:") {
            let (owner, repo, number) = parse_repo_change(input, rest, '!')?;
            return Self::gitlab_merge_request(owner, repo, number);
        }
        if let Some(rest) = input.strip_prefix("local:") {
            if rest.trim().is_empty() {
                return Err(ReviewSessionError::InvalidSource {
                    input: input.to_string(),
                    reason: "local source path is empty".to_string(),
                });
            }
            return Ok(Self::local(PathBuf::from(rest)));
        }
        Err(ReviewSessionError::InvalidSource {
            input: input.to_string(),
            reason: "expected github:owner/repo#number, gitlab:owner/repo!number, or local:path"
                .to_string(),
        })
    }
}

impl std::fmt::Display for ReviewSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.source_key())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewSourceLike {
    Source(ReviewSource),
    Shorthand(String),
    LocalPath(PathBuf),
}

impl ReviewSourceLike {
    pub fn resolve(self) -> Result<ReviewSource, ReviewSessionError> {
        match self {
            Self::Source(source) => Ok(source),
            Self::Shorthand(source) => ReviewSource::from_str(&source),
            Self::LocalPath(path) => Ok(ReviewSource::local(path)),
        }
    }
}

impl From<ReviewSource> for ReviewSourceLike {
    fn from(value: ReviewSource) -> Self {
        Self::Source(value)
    }
}

impl From<String> for ReviewSourceLike {
    fn from(value: String) -> Self {
        Self::Shorthand(value)
    }
}

impl From<&str> for ReviewSourceLike {
    fn from(value: &str) -> Self {
        Self::Shorthand(value.to_string())
    }
}

impl From<PathBuf> for ReviewSourceLike {
    fn from(value: PathBuf) -> Self {
        Self::LocalPath(value)
    }
}

impl From<&Path> for ReviewSourceLike {
    fn from(value: &Path) -> Self {
        Self::LocalPath(value.to_path_buf())
    }
}

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
        let Some(repo) = self.source.local_repo() else {
            return Err(ReviewSessionError::UnsupportedSourceForLocalRunner {
                source_key: self.source.source_key(),
            });
        };
        let changed_files = self.source.runner_changed_files(&self.options.scope);
        Ok(RunStartParams {
            protocol_version: Some(RUNNER_PROTOCOL_VERSION.to_string()),
            run_id: Some(review_id.as_str().to_string()),
            repo: repo.to_path_buf(),
            changed_files,
            sessions: self.options.runner_sessions(),
            limits: self.options.limits.map(ReviewLimits::into_runner_limits),
            model: None,
            tools: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewOptions {
    #[serde(default)]
    pub dedupe: DedupePolicy,
    #[serde(default)]
    pub cancel_superseded: bool,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub scope: ReviewScope,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub sessions: Vec<ReviewAgentSession>,
    #[serde(default)]
    pub limits: Option<ReviewLimits>,
    #[serde(default)]
    pub config_snapshot: Option<EffectiveConfigSnapshot>,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            dedupe: DedupePolicy::None,
            cancel_superseded: false,
            user_id: None,
            model: None,
            scope: ReviewScope::default(),
            metadata: BTreeMap::new(),
            sessions: Vec::new(),
            limits: None,
            config_snapshot: None,
        }
    }
}

impl ReviewOptions {
    fn runner_sessions(&self) -> Vec<RunSessionParams> {
        self.sessions
            .iter()
            .map(|session| session.to_runner_session(self.model.as_deref()))
            .collect()
    }

    fn dedupe_key(&self, source: &ReviewSource) -> Option<String> {
        self.dedupe.key_for_source(source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupePolicy {
    None,
    Source,
    SourceHead,
    Key(String),
}

impl Default for DedupePolicy {
    fn default() -> Self {
        Self::None
    }
}

impl DedupePolicy {
    fn key_for_source(&self, source: &ReviewSource) -> Option<String> {
        match self {
            Self::None => None,
            Self::Source => Some(format!("source:{}", source.source_key())),
            Self::SourceHead => Some(format!("source-head:{}", source.source_key())),
            Self::Key(key) => Some(format!("key:{key}")),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScope {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAgentSession {
    pub id: String,
    pub role: Role,
    pub objective: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model_profile_id: Option<String>,
    #[serde(default)]
    pub budget: Option<AgentBudget>,
}

impl ReviewAgentSession {
    pub fn new(id: impl Into<String>, role: Role, objective: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role,
            objective: objective.into(),
            cwd: None,
            model_profile_id: None,
            budget: None,
        }
    }

    fn to_runner_session(&self, default_model: Option<&str>) -> RunSessionParams {
        RunSessionParams {
            id: self.id.clone(),
            role: self.role,
            objective: self.objective.clone(),
            cwd: self.cwd.clone(),
            model_profile_id: self
                .model_profile_id
                .clone()
                .or_else(|| default_model.map(str::to_string)),
            budget: self.budget.as_ref().map(|budget| RunAgentBudgetParams {
                max_turns: budget.max_turns,
                max_tool_calls: budget.max_tool_calls,
                max_prompt_tokens: budget.max_prompt_tokens,
                max_output_tokens: budget.max_output_tokens,
            }),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewLimits {
    #[serde(default)]
    pub max_active_sessions: Option<usize>,
    #[serde(default)]
    pub max_file_bytes: Option<usize>,
    #[serde(default)]
    pub max_search_matches: Option<usize>,
}

impl ReviewLimits {
    fn into_runner_limits(self) -> RunLimitParams {
        RunLimitParams {
            max_active_sessions: self.max_active_sessions,
            max_file_bytes: self.max_file_bytes,
            max_search_matches: self.max_search_matches,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveConfigSnapshot {
    #[serde(default)]
    pub model_profile: Option<ProfileVersionRef>,
    #[serde(default)]
    pub provider_profile: Option<ProfileVersionRef>,
    #[serde(default)]
    pub routing: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileVersionRef {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub secret_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewSessionId(String);

impl ReviewSessionId {
    pub fn new(id: impl Into<String>) -> Result<Self, ReviewSessionError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(ReviewSessionError::EmptyReviewSessionId);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReviewSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Created,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ReviewStatus {
    pub fn from_runner_status(status: &str) -> Self {
        match status {
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "running" => Self::Running,
            "created" => Self::Created,
            "queued" => Self::Queued,
            "failed" | "partial" => Self::Failed,
            _ => Self::Failed,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCancelOptions {
    #[serde(default)]
    pub reason: Option<String>,
}

impl ReviewCancelOptions {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
        }
    }
}

impl From<String> for ReviewCancelOptions {
    fn from(reason: String) -> Self {
        Self::new(reason)
    }
}

impl From<&str> for ReviewCancelOptions {
    fn from(reason: &str) -> Self {
        Self::new(reason)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewArtifactView {
    Redacted,
    Raw,
}

impl Default for ReviewArtifactView {
    fn default() -> Self {
        Self::Redacted
    }
}

impl From<ReviewArtifactView> for RunnerArtifactView {
    fn from(value: ReviewArtifactView) -> Self {
        match value {
            ReviewArtifactView::Redacted => Self::Redacted,
            ReviewArtifactView::Raw => Self::Raw,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifactReadOptions {
    #[serde(default)]
    pub view: ReviewArtifactView,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifactExportOptions {
    #[serde(default)]
    pub view: ReviewArtifactView,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub max_artifacts: Option<usize>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifactExport {
    pub view: ReviewArtifactView,
    pub artifact_count: usize,
    pub total_bytes: usize,
    pub artifacts: Vec<ReviewArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifact {
    pub artifact_id: String,
    pub bytes: usize,
    pub content_hash: String,
    pub content: String,
}

impl ReviewArtifact {
    fn from_runner_artifact(artifact: &RunnerArtifact) -> Self {
        Self {
            artifact_id: artifact.artifact_id.clone(),
            bytes: artifact.bytes,
            content_hash: artifact.content_hash.clone(),
            content: artifact.content.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSessionSnapshot {
    pub id: ReviewSessionId,
    pub status: ReviewStatus,
    pub source: ReviewSource,
    #[serde(default)]
    pub result: Option<ReviewResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewResult {
    pub review_id: ReviewSessionId,
    pub session_id: ReviewSessionId,
    pub status: ReviewStatus,
    pub conclusion: ReviewConclusion,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub coverage: ReviewCoverage,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl ReviewResult {
    pub fn from_runner_result(
        review_id: ReviewSessionId,
        source: &ReviewSource,
        result: RunnerRunResult,
    ) -> Self {
        let findings = result
            .findings
            .iter()
            .map(ReviewFinding::from_runner_finding)
            .collect::<Vec<_>>();
        let conclusion = ReviewConclusion::from_findings(&findings);
        let coverage = ReviewCoverage::from_runner_snapshots(&result.snapshots);
        let status = ReviewStatus::from_runner_status(&result.status);
        let mut metadata = BTreeMap::new();
        metadata.insert("runnerRunId".to_string(), json!(result.run_id));
        metadata.insert("runnerStatus".to_string(), json!(result.status));
        metadata.insert("source".to_string(), json!(source.source_key()));
        Self {
            review_id: review_id.clone(),
            session_id: review_id,
            status,
            conclusion,
            summary: review_summary(&result.summary, findings.len()),
            findings,
            coverage,
            metadata,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewConclusion {
    Approved,
    Commented,
    ChangesRequested,
}

impl ReviewConclusion {
    fn from_findings(findings: &[ReviewFinding]) -> Self {
        if findings
            .iter()
            .any(|finding| finding.severity == ReviewFindingSeverity::Error)
        {
            return Self::ChangesRequested;
        }
        if findings.is_empty() {
            Self::Approved
        } else {
            Self::Commented
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFinding {
    pub id: String,
    pub severity: ReviewFindingSeverity,
    pub category: ReviewFindingCategory,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub location: Option<ReviewFindingLocation>,
    #[serde(default)]
    pub suggested_fix: Option<ReviewSuggestedFix>,
    #[serde(default)]
    pub confidence: Option<f32>,
}

impl ReviewFinding {
    fn from_runner_finding(finding: &RunnerFinding) -> Self {
        Self {
            id: finding.id.clone(),
            severity: if finding.publishable {
                ReviewFindingSeverity::Error
            } else {
                ReviewFindingSeverity::Info
            },
            category: ReviewFindingCategory::Other,
            title: finding.title.clone(),
            message: finding.claim.clone(),
            location: None,
            suggested_fix: None,
            confidence: None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingCategory {
    Bug,
    Security,
    Performance,
    Maintainability,
    Style,
    Test,
    Docs,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFindingLocation {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub start_column: Option<usize>,
    #[serde(default)]
    pub end_column: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSuggestedFix {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCoverage {
    pub files_considered: usize,
    pub files_reviewed: usize,
    pub files_skipped: usize,
}

impl ReviewCoverage {
    fn from_runner_snapshots(snapshots: &[RunnerSnapshotSummary]) -> Self {
        let files_considered = snapshots.iter().map(|snapshot| snapshot.files).sum();
        let files_reviewed = snapshots
            .iter()
            .map(|snapshot| snapshot.captured_files)
            .sum();
        Self {
            files_considered,
            files_reviewed,
            files_skipped: files_considered.saturating_sub(files_reviewed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewEvent {
    pub cursor: String,
    #[serde(rename = "type")]
    pub event_type: ReviewEventType,
    pub review_id: ReviewSessionId,
    pub timestamp_utc: String,
    #[serde(default)]
    pub payload: Value,
}

impl ReviewEvent {
    pub fn from_internal_record(record: InternalReviewEventRecord) -> Self {
        let review_id = ReviewSessionId(record.run_id.unwrap_or_else(|| "unknown".to_string()));
        let event_type = ReviewEventType::from_internal(&record.event);
        let payload = serde_json::to_value(&record.event).unwrap_or(Value::Null);
        Self {
            cursor: record.seq.to_string(),
            event_type,
            review_id,
            timestamp_utc: record.timestamp_utc,
            payload,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewEventType {
    #[serde(rename = "session.created")]
    SessionCreated,
    #[serde(rename = "session.queued")]
    SessionQueued,
    #[serde(rename = "session.claimed")]
    SessionClaimed,
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "source.resolved")]
    SourceResolved,
    #[serde(rename = "scope.inferred")]
    ScopeInferred,
    #[serde(rename = "scope.overridden")]
    ScopeOverridden,
    #[serde(rename = "repo.materialized")]
    RepoMaterialized,
    #[serde(rename = "plan.created")]
    PlanCreated,
    #[serde(rename = "agent.started")]
    AgentStarted,
    #[serde(rename = "agent.completed")]
    AgentCompleted,
    #[serde(rename = "tool.started")]
    ToolStarted,
    #[serde(rename = "tool.completed")]
    ToolCompleted,
    #[serde(rename = "finding.created")]
    FindingCreated,
    #[serde(rename = "finding.updated")]
    FindingUpdated,
    #[serde(rename = "review.result_created")]
    ReviewResultCreated,
    #[serde(rename = "session.completed")]
    SessionCompleted,
    #[serde(rename = "session.failed")]
    SessionFailed,
    #[serde(rename = "session.cancelled")]
    SessionCancelled,
    #[serde(rename = "runner.event")]
    RunnerEvent,
}

impl ReviewEventType {
    fn from_internal(event: &InternalReviewEvent) -> Self {
        match event {
            InternalReviewEvent::RunStarted { .. } => Self::SessionStarted,
            InternalReviewEvent::RepoManifestCompleted { .. } => Self::ScopeInferred,
            InternalReviewEvent::SnapshotStarted { .. } => Self::RunnerEvent,
            InternalReviewEvent::SessionStarted { .. } => Self::AgentStarted,
            InternalReviewEvent::ModelStarted { .. } => Self::RunnerEvent,
            InternalReviewEvent::ModelCompleted { .. } => Self::RunnerEvent,
            InternalReviewEvent::ToolBatchStarted { .. } => Self::ToolStarted,
            InternalReviewEvent::ToolCallCompleted { .. }
            | InternalReviewEvent::ToolCallDenied { .. } => Self::ToolCompleted,
            InternalReviewEvent::ArtifactCreated { .. } => Self::RunnerEvent,
            InternalReviewEvent::FindingRecorded { .. } => Self::FindingCreated,
            InternalReviewEvent::SearchBatchCompleted { .. } => Self::RunnerEvent,
            InternalReviewEvent::SessionFinished { .. } => Self::AgentCompleted,
            InternalReviewEvent::SnapshotFinished { .. } => Self::RepoMaterialized,
            InternalReviewEvent::RunFinished { status } if status == "completed" => {
                Self::SessionCompleted
            }
            InternalReviewEvent::RunFinished { status } if status == "cancelled" => {
                Self::SessionCancelled
            }
            InternalReviewEvent::RunFinished { .. } => Self::SessionFailed,
        }
    }
}

fn parse_repo_change(
    input: &str,
    rest: &str,
    number_delimiter: char,
) -> Result<(String, String, u64), ReviewSessionError> {
    let Some((path, number)) = rest.rsplit_once(number_delimiter) else {
        return Err(ReviewSessionError::InvalidSource {
            input: input.to_string(),
            reason: format!("missing `{number_delimiter}` review number delimiter"),
        });
    };
    let Some((owner, repo)) = path.rsplit_once('/') else {
        return Err(ReviewSessionError::InvalidSource {
            input: input.to_string(),
            reason: "missing owner/repo path".to_string(),
        });
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| ReviewSessionError::InvalidSource {
            input: input.to_string(),
            reason: "review number must be a positive integer".to_string(),
        })?;
    Ok((owner.to_string(), repo.to_string(), number))
}

fn validate_repo_source_parts(
    provider: &str,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<(), ReviewSessionError> {
    if owner.trim().is_empty() {
        return Err(ReviewSessionError::InvalidSource {
            input: provider.to_string(),
            reason: "owner is empty".to_string(),
        });
    }
    if repo.trim().is_empty() {
        return Err(ReviewSessionError::InvalidSource {
            input: provider.to_string(),
            reason: "repo is empty".to_string(),
        });
    }
    if number == 0 {
        return Err(ReviewSessionError::InvalidSource {
            input: provider.to_string(),
            reason: "review number must be greater than zero".to_string(),
        });
    }
    Ok(())
}

fn review_summary(summary: &crate::runner::RunnerRunSummary, findings: usize) -> String {
    format!(
        "Review completed {}/{} session(s), produced {} finding(s), used {} model call(s), {} tool call(s), and {} total token(s).",
        summary.completed_sessions,
        summary.sessions,
        findings,
        summary.model_calls,
        summary.tool_calls,
        summary.total_tokens
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RunnerRunSummary, RunnerSnapshotSummary};

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
        assert_eq!(start.repo, PathBuf::from("."));
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
    fn keeps_provider_sources_out_of_local_runner_start() {
        let input = CreateReviewSessionInput::new("github:maskdotdev/heimdaal#123").unwrap();
        let review_id = ReviewSessionId::new("review-1").unwrap();

        let error = input.into_runner_start(&review_id).unwrap_err();

        assert!(matches!(
            error,
            ReviewSessionError::UnsupportedSourceForLocalRunner { .. }
        ));
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
    fn review_worker_records_final_failure_for_unsupported_source() {
        let store = Arc::new(InMemoryReviewSessionStore::default());
        let workspace = Muzen::with_store(store.clone()).workspace("acme");
        let review = workspace
            .schedule_review("github:maskdotdev/heimdaal#123")
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
            .is_some_and(|error| error.contains("cannot run through the local runner")));
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
