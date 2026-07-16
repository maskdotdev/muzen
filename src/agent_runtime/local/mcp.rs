use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue as HttpHeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE,
};
use reqwest::{Client, StatusCode, Url};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::credentials::CredentialResolver;
use crate::agent_runtime::{
    ExecutionError, ExecutionErrorCode, HeaderValue, SessionId, ToolProvider, ToolProviderId,
};

const PROTOCOL_VERSION: &str = "2025-03-26";
const SESSION_HEADER: &str = "mcp-session-id";
const MAX_CONNECTIONS: usize = 128;
const SAFE_BODY_LIMIT: usize = 512;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct McpToolDefinition {
    pub provider: ToolProviderId,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

pub(super) struct McpToolClient {
    credentials: Arc<dyn CredentialResolver>,
    client: Client,
    allow_loopback_http: bool,
    cache: Mutex<ConnectionCache>,
}

#[derive(Default)]
struct ConnectionCache {
    entries: BTreeMap<(SessionId, ToolProviderId), Arc<Mutex<ConnectionState>>>,
    order: VecDeque<(SessionId, ToolProviderId)>,
}

#[derive(Default)]
struct ConnectionState {
    initialized: bool,
    session_id: Option<HttpHeaderValue>,
    next_request_id: u64,
    tools: Option<Vec<McpToolDefinition>>,
}

impl McpToolClient {
    pub(super) fn new(
        credentials: Arc<dyn CredentialResolver>,
        allow_loopback_http: bool,
    ) -> Result<Self, crate::agent_runtime::MuzenError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                crate::agent_runtime::MuzenError::internal(format!(
                    "failed to build MCP HTTP client: {error}"
                ))
            })?;
        Ok(Self {
            credentials,
            client,
            allow_loopback_http,
            cache: Mutex::new(ConnectionCache::default()),
        })
    }

    pub(super) async fn list_tools(
        &self,
        session: &SessionId,
        provider: &ToolProvider,
    ) -> Result<Vec<McpToolDefinition>, ExecutionError> {
        let connection = self.connection(session, provider.id()).await;
        let mut state = connection.lock().await;
        if let Some(tools) = &state.tools {
            return Ok(tools.clone());
        }
        self.initialize(provider, &mut state).await?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({ "cursor": cursor }))
                .unwrap_or_else(|| json!({}));
            let response = self
                .request(provider, &mut state, "tools/list", params)
                .await?;
            let page = response
                .get("result")
                .ok_or_else(|| protocol_error("MCP tools/list response omitted result"))?;
            let listed = page["tools"]
                .as_array()
                .ok_or_else(|| protocol_error("MCP tools/list response omitted tools"))?;
            for tool in listed {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                tools.push(McpToolDefinition {
                    provider: provider.id().clone(),
                    name: name.to_owned(),
                    description: tool["description"].as_str().map(str::to_owned),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                });
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        state.tools = Some(tools.clone());
        Ok(tools)
    }

    pub(super) async fn call(
        &self,
        session: &SessionId,
        provider: &ToolProvider,
        name: &str,
        arguments: Value,
    ) -> Result<Value, ExecutionError> {
        let connection = self.connection(session, provider.id()).await;
        let mut state = connection.lock().await;
        self.initialize(provider, &mut state).await?;
        let response = self
            .request(
                provider,
                &mut state,
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let result = response
            .get("result")
            .ok_or_else(|| protocol_error("MCP tools/call response omitted result"))?;
        if result["isError"].as_bool() == Some(true) {
            return Err(ExecutionError {
                code: ExecutionErrorCode::ToolError,
                message: text_content(result)
                    .unwrap_or_else(|| "MCP tool returned an error".to_owned()),
                retryable: false,
                details: None,
            });
        }
        if let Some(structured) = result.get("structuredContent") {
            return Ok(structured.clone());
        }
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let [block] = content.as_slice() {
            if block["type"] == "text" {
                if let Some(text) = block["text"].as_str() {
                    return Ok(Value::String(text.to_owned()));
                }
            }
        }
        Ok(Value::Array(content))
    }

    async fn connection(
        &self,
        session: &SessionId,
        provider: &ToolProviderId,
    ) -> Arc<Mutex<ConnectionState>> {
        let key = (session.clone(), provider.clone());
        let mut cache = self.cache.lock().await;
        if let Some(existing) = cache.entries.get(&key).cloned() {
            cache.order.retain(|candidate| candidate != &key);
            cache.order.push_back(key);
            return existing;
        }
        let connection = Arc::new(Mutex::new(ConnectionState::default()));
        cache.entries.insert(key.clone(), Arc::clone(&connection));
        cache.order.push_back(key);
        while cache.entries.len() > MAX_CONNECTIONS {
            if let Some(oldest) = cache.order.pop_front() {
                cache.entries.remove(&oldest);
            }
        }
        connection
    }

    async fn initialize(
        &self,
        provider: &ToolProvider,
        state: &mut ConnectionState,
    ) -> Result<(), ExecutionError> {
        if state.initialized {
            return Ok(());
        }
        let response = self
            .request(
                provider,
                state,
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "muzen", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        if response.get("result").is_none() {
            return Err(protocol_error("MCP initialize response omitted result"));
        }
        self.notification(provider, state, "notifications/initialized", json!({}))
            .await?;
        state.initialized = true;
        Ok(())
    }

    async fn request(
        &self,
        provider: &ToolProvider,
        state: &mut ConnectionState,
        method: &str,
        params: Value,
    ) -> Result<Value, ExecutionError> {
        state.next_request_id = state.next_request_id.saturating_add(1);
        let id = state.next_request_id;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = self.send(provider, state, body).await?;
        decode_response(response, id).await
    }

    async fn notification(
        &self,
        provider: &ToolProvider,
        state: &mut ConnectionState,
        method: &str,
        params: Value,
    ) -> Result<(), ExecutionError> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let response = self.send(provider, state, body).await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(http_error(response.status(), "MCP notification failed"))
        }
    }

    async fn send(
        &self,
        provider: &ToolProvider,
        state: &mut ConnectionState,
        body: Value,
    ) -> Result<reqwest::Response, ExecutionError> {
        let ToolProvider::McpHttp {
            url,
            credential,
            headers,
            ..
        } = provider
        else {
            return Err(protocol_error("tool provider is not MCP HTTP"));
        };
        let endpoint = self.endpoint(url)?;
        let mut resolved = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| protocol_error("MCP header name is invalid"))?;
            let bytes = match value {
                HeaderValue::Literal(value) => value.as_bytes().to_vec(),
                HeaderValue::Secret(secret) => self.secret(&secret.secret).await?,
            };
            let value = HttpHeaderValue::from_bytes(&bytes)
                .map_err(|_| secret_error("MCP header value is not a valid HTTP header value"))?;
            resolved.insert(name, value);
        }
        if let Some(secret) = credential {
            let secret = self.secret(secret).await?;
            let text = std::str::from_utf8(&secret)
                .map_err(|_| secret_error("MCP credential is not valid UTF-8"))?;
            let value = HttpHeaderValue::from_str(&format!("Bearer {text}"))
                .map_err(|_| secret_error("MCP credential is not a valid HTTP header value"))?;
            resolved.entry(AUTHORIZATION).or_insert(value);
        }
        let mut request = self
            .client
            .post(endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .headers(resolved)
            .json(&body);
        if let Some(session_id) = &state.session_id {
            request = request.header(SESSION_HEADER, session_id);
        }
        let response = request.send().await.map_err(|error| ExecutionError {
            code: ExecutionErrorCode::ToolError,
            message: format!("MCP transport error: {error}"),
            retryable: true,
            details: None,
        })?;
        if state.session_id.is_none() {
            state.session_id = response.headers().get(SESSION_HEADER).cloned();
        }
        Ok(response)
    }

    async fn secret(
        &self,
        secret: &crate::agent_runtime::SecretRef,
    ) -> Result<Vec<u8>, ExecutionError> {
        self.credentials
            .resolve(secret)
            .await
            .map(|value| value.as_bytes().to_vec())
            .ok_or_else(|| secret_error("MCP credential is unavailable"))
    }

    fn endpoint(&self, value: &str) -> Result<Url, ExecutionError> {
        let url = Url::parse(value).map_err(|_| protocol_error("MCP URL is invalid"))?;
        let loopback = url.host().is_some_and(|host| {
            let serialized = host.to_string();
            serialized.eq_ignore_ascii_case("localhost")
                || serialized
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|address| {
                        address == IpAddr::V4(Ipv4Addr::LOCALHOST)
                            || address == IpAddr::V6(Ipv6Addr::LOCALHOST)
                    })
        });
        if url.scheme() == "https"
            || (url.scheme() == "http" && loopback && self.allow_loopback_http)
        {
            Ok(url)
        } else {
            Err(ExecutionError {
                code: ExecutionErrorCode::ToolError,
                message: "MCP URL must use HTTPS; loopback HTTP requires explicit local opt-in"
                    .to_owned(),
                retryable: false,
                details: None,
            })
        }
    }
}

async fn decode_response(
    mut response: reqwest::Response,
    request_id: u64,
) -> Result<Value, ExecutionError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body = body.chars().take(SAFE_BODY_LIMIT).collect::<String>();
        return Err(http_error(status, &format!("MCP HTTP error: {body}")));
    }
    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
    if !is_sse {
        let value: Value = response
            .json()
            .await
            .map_err(|error| protocol_error(format!("invalid MCP JSON response: {error}")))?;
        return validate_rpc(value, request_id);
    }
    let mut parser = JsonSseParser::default();
    while let Some(chunk) = response.chunk().await.map_err(|error| ExecutionError {
        code: ExecutionErrorCode::ToolError,
        message: format!("failed to read MCP SSE response: {error}"),
        retryable: true,
        details: None,
    })? {
        for value in parser.push(&chunk)? {
            if value["id"].as_u64() == Some(request_id) {
                return validate_rpc(value, request_id);
            }
        }
    }
    for value in parser.finish()? {
        if value["id"].as_u64() == Some(request_id) {
            return validate_rpc(value, request_id);
        }
    }
    Err(protocol_error(
        "MCP SSE response ended before the JSON-RPC response",
    ))
}

fn validate_rpc(value: Value, request_id: u64) -> Result<Value, ExecutionError> {
    if value["id"].as_u64() != Some(request_id) {
        return Err(protocol_error(
            "MCP JSON-RPC response id did not match request",
        ));
    }
    if let Some(error) = value.get("error") {
        return Err(ExecutionError {
            code: ExecutionErrorCode::ToolError,
            message: error["message"]
                .as_str()
                .unwrap_or("MCP JSON-RPC request failed")
                .to_owned(),
            retryable: false,
            details: Some(error.clone()),
        });
    }
    Ok(value)
}

#[derive(Default)]
struct JsonSseParser {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl JsonSseParser {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, ExecutionError> {
        self.buffer.extend_from_slice(bytes);
        let mut values = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(value) = self.line(&line)? {
                values.push(value);
            }
        }
        Ok(values)
    }

    fn finish(&mut self) -> Result<Vec<Value>, ExecutionError> {
        let mut values = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(value) = self.line(&line)? {
                values.push(value);
            }
        }
        if let Some(value) = self.dispatch()? {
            values.push(value);
        }
        Ok(values)
    }

    fn line(&mut self, line: &[u8]) -> Result<Option<Value>, ExecutionError> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.first() == Some(&b':') {
            return Ok(None);
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| protocol_error("MCP SSE response contains invalid UTF-8"))?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
        Ok(None)
    }

    fn dispatch(&mut self) -> Result<Option<Value>, ExecutionError> {
        if self.data.is_empty() {
            return Ok(None);
        }
        let data = std::mem::take(&mut self.data).join("\n");
        serde_json::from_str(&data)
            .map(Some)
            .map_err(|error| protocol_error(format!("invalid JSON in MCP SSE event: {error}")))
    }
}

fn text_content(result: &Value) -> Option<String> {
    let texts = result["content"]
        .as_array()?
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn protocol_error(message: impl Into<String>) -> ExecutionError {
    ExecutionError {
        code: ExecutionErrorCode::ToolError,
        message: message.into(),
        retryable: false,
        details: None,
    }
}

fn secret_error(message: impl Into<String>) -> ExecutionError {
    ExecutionError {
        code: ExecutionErrorCode::SecretUnavailable,
        message: message.into(),
        retryable: false,
        details: None,
    }
}

fn http_error(status: StatusCode, message: &str) -> ExecutionError {
    ExecutionError {
        code: ExecutionErrorCode::ToolError,
        message: message.to_owned(),
        retryable: status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error(),
        details: Some(json!({ "status": status.as_u16() })),
    }
}
