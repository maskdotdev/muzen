use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::invalid;
use crate::agent_runtime::MuzenError;

type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, MuzenError>> + Send + 'static>>;
type ToolHandler = dyn Fn(Value) -> ToolFuture + Send + Sync + 'static;

/// Async handler accepted by [`Tool::typed`].
pub trait TypedToolHandler<I>: Send + Sync + 'static {
    type Future: Future<Output = Result<Value, MuzenError>> + Send + 'static;

    fn call(&self, input: I) -> Self::Future;
}

impl<I, F, Fut> TypedToolHandler<I> for F
where
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, MuzenError>> + Send + 'static,
{
    type Future = Fut;

    fn call(&self, input: I) -> Self::Future {
        self(input)
    }
}

/// An async Rust function exposed to an [`super::Agent`] as a local MCP tool.
#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    handler: Arc<ToolHandler>,
}

impl Tool {
    /// Wraps an async JSON-value handler as a local tool.
    pub fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: F,
    ) -> Result<Self, MuzenError>
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, MuzenError>> + Send + 'static,
    {
        let name = name.into();
        validate_tool_name(&name)?;
        Ok(Self {
            name,
            description: description.into(),
            input_schema,
            handler: Arc::new(move |arguments| Box::pin(handler(arguments))),
        })
    }

    /// Wraps an async handler whose input is deserialized from the MCP arguments object.
    pub fn typed<I>(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl TypedToolHandler<I>,
    ) -> Result<Self, MuzenError>
    where
        I: DeserializeOwned + Send + 'static,
    {
        let handler = Arc::new(handler);
        Self::new(name, description, input_schema, move |arguments| {
            let handler = Arc::clone(&handler);
            async move {
                let input = serde_json::from_value(arguments).map_err(|error| {
                    MuzenError::invalid_input(format!("invalid tool arguments: {error}"))
                })?;
                handler.call(input).await
            }
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    pub async fn invoke(&self, arguments: Value) -> Result<Value, MuzenError> {
        (self.handler)(arguments).await
    }
}

fn validate_tool_name(name: &str) -> Result<(), MuzenError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(invalid("tool.name", "must match [a-zA-Z0-9_-]{1,64}"))
    }
}

struct ToolServerState {
    tools: Vec<Tool>,
}

#[derive(Default)]
struct RunningServer {
    address: Option<SocketAddr>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

/// A small Streamable HTTP MCP server bound only to an ephemeral loopback port.
pub struct LoopbackToolServer {
    state: Arc<ToolServerState>,
    running: Mutex<RunningServer>,
}

impl LoopbackToolServer {
    pub fn new(tools: impl IntoIterator<Item = Tool>) -> Result<Self, MuzenError> {
        let tools = tools.into_iter().collect::<Vec<_>>();
        let mut names = std::collections::BTreeSet::new();
        if tools.iter().any(|tool| !names.insert(tool.name())) {
            return Err(invalid("tools", "must have unique function names"));
        }
        Ok(Self {
            state: Arc::new(ToolServerState { tools }),
            running: Mutex::new(RunningServer::default()),
        })
    }

    /// Starts the server if needed and returns its MCP endpoint.
    ///
    /// This is synchronous so no future holds the lifecycle lock while binding or spawning.
    pub fn start(&self) -> Result<String, MuzenError> {
        let mut running = self.running.lock();
        if let Some(address) = running.address {
            return Ok(endpoint(address));
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| {
            MuzenError::unavailable(format!("failed to bind local tool server: {error}"))
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            MuzenError::internal(format!("failed to configure local tool server: {error}"))
        })?;
        let address = listener.local_addr().map_err(|error| {
            MuzenError::internal(format!("failed to read local tool server address: {error}"))
        })?;
        let listener = tokio::net::TcpListener::from_std(listener).map_err(|error| {
            MuzenError::internal(format!("failed to start local tool server: {error}"))
        })?;
        let app = Router::new()
            .route("/mcp", post(mcp_handler))
            .with_state(Arc::clone(&self.state));
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = receiver.await;
            });
            let _ = server.await;
        });
        running.address = Some(address);
        running.shutdown = Some(shutdown);
        running.task = Some(task);
        Ok(endpoint(address))
    }

    pub fn url(&self) -> Result<String, MuzenError> {
        self.running
            .lock()
            .address
            .map(endpoint)
            .ok_or_else(|| MuzenError::conflict("tool server has not been started"))
    }

    /// Gracefully stops the server. Calling this more than once is harmless.
    pub async fn close(&self) {
        let task = self.begin_shutdown();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    fn begin_shutdown(&self) -> Option<JoinHandle<()>> {
        let mut running = self.running.lock();
        if let Some(shutdown) = running.shutdown.take() {
            let _ = shutdown.send(());
        }
        running.address = None;
        running.task.take()
    }
}

impl Drop for LoopbackToolServer {
    fn drop(&mut self) {
        // Dropping cannot await, but sending the graceful-shutdown signal releases the listener
        // and lets the already-spawned axum task finish without an abort.
        let _ = self.begin_shutdown();
    }
}

fn endpoint(address: SocketAddr) -> String {
    format!("http://{address}/mcp")
}

async fn mcp_handler(State(state): State<Arc<ToolServerState>>, body: Bytes) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return rpc_error(Value::Null, -32700, "invalid JSON"),
    };
    let request_id = request.get("id").cloned().unwrap_or(Value::Null);
    match request.get("method").and_then(Value::as_str) {
        Some("notifications/initialized") => StatusCode::ACCEPTED.into_response(),
        Some("initialize") => {
            let mut response = Json(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "muzen-python-tools", "version": "1" }
                }
            }))
            .into_response();
            response.headers_mut().insert(
                "mcp-session-id",
                HeaderValue::from_static("muzen-python-tools"),
            );
            response
        }
        Some("tools/list") => {
            let tools = state
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name(),
                        "description": tool.description(),
                        "inputSchema": tool.input_schema()
                    })
                })
                .collect::<Vec<_>>();
            rpc_result(request_id, json!({ "tools": tools }))
        }
        Some("tools/call") => call_tool(state, request_id, &request).await,
        _ => rpc_error(request_id, -32601, "method not found"),
    }
}

async fn call_tool(state: Arc<ToolServerState>, request_id: Value, request: &Value) -> Response {
    let params = request.get("params").and_then(Value::as_object);
    let name = params
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str);
    let Some(tool) = name.and_then(|name| state.tools.iter().find(|tool| tool.name() == name))
    else {
        return rpc_error(request_id, -32602, "unknown tool");
    };
    let arguments = params.and_then(|params| params.get("arguments"));
    let arguments = match arguments {
        None => json!({}),
        Some(Value::Object(_)) => arguments.cloned().unwrap_or_else(|| json!({})),
        Some(_) => return rpc_error(request_id, -32602, "arguments must be an object"),
    };
    match tool.invoke(arguments).await {
        Ok(Value::String(text)) => rpc_result(
            request_id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            }),
        ),
        Ok(value) => {
            let text = python_json(&value);
            rpc_result(
                request_id,
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": value,
                    "isError": false
                }),
            )
        }
        Err(error) => rpc_result(
            request_id,
            json!({
                "content": [{ "type": "text", "text": error.to_string() }],
                "isError": true
            }),
        ),
    }
}

fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    }))
    .into_response()
}

// Python's json.dumps inserts one space after commas and colons. Keep that textual MCP result
// parity while still using serde_json for all escaping and number formatting.
fn python_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).expect("strings serialize"),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    serde_json::to_string(key).expect("object keys serialize"),
                    python_json(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
