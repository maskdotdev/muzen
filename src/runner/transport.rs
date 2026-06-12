use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::protocol::{
    parse_jsonrpc_frame, write_notification, write_response, JsonRpcFrame, JsonRpcResponse,
};

#[derive(Debug, Clone, serde::Serialize)]
struct JsonRpcOutboundRequest {
    jsonrpc: &'static str,
    id: Value,
    method: String,
    params: Value,
}

pub(crate) trait RunnerCallbackTransport: Send + Sync {
    fn request(&self, method: &str, params: Value) -> Result<Value>;
    fn notify(&self, method: &str, params: Value) -> Result<()>;
    fn respond(&self, response: &JsonRpcResponse) -> Result<()>;
}

pub(crate) struct InteractiveTransport<R, W> {
    incoming: Mutex<mpsc::Receiver<TransportRead>>,
    callbacks: Arc<Mutex<CallbackRoutes>>,
    writer: Mutex<W>,
    next_request_id: AtomicU64,
    _reader: std::marker::PhantomData<fn(R)>,
}

struct InteractiveTransportReader<R> {
    reader: R,
    line: String,
}

/// A malformed line must not kill the session: the reader reports it as a
/// recoverable event so the session can answer with a JSON-RPC parse error
/// and keep reading. Only I/O errors tear the transport down.
pub(crate) enum TransportEvent {
    Frame(JsonRpcFrame),
    ParseError(String),
}

type TransportRead = std::result::Result<TransportEvent, String>;

#[derive(Default)]
struct CallbackRoutes {
    waiters: BTreeMap<String, mpsc::Sender<JsonRpcResponse>>,
    early_responses: BTreeMap<String, Vec<JsonRpcResponse>>,
    /// Set when the read side is gone (EOF or I/O error). Callback requests
    /// can never be answered after that, so waits must fail instead of
    /// blocking forever — otherwise draining in-flight runs on EOF would
    /// deadlock on callback-model sessions.
    closed: bool,
}

impl<R, W> InteractiveTransport<R, W>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    pub(crate) fn new(reader: R, writer: W) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel();
        let callbacks = Arc::new(Mutex::new(CallbackRoutes::default()));
        let reader_callbacks = Arc::clone(&callbacks);
        std::thread::spawn(move || {
            let mut reader = InteractiveTransportReader {
                reader,
                line: String::new(),
            };
            loop {
                match reader.read_frame() {
                    Ok(Some(TransportEvent::Frame(frame))) => {
                        if route_callback_response(&reader_callbacks, &frame) {
                            continue;
                        }
                        if incoming_tx.send(Ok(TransportEvent::Frame(frame))).is_err() {
                            break;
                        }
                    }
                    Ok(Some(event)) => {
                        if incoming_tx.send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = incoming_tx.send(Err(error.to_string()));
                        break;
                    }
                }
            }
            let mut callbacks = reader_callbacks
                .lock()
                .expect("runner callback routes poisoned");
            callbacks.closed = true;
            callbacks.waiters.clear();
        });
        Self {
            incoming: Mutex::new(incoming_rx),
            callbacks,
            writer: Mutex::new(writer),
            next_request_id: AtomicU64::new(1),
            _reader: std::marker::PhantomData,
        }
    }

    pub(crate) fn read_frame(&self) -> Result<Option<TransportEvent>> {
        let incoming = self.incoming.lock().expect("runner stdio lock poisoned");
        match incoming.recv() {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(error)) => anyhow::bail!("{error}"),
            Err(_) => Ok(None),
        }
    }

    pub(crate) fn write_response(&self, response: &JsonRpcResponse) -> Result<()> {
        let mut writer = self.writer.lock().expect("runner stdio lock poisoned");
        write_response(&mut *writer, response)
    }
}

impl<R, W> RunnerCallbackTransport for InteractiveTransport<R, W>
where
    R: Send,
    W: Write + Send,
{
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let request_id = format!(
            "runner-callback-{}",
            self.next_request_id.fetch_add(1, Ordering::SeqCst)
        );
        let request_id_value = json!(request_id);
        let waiter_key =
            jsonrpc_id_key(&request_id_value).expect("runner callback request ids are keyed");
        let write_result = {
            let mut writer = self.writer.lock().expect("runner stdio lock poisoned");
            write_request(&mut *writer, &request_id_value, method, params)
        };
        write_result?;
        let response = wait_for_callback_response(&self.callbacks, &waiter_key)
            .with_context(|| format!("SDK closed stdio while waiting for {method} response"))?;
        callback_response_result(method, response)
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut writer = self.writer.lock().expect("runner stdio lock poisoned");
        write_notification(&mut *writer, method, params)
    }

    fn respond(&self, response: &JsonRpcResponse) -> Result<()> {
        let mut writer = self.writer.lock().expect("runner stdio lock poisoned");
        write_response(&mut *writer, response)
    }
}

fn callback_response_result(method: &str, response: JsonRpcResponse) -> Result<Value> {
    if let Some(error) = response.error {
        anyhow::bail!(
            "SDK callback {method} failed: {} ({})",
            error.message,
            error
                .data
                .as_ref()
                .map(|data| data.kind.as_str())
                .unwrap_or("unknown")
        );
    }
    Ok(response.result.unwrap_or(Value::Null))
}

impl<R> InteractiveTransportReader<R>
where
    R: BufRead,
{
    fn read_frame(&mut self) -> Result<Option<TransportEvent>> {
        loop {
            self.line.clear();
            let bytes = self
                .reader
                .read_line(&mut self.line)
                .context("failed to read runner protocol frame")?;
            if bytes == 0 {
                return Ok(None);
            }
            if self.line.trim().is_empty() {
                continue;
            }
            return Ok(Some(match parse_jsonrpc_frame(self.line.trim_end()) {
                Ok(frame) => TransportEvent::Frame(frame),
                Err(error) => TransportEvent::ParseError(error.to_string()),
            }));
        }
    }
}

fn route_callback_response(callbacks: &Mutex<CallbackRoutes>, frame: &JsonRpcFrame) -> bool {
    let JsonRpcFrame::Response(response) = frame else {
        return false;
    };
    let Some(key) = response.id.as_ref().and_then(jsonrpc_id_key) else {
        return false;
    };
    if !is_runner_callback_key(&key) {
        return false;
    }
    let mut callbacks = callbacks.lock().expect("runner callback routes poisoned");
    if let Some(waiter) = callbacks.waiters.remove(&key) {
        let _ = waiter.send(response.clone());
    } else {
        callbacks
            .early_responses
            .entry(key)
            .or_default()
            .push(response.clone());
    }
    true
}

fn wait_for_callback_response(
    callbacks: &Mutex<CallbackRoutes>,
    key: &str,
) -> Option<JsonRpcResponse> {
    let mut callbacks = callbacks.lock().expect("runner callback routes poisoned");
    if let Some(responses) = callbacks.early_responses.get_mut(key) {
        let response = responses.remove(0);
        if responses.is_empty() {
            callbacks.early_responses.remove(key);
        }
        return Some(response);
    }
    if callbacks.closed {
        return None;
    }
    let (response_tx, response_rx) = mpsc::channel();
    callbacks.waiters.insert(key.to_string(), response_tx);
    drop(callbacks);
    response_rx.recv().ok()
}

fn is_runner_callback_key(key: &str) -> bool {
    key.starts_with("s:runner-callback-")
}

fn jsonrpc_id_key(id: &Value) -> Option<String> {
    match id {
        Value::String(value) => Some(format!("s:{value}")),
        Value::Number(value) => Some(format!("n:{value}")),
        Value::Null => Some("null".to_string()),
        _ => None,
    }
}

fn write_request<W: Write>(writer: &mut W, id: &Value, method: &str, params: Value) -> Result<()> {
    let request = JsonRpcOutboundRequest {
        jsonrpc: "2.0",
        id: id.clone(),
        method: method.to_string(),
        params,
    };
    serde_json::to_writer(&mut *writer, &request)
        .context("failed to write runner callback request")?;
    writer
        .write_all(b"\n")
        .context("failed to terminate runner callback request")?;
    writer
        .flush()
        .context("failed to flush runner callback request")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_request_fails_after_reader_eof_instead_of_hanging() {
        let transport = InteractiveTransport::new(std::io::Cursor::new(Vec::new()), Vec::new());
        assert!(
            transport.read_frame().expect("transport read").is_none(),
            "empty stdin reads as EOF"
        );
        let error = transport
            .request("model.complete", json!({}))
            .expect_err("callback request after EOF must fail");
        assert!(error.to_string().contains("model.complete"));
    }

    #[test]
    fn malformed_line_surfaces_as_recoverable_parse_error() {
        let input = b"{\"this is not json\n{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n".to_vec();
        let transport = InteractiveTransport::new(std::io::Cursor::new(input), Vec::new());
        match transport.read_frame().expect("transport read") {
            Some(TransportEvent::ParseError(message)) => {
                assert!(message.contains("invalid JSON-RPC frame"));
            }
            other => panic!("expected parse error, got {:?}", other.is_some()),
        }
        match transport.read_frame().expect("transport read") {
            Some(TransportEvent::Frame(JsonRpcFrame::Notification)) => {}
            other => panic!(
                "stream continues past parse error, got {:?}",
                other.is_some()
            ),
        }
        assert!(transport.read_frame().expect("transport read").is_none());
    }
}
