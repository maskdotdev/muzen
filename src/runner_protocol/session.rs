use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use anyhow::Context;
use anyhow::Result;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::execution::execute_run_start;
use super::failures::RunFailedNotification;
use super::protocol::{
    parse_params, stateful_method, JsonRpcError, JsonRpcFrame, JsonRpcRequest, JsonRpcResponse,
};
#[cfg(test)]
use super::protocol::{write_notification, write_response};
use super::schema::{protocol_schema, runner_check, runner_handshake};
use super::stored::RunnerStoredRun;
use super::transport::{InteractiveTransport, RunnerCallbackTransport, TransportEvent};
use super::types::{
    RunCancelResult, RunLookupParams, RunStartParams, RunStatusResult, RunnerHandshakeParams,
    WebhookHandleParams, WorkerRunOnceParams, WorkerRunOnceResult,
};
use super::RUNNER_PROTOCOL_VERSION;
use crate::context_engine::SnapshotContextEngine;
use crate::review_sessions::{Muzen, ReviewSessionError, WebhookHeaders};

#[cfg(test)]
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

#[cfg(test)]
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
    pub(super) state: Arc<Mutex<RunnerStdioState>>,
    muzen: Muzen,
    run_threads: Vec<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
pub(super) struct RunnerStdioState {
    pub(super) reports: BTreeMap<String, RunnerStoredRun>,
    active_runs: BTreeMap<String, ActiveRun>,
    pub(super) context_engines: BTreeMap<String, SnapshotContextEngine>,
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
    #[cfg(test)]
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

    #[cfg(test)]
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
            "run.status" => self.handle_run_status(request),
            "run.result" => self.handle_run_result(request),
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
            "run.status" => self.handle_run_status(request),
            "run.result" => self.handle_run_result(request),
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

    fn handle_run_status(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_run_result(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let params = match parse_params::<RunLookupParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let state = self.state.lock().expect("runner state poisoned");
        if state.active_runs.contains_key(&params.run_id) {
            return Ok(JsonRpcResponse::error(
                request.id,
                JsonRpcError::invalid_params(format!("runId {} is still active", params.run_id)),
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

    fn handle_run_cancel(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
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

    fn handle_webhook(&self, request: JsonRpcRequest, provider: &str) -> Result<JsonRpcResponse> {
        let params = match parse_params::<WebhookHandleParams>(request.params) {
            Ok(params) => params,
            Err(error) => return Ok(JsonRpcResponse::error(request.id, error)),
        };
        let workspace = self.muzen.project(
            params
                .project_id
                .as_deref()
                .filter(|project_id| !project_id.trim().is_empty())
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

#[cfg(test)]
mod tests;
