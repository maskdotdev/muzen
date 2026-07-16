use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE, RANGE};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;

use super::super::client::is_terminal_run_event;
use super::super::{
    AgentEvent, AgentMessage, ArtifactChunk, ArtifactId, CancelOptions, Capabilities,
    CommandOptions, CommandReceipt, CreateOptions, ErrorCode, EventOptions, EventStream,
    IdempotencyKey, MessagePage, MuzenError, Page, PutSecretInput, RunId, RunResult, RunSnapshot,
    RunSpec, RuntimeTransport, SecretRef, SendCommand, SessionId, SessionSnapshot, SessionSpec,
    SpawnCommand,
};

const IDEMPOTENCY_HEADER: &str = "idempotency-key";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

#[derive(Debug, Clone, Default)]
pub struct HttpTransportOptions {
    pub bearer_token: Option<String>,
}

#[derive(Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    base_url: Url,
    bearer_token: Option<String>,
    artifact_runs: Arc<Mutex<BTreeMap<ArtifactId, RunId>>>,
}

impl HttpTransport {
    pub fn new(
        base_url: impl AsRef<str>,
        options: HttpTransportOptions,
    ) -> Result<Self, MuzenError> {
        let mut base_url = Url::parse(base_url.as_ref()).map_err(|error| {
            MuzenError::invalid_input(format!("invalid HTTP base URL: {error}"))
        })?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(MuzenError::invalid_input(
                "HTTP base URL must use http or https",
            ));
        }
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        Ok(Self {
            client: reqwest::Client::new(),
            base_url,
            bearer_token: options.bearer_token,
            artifact_runs: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    fn endpoint<'a>(&self, segments: impl IntoIterator<Item = &'a str>) -> Url {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("http URLs support path segments")
            .pop_if_empty()
            .extend(segments);
        url
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        let request = self.client.request(method, url);
        if let Some(token) = self.bearer_token.as_deref() {
            request.header(AUTHORIZATION, format!("Bearer {token}"))
        } else {
            request
        }
    }

    fn with_key(request: RequestBuilder, key: Option<&IdempotencyKey>) -> RequestBuilder {
        match key {
            Some(key) => request.header(IDEMPOTENCY_HEADER, key.as_str()),
            None => request,
        }
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, MuzenError> {
        let response = request.send().await.map_err(transport_error)?;
        decode_json_response(response).await
    }
}

#[async_trait]
impl RuntimeTransport for HttpTransport {
    async fn capabilities(&self) -> Result<Capabilities, MuzenError> {
        self.json(self.request(Method::GET, self.endpoint(["v1", "capabilities"])))
            .await
    }

    async fn put_secret(&self, input: PutSecretInput) -> Result<SecretRef, MuzenError> {
        let request = Self::with_key(
            self.request(Method::POST, self.endpoint(["v1", "secrets"])),
            input.idempotency_key.as_ref(),
        )
        .json(&input);
        self.json(request).await
    }

    async fn delete_secret(&self, secret: &SecretRef) -> Result<(), MuzenError> {
        self.json(self.request(
            Method::DELETE,
            self.endpoint(["v1", "secrets", secret.as_str()]),
        ))
        .await
    }

    async fn create_session(
        &self,
        spec: SessionSpec,
        options: CreateOptions,
    ) -> Result<SessionId, MuzenError> {
        let request = Self::with_key(
            self.request(Method::POST, self.endpoint(["v1", "sessions"])),
            options.idempotency_key.as_ref(),
        )
        .json(&spec);
        self.json(request).await
    }

    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot, MuzenError> {
        self.json(self.request(Method::GET, self.endpoint(["v1", "sessions", id.as_str()])))
            .await
    }

    async fn messages(
        &self,
        id: &SessionId,
        page: MessagePage,
    ) -> Result<Page<AgentMessage>, MuzenError> {
        let mut url = self.endpoint(["v1", "sessions", id.as_str(), "messages"]);
        {
            let mut query = url.query_pairs_mut();
            if let Some(after) = page.after.as_deref() {
                query.append_pair("after", after);
            }
            if let Some(limit) = page.limit {
                query.append_pair("limit", &limit.to_string());
            }
        }
        self.json(self.request(Method::GET, url)).await
    }

    async fn archive_session(
        &self,
        id: &SessionId,
        options: CommandOptions,
    ) -> Result<(), MuzenError> {
        let request = Self::with_key(
            self.request(
                Method::POST,
                self.endpoint(["v1", "sessions", id.as_str(), "archive"]),
            ),
            options.idempotency_key.as_ref(),
        )
        .json(&options);
        self.json(request).await
    }

    async fn start_run(&self, spec: RunSpec) -> Result<RunId, MuzenError> {
        let request = Self::with_key(
            self.request(Method::POST, self.endpoint(["v1", "runs"])),
            spec.idempotency_key.as_ref(),
        )
        .json(&spec);
        self.json(request).await
    }

    async fn run_snapshot(&self, id: &RunId) -> Result<RunSnapshot, MuzenError> {
        self.json(self.request(Method::GET, self.endpoint(["v1", "runs", id.as_str()])))
            .await
    }

    async fn run_result(&self, id: &RunId) -> Result<Option<RunResult>, MuzenError> {
        let result: Option<RunResult> = self
            .json(self.request(
                Method::GET,
                self.endpoint(["v1", "runs", id.as_str(), "result"]),
            ))
            .await?;
        if let Some(result) = &result {
            let mut artifact_runs = self.artifact_runs.lock();
            for artifact in &result.artifacts {
                artifact_runs.insert(artifact.id.clone(), id.clone());
            }
        }
        Ok(result)
    }

    fn events(&self, id: &RunId, options: EventOptions) -> EventStream {
        let mut url = self.endpoint(["v1", "runs", id.as_str(), "events"]);
        if let Some(after) = options.after {
            url.query_pairs_mut()
                .append_pair("after", &after.to_string());
        }
        let client = self.clone();
        let after = options.after;
        let state = EventStreamState::Connecting { client, url, after };
        Box::pin(futures::stream::try_unfold(state, |state| async move {
            let mut state = match state {
                EventStreamState::Connecting { client, url, after } => {
                    let mut request = client.request(Method::GET, url);
                    if let Some(after) = after {
                        request = request.header(LAST_EVENT_ID_HEADER, after.to_string());
                    }
                    let response = request.send().await.map_err(transport_error)?;
                    if !response.status().is_success() {
                        return Err(decode_error_response(response).await);
                    }
                    EventStreamState::Reading {
                        response,
                        parser: SseParser::default(),
                        saw_terminal: false,
                    }
                }
                reading => reading,
            };
            let EventStreamState::Reading {
                response,
                parser,
                saw_terminal,
            } = &mut state
            else {
                unreachable!()
            };
            loop {
                if let Some(event) = parser.events.pop_front() {
                    *saw_terminal |= is_terminal_run_event(&event.event_type);
                    return Ok(Some((event, state)));
                }
                match response.chunk().await.map_err(transport_error)? {
                    Some(chunk) => parser.push(&chunk)?,
                    None => {
                        parser.finish()?;
                        if let Some(event) = parser.events.pop_front() {
                            *saw_terminal |= is_terminal_run_event(&event.event_type);
                            return Ok(Some((event, state)));
                        }
                        if !*saw_terminal {
                            return Err(MuzenError::unavailable(
                                "SSE stream ended before a terminal run event",
                            ));
                        }
                        return Ok(None);
                    }
                }
            }
        }))
    }

    async fn send(&self, id: &RunId, command: SendCommand) -> Result<CommandReceipt, MuzenError> {
        let request = Self::with_key(
            self.request(
                Method::POST,
                self.endpoint(["v1", "runs", id.as_str(), "send"]),
            ),
            command.idempotency_key.as_ref(),
        )
        .json(&command);
        self.json(request).await
    }

    async fn spawn(&self, id: &RunId, command: SpawnCommand) -> Result<SessionId, MuzenError> {
        let request = Self::with_key(
            self.request(
                Method::POST,
                self.endpoint(["v1", "runs", id.as_str(), "spawn"]),
            ),
            command.idempotency_key.as_ref(),
        )
        .json(&command);
        self.json(request).await
    }

    async fn cancel(
        &self,
        id: &RunId,
        options: CancelOptions,
    ) -> Result<CommandReceipt, MuzenError> {
        let request = Self::with_key(
            self.request(
                Method::POST,
                self.endpoint(["v1", "runs", id.as_str(), "cancel"]),
            ),
            options.idempotency_key.as_ref(),
        )
        .json(&options);
        self.json(request).await
    }

    async fn artifact_chunk(
        &self,
        artifact_id: &ArtifactId,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ArtifactChunk, MuzenError> {
        if max_bytes == 0 {
            return Err(MuzenError::invalid_input("max_bytes must be positive"));
        }
        let run_id = self
            .artifact_runs
            .lock()
            .get(artifact_id)
            .cloned()
            .ok_or_else(|| {
                MuzenError::not_found(
                    "artifact must be obtained from a run result before reading data",
                )
            })?;
        let end = offset.saturating_add(u64::from(max_bytes) - 1);
        let response = self
            .request(
                Method::GET,
                self.endpoint([
                    "v1",
                    "runs",
                    run_id.as_str(),
                    "artifacts",
                    artifact_id.as_str(),
                ]),
            )
            .header(RANGE, format!("bytes={offset}-{end}"))
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(decode_error_response(response).await);
        }
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| MuzenError::internal("artifact response omitted Content-Range"))?
            .to_owned();
        let (_, total) = parse_content_range(&content_range)?;
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            bytes.extend_from_slice(&chunk);
        }
        let eof = offset.saturating_add(bytes.len() as u64) >= total;
        Ok(ArtifactChunk {
            data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
            eof,
        })
    }

    async fn close(&self) -> Result<(), MuzenError> {
        Ok(())
    }
}

enum EventStreamState {
    Connecting {
        client: HttpTransport,
        url: Url,
        after: Option<u64>,
    },
    Reading {
        response: Response,
        parser: SseParser,
        saw_terminal: bool,
    },
}

#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    id: Option<String>,
    event_type: Option<String>,
    data: Vec<String>,
    events: VecDeque<AgentEvent>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Result<(), MuzenError> {
        self.buffer.extend_from_slice(bytes);
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&line)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), MuzenError> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.line(&line)?;
        }
        if !self.data.is_empty() {
            self.dispatch()?;
        }
        Ok(())
    }

    fn line(&mut self, line: &[u8]) -> Result<(), MuzenError> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line[0] == b':' {
            return Ok(());
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| MuzenError::unavailable("SSE stream contains invalid UTF-8"))?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" => self.id = Some(value.to_owned()),
            "event" => self.event_type = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<(), MuzenError> {
        if self.data.is_empty() {
            self.id = None;
            self.event_type = None;
            return Ok(());
        }
        if self
            .event_type
            .as_deref()
            .is_some_and(|kind| kind != "run.event")
        {
            return Err(MuzenError::unavailable("unexpected SSE event type"));
        }
        let event: AgentEvent = serde_json::from_str(&self.data.join("\n"))
            .map_err(|error| MuzenError::unavailable(format!("invalid SSE event data: {error}")))?;
        let id = self
            .id
            .as_deref()
            .ok_or_else(|| MuzenError::unavailable("SSE run event omitted id"))?
            .parse::<u64>()
            .map_err(|_| MuzenError::unavailable("SSE event id must be decimal"))?;
        if id != event.sequence {
            return Err(MuzenError::unavailable(
                "SSE event id does not match event sequence",
            ));
        }
        self.events.push_back(event);
        self.id = None;
        self.event_type = None;
        self.data.clear();
        Ok(())
    }
}

async fn decode_json_response<T: DeserializeOwned>(response: Response) -> Result<T, MuzenError> {
    if !response.status().is_success() {
        return Err(decode_error_response(response).await);
    }
    response
        .json()
        .await
        .map_err(|error| MuzenError::unavailable(format!("invalid HTTP response body: {error}")))
}

async fn decode_error_response(response: Response) -> MuzenError {
    let status = response.status();
    match response.json::<MuzenError>().await {
        Ok(error) => error,
        Err(_) => fallback_status_error(status),
    }
}

fn fallback_status_error(status: StatusCode) -> MuzenError {
    let code = match status {
        StatusCode::BAD_REQUEST | StatusCode::RANGE_NOT_SATISFIABLE => ErrorCode::InvalidInput,
        StatusCode::UNAUTHORIZED => ErrorCode::Unauthenticated,
        StatusCode::FORBIDDEN => ErrorCode::PermissionDenied,
        StatusCode::NOT_FOUND => ErrorCode::NotFound,
        StatusCode::CONFLICT => ErrorCode::Conflict,
        StatusCode::TOO_MANY_REQUESTS => ErrorCode::ResourceExhausted,
        StatusCode::NOT_IMPLEMENTED => ErrorCode::Unsupported,
        StatusCode::SERVICE_UNAVAILABLE => ErrorCode::Unavailable,
        StatusCode::GATEWAY_TIMEOUT => ErrorCode::DeadlineExceeded,
        _ => ErrorCode::Internal,
    };
    MuzenError::new(code, format!("HTTP service returned status {status}"))
}

fn transport_error(error: reqwest::Error) -> MuzenError {
    MuzenError::unavailable(format!("HTTP transport failed: {error}"))
}

fn parse_content_range(value: &str) -> Result<((u64, u64), u64), MuzenError> {
    let value = value
        .strip_prefix("bytes ")
        .ok_or_else(|| MuzenError::internal("invalid Content-Range unit"))?;
    let (range, total) = value
        .split_once('/')
        .ok_or_else(|| MuzenError::internal("invalid Content-Range"))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| MuzenError::internal("invalid Content-Range bounds"))?;
    let start = start
        .parse()
        .map_err(|_| MuzenError::internal("invalid Content-Range start"))?;
    let end = end
        .parse()
        .map_err(|_| MuzenError::internal("invalid Content-Range end"))?;
    let total = total
        .parse()
        .map_err(|_| MuzenError::internal("invalid Content-Range total"))?;
    Ok(((start, end), total))
}

#[cfg(test)]
mod unit {
    use super::SseParser;

    #[test]
    fn parses_incremental_crlf_multiline_sse_and_ignores_comments() {
        let mut parser = SseParser::default();
        parser
            .push(
                b": keepalive\r\n\r\nid: 7\r\nevent: run.event\r\ndata: {\"runId\":\"run-1\",\r\n",
            )
            .unwrap();
        parser.push(b"data: \"sequence\":7,\"type\":\"run.completed\",\"timestamp\":\"now\",\"payload\":{}}\r\n\r\n").unwrap();
        let event = parser.events.pop_front().expect("event");
        assert_eq!(event.sequence, 7);
        assert_eq!(event.event_type, "run.completed");
    }
}
