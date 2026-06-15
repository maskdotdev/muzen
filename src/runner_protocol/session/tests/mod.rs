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
