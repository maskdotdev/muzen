use anyhow::Result;
use serde_json::json;

use super::protocol::{parse_params, JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use super::results::{
    RunnerArtifactExportResult, RunnerArtifactReadResult, RunnerSnapshotTextResult,
};
use super::session::RunnerStdioSession;
use super::types::{ArtifactExportParams, ArtifactReadParams, SnapshotReadTextParams};

impl RunnerStdioSession {
    pub(super) fn handle_artifact_read(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let params = match parse_params::<ArtifactReadParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let state = self.state.lock().expect("runner state poisoned");
        let Some(stored) = state.reports.get(&params.run_id) else {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
            ));
        };
        let Some(artifact) = stored.artifact(params.view, &params.artifact_id) else {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_params(format!("unknown artifactId {}", params.artifact_id)),
            ));
        };
        Ok(JsonRpcResponse::success(
            request.id,
            json!(RunnerArtifactReadResult {
                run_id: params.run_id,
                view: params.view,
                artifact: artifact.clone(),
            }),
        ))
    }

    pub(super) fn handle_artifact_export(
        &self,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse> {
        let params = match parse_params::<ArtifactExportParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let state = self.state.lock().expect("runner state poisoned");
        let Some(stored) = state.reports.get(&params.run_id) else {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
            ));
        };
        let mut artifacts = stored.artifacts(params.view).to_vec();
        if !params.artifact_ids.is_empty() {
            artifacts.retain(|artifact| {
                params
                    .artifact_ids
                    .iter()
                    .any(|artifact_id| artifact_id == &artifact.artifact_id)
            });
        }
        let total_bytes = artifacts
            .iter()
            .map(|artifact| artifact.bytes)
            .sum::<usize>();
        if params
            .max_artifacts
            .is_some_and(|max_artifacts| artifacts.len() > max_artifacts)
        {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::limit_exceeded("artifact_retention_artifacts"),
            ));
        }
        if params
            .max_bytes
            .is_some_and(|max_bytes| total_bytes > max_bytes)
        {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::limit_exceeded("artifact_retention_bytes"),
            ));
        }
        Ok(JsonRpcResponse::success(
            request.id,
            json!(RunnerArtifactExportResult {
                run_id: params.run_id,
                view: params.view,
                artifact_count: artifacts.len(),
                total_bytes,
                artifacts,
            }),
        ))
    }

    pub(super) fn handle_snapshot_read_text(
        &self,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse> {
        let params = match parse_params::<SnapshotReadTextParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let state = self.state.lock().expect("runner state poisoned");
        let Some(stored) = state.reports.get(&params.run_id) else {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
            ));
        };
        let reader = match stored.snapshot_reader(params.snapshot_id.as_deref()) {
            Ok(reader) => reader,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let max_bytes = params.max_bytes.unwrap_or(200 * 1024);
        match reader.read_text_path(&params.path, max_bytes) {
            Ok(file) => Ok(JsonRpcResponse::success(
                request.id,
                json!(RunnerSnapshotTextResult {
                    run_id: params.run_id,
                    snapshot_id: file.snapshot_id.0,
                    path: file.path.display(),
                    content_hash: file.content_hash,
                    bytes: file.bytes,
                    truncated: file.truncated,
                    content: file.content,
                }),
            )),
            Err(error) => Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::runner_error(error.to_string()),
            )),
        }
    }
}
