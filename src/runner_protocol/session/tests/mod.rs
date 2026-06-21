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
    assert_eq!(result.result.as_ref().unwrap()["status"], "cancelled");
}

fn run_lookup_request(id: u64, method: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(id)),
        method: method.to_string(),
        params: Some(json!({ "runId": "cancel-me" })),
    }
}

#[test]
fn overlapping_interactive_run_starts_keep_callbacks_reports_and_artifacts_isolated() {
    let repo = tempfile::tempdir().expect("temp repo");
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .expect("fixture file");
    let mut session = RunnerStdioSession::default();
    let transport = Arc::new(IsolationTransport::default());

    let first = session
        .handle_interactive_request(
            run_start_request(11, "run-a", repo.path().to_str().expect("repo path")),
            transport.clone(),
        )
        .expect("first run.start");
    let second = session
        .handle_interactive_request(
            run_start_request(12, "run-b", repo.path().to_str().expect("repo path")),
            transport.clone(),
        )
        .expect("second run.start");
    assert!(first.is_none());
    assert!(second.is_none());

    transport.wait_for_first_model_requests(["run-a", "run-b"]);
    let status_a = session
        .handle_interactive_request(
            run_lookup_request_for(21, "run.status", "run-a"),
            transport.clone(),
        )
        .expect("run-a status")
        .expect("run-a status response");
    let status_b = session
        .handle_interactive_request(
            run_lookup_request_for(22, "run.status", "run-b"),
            transport.clone(),
        )
        .expect("run-b status")
        .expect("run-b status response");
    assert_eq!(status_a.result.as_ref().unwrap()["status"], "running");
    assert_eq!(status_b.result.as_ref().unwrap()["status"], "running");

    transport.release_run("run-b");
    let response_b = transport.wait_for_response_id(12);
    assert!(
        response_b.error.is_none(),
        "run-b should finish successfully"
    );
    assert_eq!(response_b.result.as_ref().unwrap()["runId"], "run-b");

    let early_result_a = session
        .handle_interactive_request(
            run_lookup_request_for(23, "run.result", "run-a"),
            transport.clone(),
        )
        .expect("run-a early result")
        .expect("run-a early result response");
    assert!(
        early_result_a
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("still active"),
        "run-a remains active while run-b has finished"
    );

    transport.release_run("run-a");
    let response_a = transport.wait_for_response_id(11);
    assert!(
        response_a.error.is_none(),
        "run-a should finish successfully"
    );
    assert_eq!(response_a.result.as_ref().unwrap()["runId"], "run-a");
    session.drain_active_runs();

    let requests = transport.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "model.complete" && request.run_id == "run-a")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "model.complete" && request.run_id == "run-b")
            .count(),
        2
    );
    assert!(requests.iter().any(|request| {
        request.method == "tool.execute"
            && request.run_id == "run-a"
            && request.session_id == "review-orchestrator"
            && request.call_id == "run-a-probe"
    }));
    assert!(requests.iter().any(|request| {
        request.method == "tool.execute"
            && request.run_id == "run-b"
            && request.session_id == "review-orchestrator"
            && request.call_id == "run-b-probe"
    }));

    let notifications = transport.notifications();
    let finished_run_ids = notifications
        .iter()
        .filter(|(method, _)| method == "run.finished")
        .map(|(_, params)| params["runId"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(finished_run_ids, vec!["run-b", "run-a"]);
    for (method, params) in notifications
        .iter()
        .filter(|(method, _)| method == "event.runtime")
    {
        let run_id = params["context"]["runId"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} notification missing context.runId: {params}"));
        assert!(
            run_id == "run-a" || run_id == "run-b",
            "event.runtime leaked unexpected runId {run_id}"
        );
    }
    for (method, params) in notifications
        .iter()
        .filter(|(method, _)| method == "event.review")
    {
        let run_id = params["runId"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} notification missing runId: {params}"));
        assert!(
            run_id == "run-a" || run_id == "run-b",
            "event.review leaked unexpected runId {run_id}"
        );
    }

    let result_a = session
        .handle_interactive_request(
            run_lookup_request_for(24, "run.result", "run-a"),
            transport.clone(),
        )
        .expect("run-a result")
        .expect("run-a result response");
    let result_b = session
        .handle_interactive_request(
            run_lookup_request_for(25, "run.result", "run-b"),
            transport.clone(),
        )
        .expect("run-b result")
        .expect("run-b result response");
    assert_eq!(result_a.result.as_ref().unwrap()["runId"], "run-a");
    assert_eq!(result_b.result.as_ref().unwrap()["runId"], "run-b");

    let artifact_a = exported_artifact_id(&mut session, transport.clone(), 31, "run-a");
    let artifact_b = exported_artifact_id(&mut session, transport.clone(), 32, "run-b");
    assert_ne!(
        artifact_a, artifact_b,
        "run-specific callback artifacts should not collide"
    );
    let read_a = read_artifact(&mut session, transport.clone(), 33, "run-a", &artifact_a);
    let read_b = read_artifact(&mut session, transport.clone(), 34, "run-b", &artifact_b);
    assert_eq!(
        read_a.result.as_ref().unwrap()["artifact"]["content"],
        "artifact owned by run-a"
    );
    assert_eq!(
        read_b.result.as_ref().unwrap()["artifact"]["content"],
        "artifact owned by run-b"
    );
    let cross_read = read_artifact(&mut session, transport, 35, "run-a", &artifact_b);
    assert!(
        cross_read
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("unknown artifactId"),
        "run-a must not read run-b's artifact by id"
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CallbackRequestRecord {
    method: String,
    run_id: String,
    session_id: String,
    turn: Option<u64>,
    call_id: String,
}

#[derive(Default)]
struct IsolationTransport {
    state: Mutex<IsolationTransportState>,
    changed: Condvar,
}

#[derive(Default)]
struct IsolationTransportState {
    requests: Vec<CallbackRequestRecord>,
    released_runs: std::collections::BTreeMap<String, bool>,
    notifications: Vec<(String, Value)>,
    responses: Vec<JsonRpcResponse>,
}

impl IsolationTransport {
    fn wait_for_first_model_requests(&self, run_ids: [&str; 2]) {
        let mut state = self.state.lock().expect("isolation transport poisoned");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !run_ids.iter().all(|run_id| {
            state.requests.iter().any(|request| {
                request.method == "model.complete"
                    && request.run_id == *run_id
                    && request.turn == Some(0)
            })
        }) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                panic!("timed out waiting for both model callbacks");
            };
            let (next_state, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("isolation transport condvar poisoned");
            state = next_state;
            assert!(
                !timeout.timed_out(),
                "timed out waiting for both model callbacks"
            );
        }
    }

    fn release_run(&self, run_id: &str) {
        let mut state = self.state.lock().expect("isolation transport poisoned");
        state.released_runs.insert(run_id.to_string(), true);
        self.changed.notify_all();
    }

    fn wait_for_response_id(&self, id: u64) -> JsonRpcResponse {
        let mut state = self.state.lock().expect("isolation transport poisoned");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(index) = state
                .responses
                .iter()
                .position(|response| response.id == Some(json!(id)))
            {
                return state.responses.remove(index);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                panic!("timed out waiting for response id {id}");
            };
            let (next_state, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("isolation transport condvar poisoned");
            state = next_state;
            assert!(
                !timeout.timed_out(),
                "timed out waiting for response id {id}"
            );
        }
    }

    fn requests(&self) -> Vec<CallbackRequestRecord> {
        self.state
            .lock()
            .expect("isolation transport poisoned")
            .requests
            .clone()
    }

    fn notifications(&self) -> Vec<(String, Value)> {
        self.state
            .lock()
            .expect("isolation transport poisoned")
            .notifications
            .clone()
    }
}

impl RunnerCallbackTransport for IsolationTransport {
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let run_id = params["runId"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} callback missing runId: {params}"))
            .to_string();
        let record = CallbackRequestRecord {
            method: method.to_string(),
            run_id: run_id.clone(),
            session_id: params["sessionId"].as_str().unwrap_or_default().to_string(),
            turn: params["turn"].as_u64(),
            call_id: params["callId"].as_str().unwrap_or_default().to_string(),
        };
        let mut state = self.state.lock().expect("isolation transport poisoned");
        state.requests.push(record);
        self.changed.notify_all();
        while !state
            .released_runs
            .get(&run_id)
            .copied()
            .unwrap_or_default()
        {
            state = self
                .changed
                .wait(state)
                .expect("isolation transport condvar poisoned");
        }
        drop(state);

        match method {
            "model.complete" => {
                if params["turn"].as_u64() == Some(0) {
                    Ok(json!({
                        "toolCalls": [{
                            "callId": format!("{run_id}-probe"),
                            "toolId": "ownership_probe",
                            "arguments": { "runId": run_id }
                        }],
                        "usage": { "inputTokens": 1, "outputTokens": 1, "totalTokens": 2 }
                    }))
                } else {
                    Ok(json!({
                        "content": format!(
                            "{{\"verdict\":\"clean\",\"summary\":\"{run_id} complete\",\"candidates\":[],\"notes\":[],\"completeness\":{{}}}}"
                        ),
                        "usage": { "inputTokens": 1, "outputTokens": 1, "totalTokens": 2 }
                    }))
                }
            }
            "tool.execute" => Ok(json!({
                "data": { "seenRunId": run_id },
                "artifact": {
                    "key": format!("{run_id}-ownership-artifact"),
                    "content": format!("artifact owned by {run_id}")
                }
            })),
            other => panic!("unexpected callback method {other}"),
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut state = self.state.lock().expect("isolation transport poisoned");
        state.notifications.push((method.to_string(), params));
        self.changed.notify_all();
        Ok(())
    }

    fn respond(&self, response: &JsonRpcResponse) -> Result<()> {
        let mut state = self.state.lock().expect("isolation transport poisoned");
        state.responses.push(response.clone());
        self.changed.notify_all();
        Ok(())
    }
}

fn run_start_request(id: u64, run_id: &str, repo: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(id)),
        method: "run.start".to_string(),
        params: Some(json!({
            "protocolVersion": RUNNER_PROTOCOL_VERSION,
            "runId": run_id,
            "repo": repo,
            "changedFiles": ["Cargo.toml"],
            "model": { "callback": true },
            "tools": [{
                "id": "ownership_probe",
                "description": "Records ownership labels for isolation tests.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["runId"],
                    "properties": {
                        "runId": { "type": "string" }
                    }
                },
                "effects": ["write_artifact"]
            }]
        })),
    }
}

fn run_lookup_request_for(id: u64, method: &str, run_id: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(id)),
        method: method.to_string(),
        params: Some(json!({ "runId": run_id })),
    }
}

fn exported_artifact_id(
    session: &mut RunnerStdioSession,
    transport: Arc<IsolationTransport>,
    id: u64,
    run_id: &str,
) -> String {
    let response = session
        .handle_interactive_request(
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(id)),
                method: "artifact.export".to_string(),
                params: Some(json!({ "runId": run_id })),
            },
            transport,
        )
        .expect("artifact export")
        .expect("artifact export response");
    let artifacts = response.result.as_ref().unwrap()["artifacts"]
        .as_array()
        .expect("artifact array");
    let artifact = artifacts
        .iter()
        .find(|artifact| {
            artifact["content"]
                .as_str()
                .is_some_and(|content| content == format!("artifact owned by {run_id}"))
        })
        .unwrap_or_else(|| panic!("missing ownership artifact for {run_id}: {artifacts:?}"));
    artifact["artifactId"].as_str().unwrap().to_string()
}

fn read_artifact(
    session: &mut RunnerStdioSession,
    transport: Arc<IsolationTransport>,
    id: u64,
    run_id: &str,
    artifact_id: &str,
) -> JsonRpcResponse {
    session
        .handle_interactive_request(
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(id)),
                method: "artifact.read".to_string(),
                params: Some(json!({ "runId": run_id, "artifactId": artifact_id })),
            },
            transport,
        )
        .expect("artifact read")
        .expect("artifact read response")
}
