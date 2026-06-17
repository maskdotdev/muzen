use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::protocol::{parse_params, JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use super::session::RunnerStdioSession;
use super::types::{RunnerContextIndexParams, RunnerContextLearningApprovalParams};
use crate::context_engine::{
    ContextEngine, ContextEngineConfig, ContextFeedback, ContextIndexRequest, ContextPackRequest,
    ContextQuery, SnapshotContextEngine,
};
use crate::reviewer_kernel::kernel_types::{SnapshotCapturePolicy, SnapshotId};
use crate::reviewer_kernel::review_contract::{
    ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, SnapshotMode,
};
use crate::workspace::RepoSnapshot;

impl RunnerStdioSession {
    pub(super) fn handle_context_index(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let params = match parse_params::<RunnerContextIndexParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let snapshot = match build_context_snapshot(&params.repo, &params.changed_files) {
            Ok(snapshot) => snapshot,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let config = params
            .config
            .unwrap_or_else(ContextEngineConfig::snapshot_v0);
        let engine = SnapshotContextEngine::new(config);
        let mut request_params =
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref());
        request_params.host_metadata = params.host_metadata;
        request_params.cross_repo_contracts = params.cross_repo_contracts;
        request_params.allowed_cross_repo_resources =
            params.allowed_cross_repo_resources.into_iter().collect();
        let report =
            match block_on_context(engine.index_snapshot(request_params, CancellationToken::new()))
            {
                Ok(report) => report,
                Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
            };
        let Some(index) = engine.get_index(&snapshot.snapshot_id) else {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::runner_error("context index was not stored"),
            ));
        };
        self.state
            .lock()
            .expect("runner state poisoned")
            .context_engines
            .insert(report.snapshot_id.0.clone(), engine);
        Ok(JsonRpcResponse::success(
            request.id,
            json!(index.manifest_artifact.clone()),
        ))
    }

    pub(super) fn handle_context_pack(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let params = match parse_params::<ContextPackRequest>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let engine = match self.context_engine_for_snapshot(&params.snapshot_id) {
            Ok(engine) => engine,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        match block_on_context(engine.build_pack(params, CancellationToken::new())) {
            Ok(pack) => Ok(JsonRpcResponse::success(request.id, json!(pack))),
            Err(error) => Ok(JsonRpcResponse::error(request.id, error)),
        }
    }

    pub(super) fn handle_context_query(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let params = match parse_params::<ContextQuery>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let engine = match self.context_engine_for_snapshot(&params.snapshot_id) {
            Ok(engine) => engine,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        match block_on_context(engine.query(params, CancellationToken::new())) {
            Ok(result) => Ok(JsonRpcResponse::success(request.id, json!(result))),
            Err(error) => Ok(JsonRpcResponse::error(request.id, error)),
        }
    }

    pub(super) fn handle_context_feedback(
        &self,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse> {
        let params = match parse_params::<ContextFeedback>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let engine = match self.context_engine_for_snapshot(&params.snapshot_id) {
            Ok(engine) => engine,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        match block_on_context(engine.record_feedback(params, CancellationToken::new())) {
            Ok(receipt) => Ok(JsonRpcResponse::success(request.id, json!(receipt))),
            Err(error) => Ok(JsonRpcResponse::error(request.id, error)),
        }
    }

    pub(super) fn handle_context_learning_approval(
        &self,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse> {
        let params = match parse_params::<RunnerContextLearningApprovalParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let engine = match self.context_engine_for_snapshot(&params.snapshot_id) {
            Ok(engine) => engine,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        match block_on_context(engine.approve_learning(params.approval, CancellationToken::new())) {
            Ok(receipt) => Ok(JsonRpcResponse::success(request.id, json!(receipt))),
            Err(error) => Ok(JsonRpcResponse::error(request.id, error)),
        }
    }

    fn context_engine_for_snapshot(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Result<SnapshotContextEngine, JsonRpcError> {
        self.state
            .lock()
            .expect("runner state poisoned")
            .context_engines
            .get(&snapshot_id.0)
            .cloned()
            .ok_or_else(|| {
                JsonRpcError::invalid_params(format!(
                    "context index not found for snapshot {}",
                    snapshot_id.0
                ))
            })
    }
}

fn build_context_snapshot(
    repo: &Path,
    changed_files: &[String],
) -> Result<Arc<RepoSnapshot>, JsonRpcError> {
    if changed_files.is_empty() {
        return Err(JsonRpcError::invalid_params(
            "context.index requires at least one changed file",
        ));
    }
    let changed_files = changed_files
        .iter()
        .map(|path| ChangedFileEntryV1 {
            status: ChangedFileStatus::Modified,
            old_path: Some(PathBuf::from(path)),
            new_path: Some(PathBuf::from(path)),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        })
        .collect::<Vec<_>>();
    let change = ChangeScopeV1 {
        kind: ChangeKind::LocalDiff,
        change_id: "context-runner".to_string(),
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
    RepoSnapshot::build_with_capture_policy(
        repo,
        &PathPolicyV1::bench(200, 120),
        &change,
        SnapshotCapturePolicy::default(),
    )
    .map_err(|error| JsonRpcError::runner_error(error.to_string()))
}

fn block_on_context<T>(
    future: impl std::future::Future<Output = crate::reviewer_kernel::kernel_types::RuntimeResult<T>>,
) -> Result<T, JsonRpcError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JsonRpcError::runner_error(error.to_string()))?
        .block_on(future)
        .map_err(|error| JsonRpcError::runner_error(error.to_string()))
}
