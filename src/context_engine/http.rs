use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::context_engine::{
    ContextEngine, ContextEngineConfig, ContextFeedback, ContextFeedbackReceipt,
    ContextIndexRequest, ContextLearningApproval, ContextLearningApprovalReceipt,
    ContextLearningScope, ContextLearningSource, ContextManifestArtifact, ContextPack,
    ContextPackPurpose, ContextPackRequest, ContextQuery, ContextQueryKind, ContextQueryLimits,
    ContextQueryResult, CrossRepoContractCandidate, SnapshotContextEngine,
};
use crate::remote_http::{
    ReviewHttpRequest, ReviewHttpResponse, ReviewHttpRouteError, HTTP_STATUS_OK,
};
use crate::review_sources::ReviewSource;
use crate::reviewer_kernel::kernel_types::{EvidenceId, RuntimeError, SnapshotStoragePolicy};
use crate::reviewer_kernel::review_contract::{
    ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, SnapshotMode,
};
use crate::workspace::RepoSnapshot;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContextHttpRouterOptions {
    pub learning_store_root: Option<PathBuf>,
    pub derived_cache_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ContextHttpRouter {
    options: ContextHttpRouterOptions,
    engines: Arc<Mutex<BTreeMap<String, SnapshotContextEngine>>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ContextHttpRoute {
    Index,
    Pack,
    Query,
    Feedback,
    LearningApproval,
}

impl ContextHttpRouter {
    pub fn with_options(options: ContextHttpRouterOptions) -> Self {
        Self {
            options,
            engines: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn handle(
        &self,
        route: ContextHttpRoute,
        request: &ReviewHttpRequest,
        project_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        match route {
            ContextHttpRoute::Index => self.handle_index(request, project_id),
            ContextHttpRoute::Pack => self.handle_pack(request, project_id),
            ContextHttpRoute::Query => self.handle_query(request, project_id),
            ContextHttpRoute::Feedback => self.handle_feedback(request, project_id),
            ContextHttpRoute::LearningApproval => {
                self.handle_learning_approval(request, project_id)
            }
        }
    }

    fn handle_index(
        &self,
        request: &ReviewHttpRequest,
        project_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextIndexBody = json_body(request)?;
        let (_engine, _snapshot, manifest) = self.index_request(project_id, body)?;
        response_json(HTTP_STATUS_OK, &ContextIndexResponse { manifest })
    }

    fn handle_pack(
        &self,
        request: &ReviewHttpRequest,
        project_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextPackBody = json_body(request)?;
        let (engine, snapshot, _manifest) = self.index_request(project_id, body.index)?;
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
                    },
                    CancellationToken::new(),
                )
                .await
        })?;
        response_json(HTTP_STATUS_OK, &ContextPackResponse { pack })
    }

    fn handle_query(
        &self,
        request: &ReviewHttpRequest,
        project_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextQueryBody = json_body(request)?;
        let (engine, snapshot, _manifest) = self.index_request(project_id, body.index)?;
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

    fn handle_feedback(
        &self,
        request: &ReviewHttpRequest,
        project_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextFeedbackBody = json_body(request)?;
        let (engine, snapshot, _manifest) = self.index_request(project_id, body.index)?;
        let feedback_engine = engine.clone();
        let receipt = block_on_context(async move {
            feedback_engine
                .record_feedback(
                    ContextFeedback {
                        snapshot_id: snapshot.snapshot_id.clone(),
                        evidence_ids: body.evidence_ids,
                        feedback: body.feedback,
                        source: body.learning_source,
                        scope: body.scope,
                    },
                    CancellationToken::new(),
                )
                .await
        })?;
        response_json(HTTP_STATUS_OK, &ContextFeedbackResponse { receipt })
    }

    fn handle_learning_approval(
        &self,
        request: &ReviewHttpRequest,
        project_id: &str,
    ) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
        require_method(request, "POST")?;
        let body: ContextLearningApprovalBody = json_body(request)?;
        let engine = self.engine_for_snapshot(project_id, &body.snapshot_id)?;
        let receipt = block_on_context(async move {
            engine
                .approve_learning(body.approval, CancellationToken::new())
                .await
        })?;
        response_json(HTTP_STATUS_OK, &ContextLearningApprovalResponse { receipt })
    }

    fn index_request(
        &self,
        project_id: &str,
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
        let snapshot = build_snapshot_from_source(body.source, body.changed_files)?;
        let engine = self.engine_for_workspace(project_id, config)?;
        let index_engine = engine.clone();
        let index_snapshot = Arc::clone(&snapshot);
        let mut request = ContextIndexRequest::for_snapshot(index_snapshot, engine.config_ref());
        request.host_metadata = body.host_metadata;
        request.cross_repo_contracts = body.cross_repo_contracts;
        request.allowed_cross_repo_resources =
            body.allowed_cross_repo_resources.into_iter().collect();
        block_on_context(async move {
            index_engine
                .index_snapshot(request, CancellationToken::new())
                .await
        })?;
        let index = engine.get_index(&snapshot.snapshot_id).ok_or_else(|| {
            ReviewHttpRouteError::BadRequest("context index was not stored".to_string())
        })?;
        self.engines
            .lock()
            .expect("context engine store poisoned")
            .insert(
                context_engine_key(project_id, &snapshot.snapshot_id.0),
                engine.clone(),
            );
        Ok((engine, snapshot, index.manifest_artifact.clone()))
    }

    fn engine_for_workspace(
        &self,
        project_id: &str,
        config: ContextEngineConfig,
    ) -> Result<SnapshotContextEngine, ReviewHttpRouteError> {
        let mut engine = if let Some(root) = &self.options.learning_store_root {
            SnapshotContextEngine::with_learning_store_file(
                config,
                workspace_store_path(root, project_id, "context-learnings.json"),
            )
            .map_err(context_runtime_error)?
        } else {
            SnapshotContextEngine::new(config)
        };
        if let Some(root) = &self.options.derived_cache_root {
            engine = engine.with_derived_cache_file(workspace_store_path(
                root,
                project_id,
                "context-derived-cache.json",
            ));
        }
        Ok(engine)
    }

    fn engine_for_snapshot(
        &self,
        project_id: &str,
        snapshot_id: &str,
    ) -> Result<SnapshotContextEngine, ReviewHttpRouteError> {
        self.engines
            .lock()
            .expect("context engine store poisoned")
            .get(&context_engine_key(project_id, snapshot_id))
            .cloned()
            .ok_or_else(|| {
                ReviewHttpRouteError::BadRequest(format!(
                    "context index not found for snapshot {snapshot_id}"
                ))
            })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextIndexBody {
    source: ReviewSource,
    #[serde(default)]
    changed_files: Vec<String>,
    #[serde(default)]
    host_metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    cross_repo_contracts: Vec<CrossRepoContractCandidate>,
    #[serde(default)]
    allowed_cross_repo_resources: Vec<String>,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextFeedbackBody {
    #[serde(flatten)]
    index: ContextIndexBody,
    #[serde(default)]
    evidence_ids: Vec<EvidenceId>,
    feedback: String,
    #[serde(default)]
    learning_source: Option<ContextLearningSource>,
    #[serde(default)]
    scope: Option<ContextLearningScope>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextLearningApprovalBody {
    snapshot_id: String,
    #[serde(flatten)]
    approval: ContextLearningApproval,
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
struct ContextFeedbackResponse {
    receipt: ContextFeedbackReceipt,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextLearningApprovalResponse {
    receipt: ContextLearningApprovalReceipt,
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
    serde_json::from_slice(&request.body).map_err(|error| {
        ReviewHttpRouteError::BadRequest(format!("invalid JSON request body: {error}"))
    })
}

fn response_json<T: Serialize>(
    status_code: u16,
    body: &T,
) -> Result<ReviewHttpResponse, ReviewHttpRouteError> {
    Ok(ReviewHttpResponse::json(status_code, body)?)
}

fn build_snapshot_from_source(
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

fn context_engine_key(project_id: &str, snapshot_id: &str) -> String {
    format!("{project_id}:{snapshot_id}")
}

fn workspace_store_path(root: &std::path::Path, project_id: &str, file_name: &str) -> PathBuf {
    let safe_workspace = project_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    root.join(safe_workspace).join(file_name)
}
