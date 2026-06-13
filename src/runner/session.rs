use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::execution::execute_run_start;
use super::protocol::{
    parse_params, stateful_method, write_notification, write_response, JsonRpcError, JsonRpcFrame,
    JsonRpcRequest, JsonRpcResponse,
};
use super::schema::{protocol_schema, runner_check, runner_handshake};
use super::stored::RunnerStoredRun;
use super::transport::{InteractiveTransport, RunnerCallbackTransport, TransportEvent};
use super::types::{
    ArtifactExportParams, ArtifactReadParams, RunCancelResult, RunFailedNotification,
    RunLookupParams, RunStartParams, RunStatusResult, RunnerArtifactExportResult,
    RunnerArtifactReadResult, RunnerContextIndexParams, RunnerContextLearningApprovalParams,
    RunnerHandshakeParams, RunnerSnapshotTextResult, SnapshotReadTextParams, WebhookHandleParams,
    WorkerRunOnceParams, WorkerRunOnceResult,
};
use super::RUNNER_PROTOCOL_VERSION;
use crate::context_engine::{
    ContextEngine, ContextEngineConfig, ContextFeedback, ContextIndexRequest, ContextPackRequest,
    ContextQuery, SnapshotContextEngine,
};
use crate::contracts::{
    ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, SnapshotMode,
};
use crate::review_session::{Muzen, ReviewSessionError, WebhookHeaders};
use crate::runtime::contracts::{SnapshotId, SnapshotStoragePolicy};
use crate::runtime::repo::RepoSnapshot;

pub fn run_stdio<R, W>(reader: &mut R, writer: &mut W) -> Result<i32>
where
    R: BufRead,
    W: Write,
{
    let mut session = RunnerStdioSession::default();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .context("failed to read runner protocol frame")?;
        if bytes == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        session.handle_line(line.trim_end(), writer)?;
    }
    Ok(0)
}

pub fn run_stdio_interactive<R, W>(reader: R, writer: W) -> Result<i32>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let transport = Arc::new(InteractiveTransport::new(reader, writer));
    let mut session = RunnerStdioSession::default();
    loop {
        let event = transport.read_frame()?;
        let Some(event) = event else {
            break;
        };
        match event {
            TransportEvent::Frame(JsonRpcFrame::Request(request)) => {
                if let Some(response) =
                    session.handle_interactive_request(request, transport.clone())?
                {
                    transport.write_response(&response)?;
                }
            }
            TransportEvent::Frame(JsonRpcFrame::Response(response)) => {
                let error = JsonRpcResponse::error(
                    response.id,
                    JsonRpcError::protocol_error("runner received an unexpected JSON-RPC response"),
                );
                transport.write_response(&error)?;
            }
            TransportEvent::Frame(JsonRpcFrame::Notification) => {}
            TransportEvent::ParseError(message) => {
                let error =
                    JsonRpcResponse::error(Some(Value::Null), JsonRpcError::parse_error(message));
                transport.write_response(&error)?;
            }
        }
    }
    // Stdin EOF stops intake but is not a hard kill: in-flight runs finish
    // (or fail, for callback models that can no longer be answered) and emit
    // their terminal frames before the process exits.
    session.drain_active_runs();
    Ok(0)
}

pub fn handle_jsonrpc_line(line: &str) -> JsonRpcResponse {
    match serde_json::from_str::<JsonRpcRequest>(line) {
        Ok(request) => handle_request(request),
        Err(error) => JsonRpcResponse::error(
            None,
            JsonRpcError::parse_error(format!("invalid JSON-RPC request: {error}")),
        ),
    }
}

fn handle_request(request: JsonRpcRequest) -> JsonRpcResponse {
    if request.jsonrpc != "2.0" {
        return JsonRpcResponse::error(
            request.id,
            JsonRpcError::invalid_request("jsonrpc must be 2.0"),
        );
    }
    match request.method.as_str() {
        "runner.handshake" => {
            let params = parse_params::<RunnerHandshakeParams>(request.params);
            match params {
                Ok(params) => {
                    if params.protocol_version != RUNNER_PROTOCOL_VERSION {
                        return JsonRpcResponse::error(
                            request.id,
                            JsonRpcError::protocol_error(format!(
                                "unsupported protocolVersion {}",
                                params.protocol_version
                            )),
                        );
                    }
                    JsonRpcResponse::success(request.id, json!(runner_handshake()))
                }
                Err(error) => JsonRpcResponse::error(request.id, error),
            }
        }
        "runner.check" => JsonRpcResponse::success(request.id, json!(runner_check())),
        "runner.schema.export" => JsonRpcResponse::success(request.id, json!(protocol_schema())),
        "run.start"
        | "run.cancel"
        | "run.status"
        | "run.result"
        | "artifact.read"
        | "artifact.export"
        | "snapshot.readText"
        | "context.index"
        | "context.pack"
        | "context.query"
        | "context.feedback"
        | "context.learning.approve"
        | "webhook.github.handle"
        | "webhook.gitlab.handle"
        | "worker.runOnce" => JsonRpcResponse::error(
            request.id,
            JsonRpcError::not_implemented(format!(
                "{} requires the stateful stdio session in {}",
                request.method, RUNNER_PROTOCOL_VERSION
            )),
        ),
        _ => JsonRpcResponse::error(
            request.id,
            JsonRpcError::method_not_found(format!("unknown method {}", request.method)),
        ),
    }
}

#[derive(Default)]
pub(crate) struct RunnerStdioSession {
    state: Arc<Mutex<RunnerStdioState>>,
    muzen: Muzen,
    run_threads: Vec<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct RunnerStdioState {
    reports: BTreeMap<String, RunnerStoredRun>,
    active_runs: BTreeMap<String, ActiveRun>,
    context_engines: BTreeMap<String, SnapshotContextEngine>,
}

struct ActiveRun {
    cancel: CancellationToken,
}

fn execute_interactive_run_start(
    params: RunStartParams,
    request_id: Option<Value>,
    run_id: String,
    cancel: CancellationToken,
    state: Arc<Mutex<RunnerStdioState>>,
    transport: Arc<dyn RunnerCallbackTransport>,
) -> JsonRpcResponse {
    let response = match execute_run_start(params, Some(transport.clone()), cancel.clone()) {
        Ok(executed) => {
            let result = executed.result.clone();
            state
                .lock()
                .expect("runner state poisoned")
                .reports
                .insert(result.run_id.clone(), executed.stored);
            if cancel.is_cancelled() {
                let failure =
                    RunFailedNotification::from_runner_error(format!("run {run_id} cancelled"));
                let runner_error = JsonRpcError::runner_error(failure.error.clone());
                let _ = transport.notify("run.failed", json!(failure));
                JsonRpcResponse::error(request_id, runner_error)
            } else {
                let _ = transport.notify("run.finished", json!(result.clone()));
                JsonRpcResponse::success(request_id, json!(result))
            }
        }
        Err(error) => {
            let failure = if cancel.is_cancelled() {
                RunFailedNotification::from_runner_error(format!("run {run_id} cancelled"))
            } else {
                RunFailedNotification::from_runner_error(error.to_string())
            };
            let runner_error = JsonRpcError::runner_error(failure.error.clone());
            let _ = transport.notify("run.failed", json!(failure));
            JsonRpcResponse::error(request_id, runner_error)
        }
    };
    state
        .lock()
        .expect("runner state poisoned")
        .active_runs
        .remove(&run_id);
    response
}

impl RunnerStdioSession {
    pub(crate) fn handle_line<W: Write>(&mut self, line: &str, writer: &mut W) -> Result<()> {
        let response = match serde_json::from_str::<JsonRpcRequest>(line) {
            Ok(request) => self.handle_stateful_request(request, writer)?,
            Err(error) => JsonRpcResponse::error(
                None,
                JsonRpcError::parse_error(format!("invalid JSON-RPC request: {error}")),
            ),
        };
        write_response(writer, &response)?;
        Ok(())
    }

    fn handle_stateful_request<W: Write>(
        &mut self,
        request: JsonRpcRequest,
        writer: &mut W,
    ) -> Result<JsonRpcResponse> {
        if !stateful_method(request.method.as_str()) {
            return Ok(handle_request(request));
        }
        if request.jsonrpc != "2.0" {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_request("jsonrpc must be 2.0"),
            ));
        }
        match request.method.as_str() {
            "run.start" => {
                let params = match parse_params::<RunStartParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
                };
                match execute_run_start(params, None, CancellationToken::new()) {
                    Ok(executed) => {
                        for event in &executed.events {
                            write_notification(writer, "event.review", json!(event))?;
                        }
                        let result = executed.result.clone();
                        write_notification(writer, "run.finished", json!(result.clone()))?;
                        self.state
                            .lock()
                            .expect("runner state poisoned")
                            .reports
                            .insert(result.run_id.clone(), executed.stored);
                        Ok(JsonRpcResponse::success(request.id, json!(result)))
                    }
                    Err(error) => {
                        let failure = RunFailedNotification::from_runner_error(error.to_string());
                        let runner_error = JsonRpcError::runner_error(failure.error.clone());
                        write_notification(writer, "run.failed", json!(failure))?;
                        Ok(JsonRpcResponse::error(request.id, runner_error))
                    }
                }
            }
            "run.status" => {
                let params = match parse_params::<RunLookupParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
                };
                let state = self.state.lock().expect("runner state poisoned");
                if state.active_runs.contains_key(&params.run_id) {
                    return Ok(JsonRpcResponse::success(
                        request.id,
                        json!(RunStatusResult {
                            run_id: params.run_id,
                            status: "running".to_string(),
                        }),
                    ));
                }
                let Some(stored) = state.reports.get(&params.run_id) else {
                    return Ok(JsonRpcResponse::error(
                        request.id,
                        JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
                    ));
                };
                Ok(JsonRpcResponse::success(
                    request.id,
                    json!(RunStatusResult {
                        run_id: params.run_id,
                        status: stored.status().to_string(),
                    }),
                ))
            }
            "run.result" => {
                let params = match parse_params::<RunLookupParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
                };
                let state = self.state.lock().expect("runner state poisoned");
                if state.active_runs.contains_key(&params.run_id) {
                    return Ok(JsonRpcResponse::success(
                        request.id,
                        json!(RunStatusResult {
                            run_id: params.run_id,
                            status: "running".to_string(),
                        }),
                    ));
                }
                let Some(stored) = state.reports.get(&params.run_id) else {
                    return Ok(JsonRpcResponse::error(
                        request.id,
                        JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
                    ));
                };
                Ok(JsonRpcResponse::success(
                    request.id,
                    json!(stored.result().clone()),
                ))
            }
            "run.cancel" => self.handle_run_cancel(request),
            "artifact.read" => self.handle_artifact_read(request),
            "artifact.export" => self.handle_artifact_export(request),
            "snapshot.readText" => self.handle_snapshot_read_text(request),
            "context.index" => self.handle_context_index(request),
            "context.pack" => self.handle_context_pack(request),
            "context.query" => self.handle_context_query(request),
            "context.feedback" => self.handle_context_feedback(request),
            "context.learning.approve" => self.handle_context_learning_approval(request),
            "webhook.github.handle" => self.handle_webhook(request, "github"),
            "webhook.gitlab.handle" => self.handle_webhook(request, "gitlab"),
            "worker.runOnce" => self.handle_worker_run_once(request),
            _ => Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::method_not_found(format!("unknown method {}", request.method)),
            )),
        }
    }

    fn handle_interactive_request<T>(
        &mut self,
        request: JsonRpcRequest,
        transport: Arc<T>,
    ) -> Result<Option<JsonRpcResponse>>
    where
        T: RunnerCallbackTransport + 'static,
    {
        if !stateful_method(request.method.as_str()) {
            return Ok(Some(handle_request(request)));
        }
        if request.jsonrpc != "2.0" {
            return Ok(Some(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_request("jsonrpc must be 2.0"),
            )));
        }
        if request.method.as_str() != "run.start" {
            return self
                .handle_stateful_request_without_notifications(request)
                .map(Some);
        }
        let params = match parse_params::<RunStartParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(Some(JsonRpcResponse::error(request.id, error))),
        };
        let run_id = params
            .run_id
            .clone()
            .unwrap_or_else(|| "muzen-run".to_string());
        let cancel = CancellationToken::new();
        {
            let mut state = self.state.lock().expect("runner state poisoned");
            if state.active_runs.contains_key(&run_id) {
                return Ok(Some(JsonRpcResponse::error(
                    request.id,
                    JsonRpcError::invalid_params(format!("runId {run_id} is already active")),
                )));
            }
            state.active_runs.insert(
                run_id.clone(),
                ActiveRun {
                    cancel: cancel.clone(),
                },
            );
        }
        let state = Arc::clone(&self.state);
        let transport: Arc<dyn RunnerCallbackTransport> = transport;
        self.run_threads.push(std::thread::spawn(move || {
            let response = execute_interactive_run_start(
                params,
                request.id,
                run_id.clone(),
                cancel,
                Arc::clone(&state),
                Arc::clone(&transport),
            );
            let _ = transport.respond(&response);
        }));
        Ok(None)
    }

    /// Joins every run.start worker thread so their final responses and
    /// run.finished/run.failed notifications reach the writer before exit.
    pub(crate) fn drain_active_runs(&mut self) {
        for handle in self.run_threads.drain(..) {
            let _ = handle.join();
        }
    }

    fn handle_stateful_request_without_notifications(
        &mut self,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse> {
        match request.method.as_str() {
            "run.status" => {
                let params = match parse_params::<RunLookupParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
                };
                let state = self.state.lock().expect("runner state poisoned");
                if state.active_runs.contains_key(&params.run_id) {
                    return Ok(JsonRpcResponse::success(
                        request.id,
                        json!(RunStatusResult {
                            run_id: params.run_id,
                            status: "running".to_string(),
                        }),
                    ));
                }
                let Some(stored) = state.reports.get(&params.run_id) else {
                    return Ok(JsonRpcResponse::error(
                        request.id,
                        JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
                    ));
                };
                Ok(JsonRpcResponse::success(
                    request.id,
                    json!(RunStatusResult {
                        run_id: params.run_id,
                        status: stored.status().to_string(),
                    }),
                ))
            }
            "run.result" => {
                let params = match parse_params::<RunLookupParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
                };
                let state = self.state.lock().expect("runner state poisoned");
                if state.active_runs.contains_key(&params.run_id) {
                    return Ok(JsonRpcResponse::error(
                        request.id,
                        JsonRpcError::invalid_params(format!(
                            "runId {} is still active",
                            params.run_id
                        )),
                    ));
                }
                let Some(stored) = state.reports.get(&params.run_id) else {
                    return Ok(JsonRpcResponse::error(
                        request.id,
                        JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
                    ));
                };
                Ok(JsonRpcResponse::success(
                    request.id,
                    json!(stored.result().clone()),
                ))
            }
            "run.cancel" => self.handle_run_cancel(request),
            "artifact.read" => self.handle_artifact_read(request),
            "artifact.export" => self.handle_artifact_export(request),
            "snapshot.readText" => self.handle_snapshot_read_text(request),
            "context.index" => self.handle_context_index(request),
            "context.pack" => self.handle_context_pack(request),
            "context.query" => self.handle_context_query(request),
            "context.feedback" => self.handle_context_feedback(request),
            "context.learning.approve" => self.handle_context_learning_approval(request),
            "webhook.github.handle" => self.handle_webhook(request, "github"),
            "webhook.gitlab.handle" => self.handle_webhook(request, "gitlab"),
            "worker.runOnce" => self.handle_worker_run_once(request),
            _ => Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::method_not_found(format!("unknown method {}", request.method)),
            )),
        }
    }

    fn handle_run_cancel(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let params = match parse_params::<RunLookupParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let state = self.state.lock().expect("runner state poisoned");
        if let Some(active) = state.active_runs.get(&params.run_id) {
            active.cancel.cancel();
            return Ok(JsonRpcResponse::success(
                request.id,
                json!(RunCancelResult {
                    run_id: params.run_id,
                    status: "cancelling".to_string(),
                    cancelled: true,
                    reason: "cancel requested".to_string(),
                }),
            ));
        }
        let Some(stored) = state.reports.get(&params.run_id) else {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_params(format!("unknown runId {}", params.run_id)),
            ));
        };
        Ok(JsonRpcResponse::success(
            request.id,
            json!(RunCancelResult {
                run_id: params.run_id,
                status: stored.status().to_string(),
                cancelled: false,
                reason: "run already reached a terminal state".to_string(),
            }),
        ))
    }

    fn handle_artifact_read(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_artifact_export(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_snapshot_read_text(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_context_index(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_context_pack(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_context_query(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_context_feedback(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_context_learning_approval(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_webhook(&self, request: JsonRpcRequest, provider: &str) -> Result<JsonRpcResponse> {
        let params = match parse_params::<WebhookHandleParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let workspace = self.muzen.workspace(
            params
                .workspace_id
                .as_deref()
                .filter(|workspace_id| !workspace_id.trim().is_empty())
                .unwrap_or("default"),
        );
        let headers = params.headers.into_iter().collect::<WebhookHeaders>();
        let delivery = match provider {
            "github" => block_on_muzen(workspace.handle_github_webhook(
                &headers,
                params.body.as_bytes(),
                params.secret.as_deref(),
                params.options,
            )),
            "gitlab" => block_on_muzen(workspace.handle_gitlab_webhook(
                &headers,
                params.body.as_bytes(),
                params.secret.as_deref(),
                params.options,
            )),
            _ => unreachable!("unsupported webhook provider"),
        };
        let response = match delivery {
            Ok(delivery) => delivery
                .http_response()
                .map_err(|error| JsonRpcError::invalid_params(error.to_string())),
            Err(error) => Err(error),
        };
        match response {
            Ok(response) => Ok(JsonRpcResponse::success(request.id, json!(response))),
            Err(error) => Ok(JsonRpcResponse::error(request.id, error)),
        }
    }

    fn handle_worker_run_once(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let params = match parse_params::<WorkerRunOnceParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let worker_id = params.worker_id().to_string();
        let worker = self.muzen.worker(worker_id.clone(), params.host_config);
        match block_on_muzen(worker.run_once(params.max_sessions)) {
            Ok(run) => Ok(JsonRpcResponse::success(
                request.id,
                json!(WorkerRunOnceResult::from_run(worker_id, run)),
            )),
            Err(error) => Ok(JsonRpcResponse::error(request.id, error)),
        }
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
    RepoSnapshot::build_with_storage(
        repo,
        &PathPolicyV1::bench(200, 120),
        &change,
        SnapshotStoragePolicy::default(),
    )
    .map_err(|error| JsonRpcError::runner_error(error.to_string()))
}

fn block_on_muzen<T>(
    future: impl std::future::Future<Output = Result<T, ReviewSessionError>>,
) -> Result<T, JsonRpcError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JsonRpcError::runner_error(error.to_string()))?
        .block_on(future)
        .map_err(|error| JsonRpcError::runner_error(error.to_string()))
}

fn block_on_context<T>(
    future: impl std::future::Future<Output = crate::runtime::contracts::RuntimeResult<T>>,
) -> Result<T, JsonRpcError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| JsonRpcError::runner_error(error.to_string()))?
        .block_on(future)
        .map_err(|error| JsonRpcError::runner_error(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::Condvar;
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::*;

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("shared writer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn malformed_line_yields_parse_error_and_keeps_session_alive() {
        let handshake = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "runner.handshake",
            "params": { "protocolVersion": RUNNER_PROTOCOL_VERSION }
        }))
        .expect("handshake request");
        let input = format!("{{\"this is not json\n{handshake}\n");
        let writer = SharedWriter::default();
        let code = run_stdio_interactive(std::io::Cursor::new(input.into_bytes()), writer.clone())
            .expect("stdio session survives malformed line");
        assert_eq!(code, 0);
        let output = writer.0.lock().expect("shared writer poisoned").clone();
        let frames = String::from_utf8(output)
            .expect("utf8 output")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("response frame"))
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 2, "parse error response plus handshake");
        assert_eq!(frames[0]["error"]["code"], -32700);
        assert!(
            frames[0]
                .as_object()
                .expect("response object")
                .contains_key("id"),
            "parse error response carries id null"
        );
        assert_eq!(frames[0]["id"], Value::Null);
        assert_eq!(frames[1]["id"], 1);
        assert!(frames[1]["result"].is_object(), "handshake still answered");
    }

    #[test]
    fn stdin_eof_drains_in_flight_run_before_exit() {
        let repo = tempfile::tempdir().expect("temp repo");
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
        )
        .expect("fixture file");
        let mut session = RunnerStdioSession::default();
        let transport = Arc::new(RecordingTransport::default());

        let start = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "run.start".to_string(),
            params: Some(json!({
                "protocolVersion": RUNNER_PROTOCOL_VERSION,
                "runId": "drain-me",
                "repo": repo.path(),
                "changedFiles": ["Cargo.toml"],
                "model": { "callback": true },
                "sessions": [{
                    "id": "generalist",
                    "role": "generalist",
                    "objective": "Review drain behavior."
                }]
            })),
        };
        let immediate = session
            .handle_interactive_request(start, transport.clone())
            .expect("start request");
        assert!(immediate.is_none());
        transport.wait_for_model_request();
        transport.release_model_request();

        session.drain_active_runs();

        let state = transport.state.lock().expect("transport state poisoned");
        assert_eq!(
            state.responses.len(),
            1,
            "terminal response written before exit"
        );
        assert_eq!(state.responses[0].id, Some(json!(1)));
        assert!(
            state
                .notifications
                .iter()
                .any(|(method, _)| method == "run.failed" || method == "run.finished"),
            "terminal run notification written before exit"
        );
    }

    #[derive(Default)]
    struct RecordingTransport {
        state: Mutex<RecordingTransportState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct RecordingTransportState {
        model_request_started: bool,
        release_model_request: bool,
        notifications: Vec<(String, Value)>,
        responses: Vec<JsonRpcResponse>,
    }

    impl RecordingTransport {
        fn wait_for_model_request(&self) {
            let mut state = self.state.lock().expect("transport state poisoned");
            let deadline = Instant::now() + Duration::from_secs(3);
            while !state.model_request_started {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    panic!("timed out waiting for model callback request");
                };
                let (next_state, timeout) = self
                    .changed
                    .wait_timeout(state, remaining)
                    .expect("transport condvar poisoned");
                state = next_state;
                assert!(
                    !timeout.timed_out(),
                    "timed out waiting for model callback request"
                );
            }
        }

        fn release_model_request(&self) {
            let mut state = self.state.lock().expect("transport state poisoned");
            state.release_model_request = true;
            self.changed.notify_all();
        }

        fn wait_for_response(&self) -> JsonRpcResponse {
            let mut state = self.state.lock().expect("transport state poisoned");
            let deadline = Instant::now() + Duration::from_secs(3);
            while state.responses.is_empty() {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    panic!("timed out waiting for runner response");
                };
                let (next_state, timeout) = self
                    .changed
                    .wait_timeout(state, remaining)
                    .expect("transport condvar poisoned");
                state = next_state;
                assert!(
                    !timeout.timed_out(),
                    "timed out waiting for runner response"
                );
            }
            state.responses.remove(0)
        }

        fn notifications(&self) -> Vec<(String, Value)> {
            self.state
                .lock()
                .expect("transport state poisoned")
                .notifications
                .clone()
        }
    }

    impl RunnerCallbackTransport for RecordingTransport {
        fn request(&self, method: &str, _params: Value) -> Result<Value> {
            assert_eq!(method, "model.complete");
            let mut state = self.state.lock().expect("transport state poisoned");
            state.model_request_started = true;
            self.changed.notify_all();
            while !state.release_model_request {
                state = self
                    .changed
                    .wait(state)
                    .expect("transport condvar poisoned");
            }
            anyhow::bail!("operation aborted")
        }

        fn notify(&self, method: &str, params: Value) -> Result<()> {
            let mut state = self.state.lock().expect("transport state poisoned");
            state.notifications.push((method.to_string(), params));
            self.changed.notify_all();
            Ok(())
        }

        fn respond(&self, response: &JsonRpcResponse) -> Result<()> {
            let mut state = self.state.lock().expect("transport state poisoned");
            state.responses.push(response.clone());
            self.changed.notify_all();
            Ok(())
        }
    }

    #[test]
    fn interactive_run_cancel_preempts_active_run_start() {
        let repo = tempfile::tempdir().expect("temp repo");
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
        )
        .expect("fixture file");
        let mut session = RunnerStdioSession::default();
        let transport = Arc::new(RecordingTransport::default());

        let start = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "run.start".to_string(),
            params: Some(json!({
                "protocolVersion": RUNNER_PROTOCOL_VERSION,
                "runId": "cancel-me",
                "repo": repo.path(),
                "changedFiles": ["Cargo.toml"],
                "model": { "callback": true },
                "sessions": [{
                    "id": "generalist",
                    "role": "generalist",
                    "objective": "Review cancellation behavior."
                }]
            })),
        };
        let immediate = session
            .handle_interactive_request(start, transport.clone())
            .expect("start request");
        assert!(immediate.is_none());
        transport.wait_for_model_request();

        let status = session
            .handle_interactive_request(run_lookup_request(2, "run.status"), transport.clone())
            .expect("status request")
            .expect("status response");
        assert_eq!(status.result.as_ref().unwrap()["status"], "running");

        let cancel = session
            .handle_interactive_request(run_lookup_request(3, "run.cancel"), transport.clone())
            .expect("cancel request")
            .expect("cancel response");
        assert_eq!(cancel.result.as_ref().unwrap()["status"], "cancelling");
        assert_eq!(cancel.result.as_ref().unwrap()["cancelled"], true);

        transport.release_model_request();
        let start_response = transport.wait_for_response();
        assert!(start_response.error.is_some());
        assert_eq!(start_response.id, Some(json!(1)));
        assert!(start_response
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("cancelled"));

        let notifications = transport.notifications();
        let failed = notifications
            .iter()
            .find(|(method, _)| method == "run.failed")
            .expect("run.failed notification");
        assert_eq!(failed.1["failureKind"], "cancelled");
        assert_eq!(failed.1["retryHint"], "not_retryable");

        let result = session
            .handle_interactive_request(run_lookup_request(4, "run.result"), transport)
            .expect("result request")
            .expect("result response");
        assert!(result.result.is_some(), "partial result remains stored");
    }

    fn run_lookup_request(id: u64, method: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(id)),
            method: method.to_string(),
            params: Some(json!({ "runId": "cancel-me" })),
        }
    }
}
