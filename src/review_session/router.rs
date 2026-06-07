use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    ModelProfile, ModelProfileInput, Muzen, ProviderProfile, ProviderProfileInput, ReviewArtifact,
    ReviewArtifactReadOptions, ReviewArtifactView, ReviewCancelOptions, ReviewHttpResponse,
    ReviewOptions, ReviewSessionError, ReviewSessionId, ReviewSessionSnapshot, ReviewSource,
    WebhookHeaders, WebhookReviewOptions, CONTENT_TYPE_TEXT, HTTP_STATUS_ACCEPTED,
    HTTP_STATUS_BAD_REQUEST, HTTP_STATUS_METHOD_NOT_ALLOWED, HTTP_STATUS_NOT_FOUND,
    HTTP_STATUS_NO_CONTENT, HTTP_STATUS_OK,
};
use crate::context_engine::{
    ContextEngine, ContextEngineConfig, ContextIndexRequest, ContextManifestArtifact, ContextPack,
    ContextPackPurpose, ContextPackRequest, ContextQuery, ContextQueryKind, ContextQueryLimits,
    ContextQueryResult, SnapshotContextEngine,
};
use crate::contracts::{
    ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, SnapshotMode,
};
use crate::runtime::contracts::{EvidenceId, RuntimeError, SnapshotStoragePolicy};
use crate::runtime::repo::RepoSnapshot;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewHttpRequest {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl ReviewHttpRequest {
    pub fn new(method: impl Into<String>, target: impl AsRef<str>) -> Self {
        let target = target.as_ref();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Self {
            method: method.into().to_ascii_uppercase(),
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            query: parse_query_lossy(query),
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.headers
            .insert(normalize_header_name(name.as_ref()), value.into());
        self
    }

    pub fn query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.insert(name.into(), value.into());
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn json<T: Serialize>(self, body: &T) -> Result<Self, ReviewSessionError> {
        let body = serde_json::to_vec(body).map_err(|error| {
            ReviewSessionError::Http(format!("failed to serialize JSON request: {error}"))
        })?;
        Ok(self.header("Content-Type", "application/json").body(body))
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&normalize_header_name(name))
            .map(String::as_str)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReviewHttpRouterOptions {
    pub github_webhook_secret: Option<String>,
    pub gitlab_webhook_secret: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewHttpRouter {
    muzen: Muzen,
    options: ReviewHttpRouterOptions,
}

impl ReviewHttpRouter {
    pub fn new(muzen: Muzen) -> Self {
        Self::with_options(muzen, ReviewHttpRouterOptions::default())
    }

    pub fn with_options(muzen: Muzen, options: ReviewHttpRouterOptions) -> Self {
        Self { muzen, options }
    }

    pub fn handle(&self, request: ReviewHttpRequest) -> ReviewHttpResponse {
        match self.try_handle(&request) {
            Ok(response) => response,
            Err(error) => error.into_response(),
        }
    }

    pub fn try_handle(
        &self,
        request: &ReviewHttpRequest,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        let segments = path_segments(&request.path)?;
        let segment_refs = segments.iter().map(String::as_str).collect::<Vec<_>>();
        match segment_refs.as_slice() {
            ["v1", "reviews"] => self.handle_reviews_collection(request),
            ["v1", "reviews", review_id] => self.handle_review(request, review_id),
            ["v1", "reviews", review_id, "result"] => self.handle_review_result(request, review_id),
            ["v1", "reviews", review_id, "cancel"] => self.handle_cancel(request, review_id),
            ["v1", "reviews", review_id, "events"] => self.handle_review_events(request, review_id),
            ["v1", "reviews", review_id, "events", "stream"] => {
                self.handle_review_event_stream(request, review_id)
            }
            ["v1", "reviews", review_id, "artifacts", "export"] => {
                self.handle_artifact_export(request, review_id)
            }
            ["v1", "reviews", review_id, "artifacts", artifact_id] => {
                self.handle_artifact_read(request, review_id, artifact_id)
            }
            ["v1", "workspaces", workspace_id, "reviews"] => {
                self.handle_workspace_reviews_collection(request, workspace_id)
            }
            ["v1", "webhooks", provider] => self.handle_webhook(request, None, provider),
            ["v1", "workspaces", workspace_id, "webhooks", provider] => {
                self.handle_webhook(request, Some(workspace_id), provider)
            }
            ["v1", "workspaces", workspace_id, "models"] => {
                self.handle_model_profiles_collection(request, workspace_id)
            }
            ["v1", "workspaces", workspace_id, "models", name] => {
                self.handle_model_profile(request, workspace_id, name)
            }
            ["v1", "workspaces", workspace_id, "providers"] => {
                self.handle_provider_profiles_collection(request, workspace_id)
            }
            ["v1", "workspaces", workspace_id, "providers", name] => {
                self.handle_provider_profile(request, workspace_id, name)
            }
            ["v1", "workspaces", workspace_id, "context", "index"] => {
                self.handle_context_index(request, workspace_id)
            }
            ["v1", "workspaces", workspace_id, "context", "packs"] => {
                self.handle_context_pack(request, workspace_id)
            }
            ["v1", "workspaces", workspace_id, "context", "query"] => {
                self.handle_context_query(request, workspace_id)
            }
            _ => Err(ReviewHttpRouteError::NotFound(format!(
                "no Muzen route matches {} {}",
                request.method, request.path
            ))),
        }
    }

    fn handle_reviews_collection(
        &self,
        request: &ReviewHttpRequest,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: CreateReviewBody = json_body(request)?;
        let review = self
            .muzen
            .schedule_review_with_options(body.source, body.options)?;
        response_json(
            HTTP_STATUS_ACCEPTED,
            &ReviewResponse {
                review: review.refresh(),
            },
        )
    }

    fn handle_workspace_reviews_collection(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: CreateReviewBody = json_body(request)?;
        let review = self
            .muzen
            .workspace(workspace_id)
            .schedule_review_with_options(body.source, body.options)?;
        response_json(
            HTTP_STATUS_ACCEPTED,
            &ReviewResponse {
                review: review.refresh(),
            },
        )
    }

    fn handle_review(
        &self,
        request: &ReviewHttpRequest,
        review_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "GET")?;
        response_json(
            HTTP_STATUS_OK,
            &ReviewResponse {
                review: self.review_snapshot(review_id)?,
            },
        )
    }

    fn handle_review_result(
        &self,
        request: &ReviewHttpRequest,
        review_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "GET")?;
        let id = ReviewSessionId::new(review_id)?;
        let record = self.review_record(&id)?;
        let Some(result) = record.result else {
            return Ok(ReviewHttpResponse::empty(HTTP_STATUS_NO_CONTENT));
        };
        response_json(HTTP_STATUS_OK, &ReviewResultResponse { result })
    }

    fn handle_cancel(
        &self,
        request: &ReviewHttpRequest,
        review_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let options = optional_json_body::<ReviewCancelOptions>(request)?;
        let id = ReviewSessionId::new(review_id)?;
        let record = self.muzen.store.request_cancellation(&id, options)?;
        response_json(
            HTTP_STATUS_OK,
            &ReviewResponse {
                review: super::ReviewSession::from_record(record).refresh(),
            },
        )
    }

    fn handle_review_events(
        &self,
        request: &ReviewHttpRequest,
        review_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "GET")?;
        let id = ReviewSessionId::new(review_id)?;
        Ok(self
            .muzen
            .review_events_response(&id, request.query("after"))?)
    }

    fn handle_review_event_stream(
        &self,
        request: &ReviewHttpRequest,
        review_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "GET")?;
        let id = ReviewSessionId::new(review_id)?;
        Ok(self
            .muzen
            .review_events_sse_response(&id, request.query("after"))?)
    }

    fn handle_artifact_read(
        &self,
        request: &ReviewHttpRequest,
        review_id: &str,
        artifact_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "GET")?;
        let id = ReviewSessionId::new(review_id)?;
        let review = super::ReviewSession::from_record(self.review_record(&id)?);
        let artifact = review.read_artifact(
            artifact_id,
            ReviewArtifactReadOptions {
                view: artifact_view(request)?,
            },
        )?;
        response_json(HTTP_STATUS_OK, &ReviewArtifactResponse { artifact })
    }

    fn handle_artifact_export(
        &self,
        request: &ReviewHttpRequest,
        review_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let id = ReviewSessionId::new(review_id)?;
        let review = super::ReviewSession::from_record(self.review_record(&id)?);
        let export = review.export_artifacts(optional_json_body(request)?)?;
        response_json(HTTP_STATUS_OK, &export)
    }

    fn handle_webhook(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: Option<&str>,
        provider: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let headers = WebhookHeaders::from_iter(
            request
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        let workspace = self.muzen.workspace(workspace_id.unwrap_or("default"));
        let delivery = match provider {
            "github" => workspace.handle_github_webhook(
                &headers,
                &request.body,
                self.options.github_webhook_secret.as_deref(),
                WebhookReviewOptions::new(ReviewOptions::default()),
            )?,
            "gitlab" => workspace.handle_gitlab_webhook(
                &headers,
                &request.body,
                self.options.gitlab_webhook_secret.as_deref(),
                WebhookReviewOptions::new(ReviewOptions::default()),
            )?,
            _ => {
                return Err(ReviewHttpRouteError::NotFound(format!(
                    "unsupported webhook provider `{provider}`"
                )))
            }
        };
        Ok(delivery.http_response()?)
    }

    fn handle_model_profiles_collection(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "GET")?;
        let profiles = self.muzen.workspace(workspace_id).list_model_profiles()?;
        response_json(HTTP_STATUS_OK, &ModelProfilesResponse { profiles })
    }

    fn handle_model_profile(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
        name: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        let workspace = self.muzen.workspace(workspace_id);
        match request.method.as_str() {
            "PUT" => {
                let profile =
                    workspace.set_model_profile(name, json_body::<ModelProfileInput>(request)?)?;
                response_json(HTTP_STATUS_OK, &ModelProfileResponse { profile })
            }
            "GET" => {
                let Some(profile) = workspace.get_model_profile(name)? else {
                    return Ok(ReviewHttpResponse::empty(HTTP_STATUS_NO_CONTENT));
                };
                response_json(HTTP_STATUS_OK, &ModelProfileResponse { profile })
            }
            _ => Err(ReviewHttpRouteError::MethodNotAllowed(format!(
                "{} is not allowed for workspace model profiles",
                request.method
            ))),
        }
    }

    fn handle_provider_profiles_collection(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "GET")?;
        let profiles = self
            .muzen
            .workspace(workspace_id)
            .list_provider_profiles()?;
        response_json(HTTP_STATUS_OK, &ProviderProfilesResponse { profiles })
    }

    fn handle_provider_profile(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
        name: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        let workspace = self.muzen.workspace(workspace_id);
        match request.method.as_str() {
            "PUT" => {
                let profile = workspace
                    .set_provider_profile(name, json_body::<ProviderProfileInput>(request)?)?;
                response_json(HTTP_STATUS_OK, &ProviderProfileResponse { profile })
            }
            "GET" => {
                let Some(profile) = workspace.get_provider_profile(name)? else {
                    return Ok(ReviewHttpResponse::empty(HTTP_STATUS_NO_CONTENT));
                };
                response_json(HTTP_STATUS_OK, &ProviderProfileResponse { profile })
            }
            _ => Err(ReviewHttpRouteError::MethodNotAllowed(format!(
                "{} is not allowed for workspace provider profiles",
                request.method
            ))),
        }
    }

    fn handle_context_index(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextIndexBody = json_body(request)?;
        let (_engine, _snapshot, manifest) = self.index_context_request(workspace_id, body)?;
        response_json(HTTP_STATUS_OK, &ContextIndexResponse { manifest })
    }

    fn handle_context_pack(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextPackBody = json_body(request)?;
        let (engine, snapshot, _manifest) = self.index_context_request(workspace_id, body.index)?;
        let pack_engine = engine.clone();
        let pack = block_on_context(async move {
            pack_engine
                .build_pack(
                    ContextPackRequest {
                        run_id: None,
                        snapshot_id: snapshot.snapshot_id.clone(),
                        session_id: None,
                        purpose: body.purpose.unwrap_or(ContextPackPurpose::GeneralReview),
                        max_tokens: body
                            .max_tokens
                            .unwrap_or_else(|| pack_engine.config_ref().max_pack_tokens),
                        seed_evidence: Vec::new(),
                    },
                    CancellationToken::new(),
                )
                .await
        })?;
        response_json(HTTP_STATUS_OK, &ContextPackResponse { pack })
    }

    fn handle_context_query(
        &self,
        request: &ReviewHttpRequest,
        workspace_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextQueryBody = json_body(request)?;
        let (engine, snapshot, _manifest) = self.index_context_request(workspace_id, body.index)?;
        let query_engine = engine.clone();
        let result = block_on_context(async move {
            query_engine
                .query(
                    ContextQuery {
                        run_id: None,
                        snapshot_id: snapshot.snapshot_id.clone(),
                        session_id: None,
                        purpose: body.purpose,
                        kind: body.kind,
                        arguments: body.arguments,
                        current_evidence: body.current_evidence,
                        limits: body.limits.unwrap_or(ContextQueryLimits {
                            max_results: query_engine.config_ref().max_query_results,
                            max_tokens: query_engine.config_ref().max_pack_tokens,
                        }),
                    },
                    CancellationToken::new(),
                )
                .await
        })?;
        response_json(HTTP_STATUS_OK, &ContextQueryResponse { result })
    }

    fn index_context_request(
        &self,
        _workspace_id: &str,
        body: ContextIndexBody,
    ) -> Result<
        (
            SnapshotContextEngine,
            Arc<RepoSnapshot>,
            ContextManifestArtifact,
        ),
        ReviewHttpRouteError,
    > {
        let config = body.config.unwrap_or_else(ContextEngineConfig::snapshot_v0);
        let snapshot = build_context_snapshot_from_source(body.source, body.changed_files)?;
        let engine = SnapshotContextEngine::new(config);
        let index_engine = engine.clone();
        let index_snapshot = Arc::clone(&snapshot);
        let request = ContextIndexRequest::for_snapshot(index_snapshot, engine.config_ref());
        block_on_context(async move {
            index_engine
                .index_snapshot(request, CancellationToken::new())
                .await
        })?;
        let index = engine.get_index(&snapshot.snapshot_id).ok_or_else(|| {
            ReviewHttpRouteError::BadRequest("context index was not stored".to_string())
        })?;
        Ok((engine, snapshot, index.manifest_artifact.clone()))
    }

    fn review_snapshot(
        &self,
        review_id: &str,
    ) -> Result<ReviewSessionSnapshot, ReviewHttpRouteError> {
        let id = ReviewSessionId::new(review_id)?;
        Ok(super::ReviewSession::from_record(self.review_record(&id)?).refresh())
    }

    fn review_record(
        &self,
        id: &ReviewSessionId,
    ) -> Result<super::ReviewSessionRecord, ReviewHttpRouteError> {
        self.muzen
            .store
            .get(id)?
            .ok_or_else(|| ReviewHttpRouteError::NotFound(format!("unknown review session `{id}`")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewHttpRouteError {
    BadRequest(String),
    NotFound(String),
    MethodNotAllowed(String),
    Core(ReviewSessionError),
}

impl ReviewHttpRouteError {
    fn into_response(self) -> ReviewHttpResponse {
        let (status_code, message) = match self {
            Self::BadRequest(message) => (HTTP_STATUS_BAD_REQUEST, message),
            Self::NotFound(message) => (HTTP_STATUS_NOT_FOUND, message),
            Self::MethodNotAllowed(message) => (HTTP_STATUS_METHOD_NOT_ALLOWED, message),
            Self::Core(error) => route_error_status(&error),
        };
        if status_code == HTTP_STATUS_NO_CONTENT {
            return ReviewHttpResponse::empty(HTTP_STATUS_NO_CONTENT);
        }
        response_json(
            status_code,
            &ErrorResponse {
                error: ErrorBody { message },
            },
        )
        .unwrap_or_else(|error| {
            ReviewHttpResponse::with_body(
                HTTP_STATUS_BAD_REQUEST,
                CONTENT_TYPE_TEXT,
                format!("{error:?}"),
            )
        })
    }
}

impl From<ReviewSessionError> for ReviewHttpRouteError {
    fn from(error: ReviewSessionError) -> Self {
        match &error {
            ReviewSessionError::InvalidSource { .. }
            | ReviewSessionError::EmptyReviewSessionId
            | ReviewSessionError::Webhook(_)
            | ReviewSessionError::Profile(_)
            | ReviewSessionError::ArtifactLimitExceeded { .. } => {
                Self::BadRequest(error.to_string())
            }
            ReviewSessionError::UnknownArtifactId { .. } => Self::NotFound(error.to_string()),
            ReviewSessionError::Store(message) if message.contains("unknown review session") => {
                Self::NotFound(error.to_string())
            }
            _ => Self::Core(error),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateReviewBody {
    source: ReviewSource,
    #[serde(default)]
    options: ReviewOptions,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewResponse {
    review: ReviewSessionSnapshot,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewResultResponse {
    result: super::ReviewResult,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewArtifactResponse {
    artifact: ReviewArtifact,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextIndexBody {
    source: ReviewSource,
    #[serde(default)]
    changed_files: Vec<String>,
    #[serde(default)]
    config: Option<ContextEngineConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextPackBody {
    #[serde(flatten)]
    index: ContextIndexBody,
    #[serde(default)]
    purpose: Option<ContextPackPurpose>,
    #[serde(default)]
    max_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextQueryBody {
    #[serde(flatten)]
    index: ContextIndexBody,
    #[serde(default)]
    purpose: Option<ContextPackPurpose>,
    kind: ContextQueryKind,
    #[serde(default)]
    arguments: serde_json::Value,
    #[serde(default)]
    current_evidence: Vec<EvidenceId>,
    #[serde(default)]
    limits: Option<ContextQueryLimits>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextIndexResponse {
    manifest: ContextManifestArtifact,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextPackResponse {
    pack: ContextPack,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextQueryResponse {
    result: ContextQueryResult,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelProfileResponse {
    profile: ModelProfile,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelProfilesResponse {
    profiles: Vec<ModelProfile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderProfileResponse {
    profile: ProviderProfile,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderProfilesResponse {
    profiles: Vec<ProviderProfile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    message: String,
}

fn require_method(request: &ReviewHttpRequest, method: &str) -> Result<(), ReviewHttpRouteError> {
    if request.method == method {
        Ok(())
    } else {
        Err(ReviewHttpRouteError::MethodNotAllowed(format!(
            "{} is not allowed for {}",
            request.method, request.path
        )))
    }
}

fn json_body<T: DeserializeOwned>(request: &ReviewHttpRequest) -> Result<T, ReviewHttpRouteError> {
    if request.body.is_empty() {
        return Err(ReviewHttpRouteError::BadRequest(
            "request body must contain JSON".to_string(),
        ));
    }
    serde_json::from_slice(&request.body)
        .map_err(|error| ReviewHttpRouteError::BadRequest(format!("invalid JSON body: {error}")))
}

fn optional_json_body<T>(request: &ReviewHttpRequest) -> Result<T, ReviewHttpRouteError>
where
    T: DeserializeOwned + Default,
{
    if request.body.is_empty() {
        return Ok(T::default());
    }
    json_body(request)
}

fn response_json<T: Serialize>(
    status_code: u16,
    body: &T,
) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
    Ok(ReviewHttpResponse::json(status_code, body)?)
}

fn artifact_view(request: &ReviewHttpRequest) -> Result<ReviewArtifactView, ReviewHttpRouteError> {
    match request.query("view").unwrap_or("redacted") {
        "redacted" => Ok(ReviewArtifactView::Redacted),
        "raw" => Ok(ReviewArtifactView::Raw),
        value => Err(ReviewHttpRouteError::BadRequest(format!(
            "unsupported artifact view `{value}`"
        ))),
    }
}

fn build_context_snapshot_from_source(
    source: ReviewSource,
    changed_files_override: Vec<String>,
) -> Result<Arc<RepoSnapshot>, ReviewHttpRouteError> {
    let (root, source_changed_files) = match source {
        ReviewSource::Local {
            repo,
            changed_files,
        } => (repo, changed_files),
        ReviewSource::RawSnapshot {
            root,
            changed_files,
        } => (root, changed_files),
        other => {
            return Err(ReviewHttpRouteError::BadRequest(format!(
                "context HTTP routes require local or raw_snapshot source, got {}",
                other.source_key()
            )))
        }
    };
    let changed_files = if changed_files_override.is_empty() {
        source_changed_files
    } else {
        changed_files_override
    };
    if changed_files.is_empty() {
        return Err(ReviewHttpRouteError::BadRequest(
            "context request requires at least one changed file".to_string(),
        ));
    }
    let changed_files = changed_files
        .into_iter()
        .map(|path| ChangedFileEntryV1 {
            status: ChangedFileStatus::Modified,
            old_path: Some(PathBuf::from(&path)),
            new_path: Some(PathBuf::from(path)),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        })
        .collect::<Vec<_>>();
    let change = ChangeScopeV1 {
        kind: ChangeKind::LocalDiff,
        change_id: "context-http".to_string(),
        source_ref: "head".to_string(),
        target_ref: "base".to_string(),
        base_revision_id: "base".to_string(),
        head_revision_id: "head".to_string(),
        merge_base_revision_id: None,
        changed_files_manifest_ref: None,
        diff_manifest_ref: None,
        inline_diff: None,
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files,
    };
    RepoSnapshot::build_with_storage(
        &root,
        &PathPolicyV1::bench(200, 120),
        &change,
        SnapshotStoragePolicy::default(),
    )
    .map_err(|error| ReviewHttpRouteError::BadRequest(error.to_string()))
}

fn block_on_context<T>(
    future: impl std::future::Future<Output = Result<T, RuntimeError>> + Send + 'static,
) -> Result<T, ReviewHttpRouteError>
where
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || run_context_future(future))
            .join()
            .map_err(|_| {
                ReviewHttpRouteError::BadRequest("context worker thread panicked".to_string())
            })?
    } else {
        run_context_future(future)
    }
}

fn run_context_future<T>(
    future: impl std::future::Future<Output = Result<T, RuntimeError>>,
) -> Result<T, ReviewHttpRouteError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ReviewHttpRouteError::BadRequest(error.to_string()))?
        .block_on(future)
        .map_err(context_runtime_error)
}

fn context_runtime_error(error: RuntimeError) -> ReviewHttpRouteError {
    ReviewHttpRouteError::BadRequest(error.to_string())
}

fn route_error_status(error: &ReviewSessionError) -> (u16, String) {
    match error {
        ReviewSessionError::ResultUnavailable { .. } => (HTTP_STATUS_NO_CONTENT, error.to_string()),
        ReviewSessionError::Store(message) if message.contains("unknown review session") => {
            (HTTP_STATUS_NOT_FOUND, error.to_string())
        }
        _ => (HTTP_STATUS_BAD_REQUEST, error.to_string()),
    }
}

fn path_segments(path: &str) -> Result<Vec<String>, ReviewHttpRouteError> {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| percent_decode(segment, false))
        .collect()
}

fn parse_query_lossy(query: &str) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        result.insert(
            percent_decode_lossy(key, true),
            percent_decode_lossy(value, true),
        );
    }
    result
}

fn percent_decode(input: &str, plus_as_space: bool) -> Result<String, ReviewHttpRouteError> {
    let mut output = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' if plus_as_space => {
                output.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(ReviewHttpRouteError::BadRequest(
                        "invalid percent-encoded path segment".to_string(),
                    ));
                }
                let high = hex_value(bytes[index + 1]).ok_or_else(|| {
                    ReviewHttpRouteError::BadRequest(
                        "invalid percent-encoded path segment".to_string(),
                    )
                })?;
                let low = hex_value(bytes[index + 2]).ok_or_else(|| {
                    ReviewHttpRouteError::BadRequest(
                        "invalid percent-encoded path segment".to_string(),
                    )
                })?;
                output.push((high << 4) | low);
                index += 3;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| {
        ReviewHttpRouteError::BadRequest("path segment is not valid UTF-8".to_string())
    })
}

fn percent_decode_lossy(input: &str, plus_as_space: bool) -> String {
    percent_decode(input, plus_as_space).unwrap_or_else(|_| input.to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn normalize_header_name(name: &str) -> String {
    name.to_ascii_lowercase()
}
