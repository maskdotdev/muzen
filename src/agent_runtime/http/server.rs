use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use futures::{stream, StreamExt};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;

use super::super::{
    AgentInput, ArtifactChunk, ArtifactId, CancelOptions, CommandOptions, CommandReceipt,
    CreateOptions, ErrorCode, EventOptions, IdempotencyKey, MessagePage, MuzenError,
    PutSecretInput, RunId, RunRoot, RunSpec, RuntimeTransport, SecretRef, SendCommand, SessionId,
    SessionSpec, SingleRunOptions, SpawnCommand,
};

const ARTIFACT_CHUNK_BYTES: u32 = 64 * 1024;
const IDEMPOTENCY_HEADER: &str = "idempotency-key";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

#[derive(Debug, Clone)]
pub struct HttpServiceConfig {
    pub bearer_token: Option<String>,
    pub keepalive_interval: Duration,
}

impl Default for HttpServiceConfig {
    fn default() -> Self {
        Self {
            bearer_token: None,
            keepalive_interval: Duration::from_secs(15),
        }
    }
}

#[derive(Clone)]
struct ServiceState {
    inner: Arc<dyn RuntimeTransport>,
    keepalive_interval: Duration,
    archive_replays: Arc<Mutex<BTreeMap<SessionId, Option<IdempotencyKey>>>>,
    cancel_replays: Arc<Mutex<BTreeMap<(RunId, IdempotencyKey), (CancelOptions, CommandReceipt)>>>,
}

#[derive(Clone)]
struct AuthState {
    bearer_token: Option<String>,
}

/// Builds all sixteen v1 routes around an existing runtime transport.
pub fn router(inner: Arc<dyn RuntimeTransport>, config: HttpServiceConfig) -> Router {
    let state = ServiceState {
        inner,
        keepalive_interval: config.keepalive_interval,
        archive_replays: Arc::new(Mutex::new(BTreeMap::new())),
        cancel_replays: Arc::new(Mutex::new(BTreeMap::new())),
    };
    let auth = Arc::new(AuthState {
        bearer_token: config.bearer_token,
    });
    Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/secrets", post(put_secret))
        .route("/v1/secrets/{secret_ref}", delete(delete_secret))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{session_id}", get(session_snapshot))
        .route("/v1/sessions/{session_id}/messages", get(session_messages))
        .route("/v1/sessions/{session_id}/archive", post(archive_session))
        .route("/v1/sessions/{session_id}/runs", post(session_run))
        .route("/v1/runs", post(start_run))
        .route("/v1/runs/{run_id}", get(run_snapshot))
        .route("/v1/runs/{run_id}/result", get(run_result))
        .route("/v1/runs/{run_id}/events", get(run_events))
        .route("/v1/runs/{run_id}/send", post(run_send))
        .route("/v1/runs/{run_id}/spawn", post(run_spawn))
        .route("/v1/runs/{run_id}/cancel", post(run_cancel))
        .route(
            "/v1/runs/{run_id}/artifacts/{artifact_id}",
            get(run_artifact),
        )
        .fallback(route_not_found)
        .method_not_allowed_fallback(route_not_found)
        .with_state(state)
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(auth, authenticate))
}

/// Serves until `shutdown` resolves, then closes the shared runtime.
pub async fn serve<F>(
    listener: TcpListener,
    inner: Arc<dyn RuntimeTransport>,
    config: HttpServiceConfig,
    shutdown: F,
) -> Result<(), MuzenError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let app = router(Arc::clone(&inner), config);
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| MuzenError::unavailable(format!("HTTP service failed: {error}")));
    let closed = inner.close().await;
    served.and(closed)
}

async fn authenticate(
    State(state): State<Arc<AuthState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.bearer_token.as_deref() else {
        return next.run(request).await;
    };
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied == Some(expected) {
        next.run(request).await
    } else {
        HttpError::new(MuzenError::unauthenticated(
            "missing or invalid bearer token",
        ))
        .into_response()
    }
}

async fn capabilities(State(state): State<ServiceState>) -> Result<Json<Value>, HttpError> {
    json_result(state.inner.capabilities().await)
}

async fn put_secret(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let mut input: PutSecretInput = parse_json(&body)?;
    merge_key(&headers, &mut input.idempotency_key)?;
    json_result(state.inner.put_secret(input).await)
}

async fn delete_secret(
    State(state): State<ServiceState>,
    Path(secret): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let _ = header_key(&headers)?;
    let secret = SecretRef::new(secret).map_err(invalid)?;
    json_result(state.inner.delete_secret(&secret).await)
}

async fn create_session(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let spec: SessionSpec = parse_json(&body)?;
    let options = CreateOptions {
        idempotency_key: header_key(&headers)?,
    };
    json_result(state.inner.create_session(spec, options).await)
}

async fn session_snapshot(
    State(state): State<ServiceState>,
    Path(session): Path<String>,
) -> Result<Json<Value>, HttpError> {
    let session = SessionId::new(session).map_err(invalid)?;
    json_result(state.inner.session_snapshot(&session).await)
}

async fn session_messages(
    State(state): State<ServiceState>,
    Path(session): Path<String>,
    query: Result<Query<MessagePage>, QueryRejection>,
) -> Result<Json<Value>, HttpError> {
    let session = SessionId::new(session).map_err(invalid)?;
    let Query(page) = query.map_err(query_error)?;
    json_result(state.inner.messages(&session, page).await)
}

async fn archive_session(
    State(state): State<ServiceState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let session = SessionId::new(session).map_err(invalid)?;
    let mut options: CommandOptions = parse_optional_json(&body)?;
    merge_key(&headers, &mut options.idempotency_key)?;
    {
        let replays = state.archive_replays.lock();
        if let Some(previous) = replays.get(&session) {
            if previous.is_some() && previous == &options.idempotency_key {
                return Ok(Json(Value::Null));
            }
            return Err(HttpError::new(MuzenError::conflict(
                "session is already archived",
            )));
        }
    }
    state
        .inner
        .archive_session(&session, options.clone())
        .await
        .map_err(HttpError::new)?;
    state
        .archive_replays
        .lock()
        .insert(session, options.idempotency_key);
    Ok(Json(Value::Null))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionRunRequest {
    input: AgentInput,
    #[serde(default)]
    options: Option<SingleRunOptions>,
}

async fn session_run(
    State(state): State<ServiceState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let session = SessionId::new(session).map_err(invalid)?;
    let request: SessionRunRequest = parse_json(&body)?;
    let options = request.options.ok_or_else(|| {
        HttpError::new(MuzenError::invalid_input(
            "options is required because run limits have no defaults",
        ))
    })?;
    let mut spec = RunSpec {
        roots: vec![RunRoot::Existing(super::super::ExistingSessionRoot {
            session_id: session,
            input: request.input,
        })],
        limits: options.limits,
        idempotency_key: options.idempotency_key,
        metadata: options.metadata,
    };
    merge_key(&headers, &mut spec.idempotency_key)?;
    json_result(state.inner.start_run(spec).await)
}

async fn start_run(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let mut spec: RunSpec = parse_json(&body)?;
    merge_key(&headers, &mut spec.idempotency_key)?;
    json_result(state.inner.start_run(spec).await)
}

async fn run_snapshot(
    State(state): State<ServiceState>,
    Path(run): Path<String>,
) -> Result<Json<Value>, HttpError> {
    let run = RunId::new(run).map_err(invalid)?;
    json_result(state.inner.run_snapshot(&run).await)
}

async fn run_result(
    State(state): State<ServiceState>,
    Path(run): Path<String>,
) -> Result<Json<Value>, HttpError> {
    let run = RunId::new(run).map_err(invalid)?;
    json_result(state.inner.run_result(&run).await)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsQuery {
    after: Option<u64>,
}

async fn run_events(
    State(state): State<ServiceState>,
    Path(run): Path<String>,
    query: Result<Query<EventsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HttpError> {
    let run = RunId::new(run).map_err(invalid)?;
    let Query(query) = query.map_err(query_error)?;
    let header_after = headers
        .get(LAST_EVENT_ID_HEADER)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| invalid("Last-Event-ID must be decimal"))?
                .parse::<u64>()
                .map_err(|_| invalid("Last-Event-ID must be decimal"))
        })
        .transpose()?;
    if query.after.is_some() && header_after.is_some() && query.after != header_after {
        return Err(HttpError::new(MuzenError::invalid_input(
            "after and Last-Event-ID must agree",
        )));
    }
    state
        .inner
        .run_snapshot(&run)
        .await
        .map_err(HttpError::new)?;
    let events = state.inner.events(
        &run,
        EventOptions {
            after: query.after.or(header_after),
        },
    );
    let events = events.map(|item| {
        item.and_then(|event| {
            Event::default()
                .id(event.sequence.to_string())
                .event("run.event")
                .json_data(event)
                .map_err(|error| {
                    MuzenError::internal(format!("failed to encode SSE event: {error}"))
                })
        })
    });
    Ok(Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(state.keepalive_interval)
            .text("keepalive"),
    ))
}

async fn run_send(
    State(state): State<ServiceState>,
    Path(run): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let run = RunId::new(run).map_err(invalid)?;
    let mut command: SendCommand = parse_json(&body)?;
    merge_key(&headers, &mut command.idempotency_key)?;
    json_result(state.inner.send(&run, command).await)
}

async fn run_spawn(
    State(state): State<ServiceState>,
    Path(run): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let run = RunId::new(run).map_err(invalid)?;
    let mut command: SpawnCommand = parse_json(&body)?;
    merge_key(&headers, &mut command.idempotency_key)?;
    json_result(state.inner.spawn(&run, command).await)
}

async fn run_cancel(
    State(state): State<ServiceState>,
    Path(run): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let run = RunId::new(run).map_err(invalid)?;
    let mut options: CancelOptions = parse_optional_json(&body)?;
    merge_key(&headers, &mut options.idempotency_key)?;
    if let Some(key) = options.idempotency_key.as_ref() {
        if let Some((previous, receipt)) =
            state.cancel_replays.lock().get(&(run.clone(), key.clone()))
        {
            if previous == &options {
                return json_result(Ok(receipt.clone()));
            }
            return Err(HttpError::new(MuzenError::conflict(
                "idempotency key was reused with a different cancel body",
            )));
        }
    }
    let receipt = state
        .inner
        .cancel(&run, options.clone())
        .await
        .map_err(HttpError::new)?;
    if let Some(key) = options.idempotency_key.as_ref() {
        state
            .cancel_replays
            .lock()
            .insert((run, key.clone()), (options, receipt.clone()));
    }
    json_result(Ok(receipt))
}

async fn run_artifact(
    State(state): State<ServiceState>,
    Path((run, artifact)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let run = RunId::new(run).map_err(invalid)?;
    let artifact = ArtifactId::new(artifact).map_err(invalid)?;
    state
        .inner
        .run_snapshot(&run)
        .await
        .map_err(HttpError::new)?;
    let range = headers
        .get(RANGE)
        .map(parse_range)
        .transpose()
        .map_err(HttpError::range)?;
    if let Some(range) = range {
        let bytes = read_all(&state.inner, &artifact).await?;
        let (start, end) = range
            .resolve(bytes.len() as u64)
            .map_err(HttpError::range)?;
        let selected = bytes[start as usize..=end as usize].to_vec();
        let mut response = Response::new(Body::from(selected.clone()));
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response.headers_mut().insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{}", bytes.len()))
                .expect("valid content range"),
        );
        response.headers_mut().insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&selected.len().to_string()).expect("valid content length"),
        );
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        return Ok(response);
    }
    let first = state
        .inner
        .artifact_chunk(&artifact, 0, ARTIFACT_CHUNK_BYTES)
        .await
        .map_err(HttpError::new)?;
    let first_bytes = decode_chunk(&first).map_err(HttpError::new)?;
    let stream = artifact_stream(Arc::clone(&state.inner), artifact, first, first_bytes);
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    Ok(response)
}

fn artifact_stream(
    inner: Arc<dyn RuntimeTransport>,
    artifact: ArtifactId,
    first: ArtifactChunk,
    first_bytes: Vec<u8>,
) -> impl futures::Stream<Item = Result<Vec<u8>, MuzenError>> + Send {
    let state = (
        inner,
        artifact,
        first_bytes.len() as u64,
        first.eof,
        Some(first_bytes),
    );
    stream::try_unfold(
        state,
        |(inner, artifact, offset, eof, pending)| async move {
            if let Some(bytes) = pending {
                if bytes.is_empty() && !eof {
                    return Err(MuzenError::internal(
                        "artifact transport returned an empty non-terminal chunk",
                    ));
                }
                if bytes.is_empty() {
                    return Ok(None);
                }
                return Ok(Some((bytes, (inner, artifact, offset, eof, None))));
            }
            if eof {
                return Ok(None);
            }
            let chunk = inner
                .artifact_chunk(&artifact, offset, ARTIFACT_CHUNK_BYTES)
                .await?;
            let bytes = decode_chunk(&chunk)?;
            if bytes.is_empty() && !chunk.eof {
                return Err(MuzenError::internal(
                    "artifact transport returned an empty non-terminal chunk",
                ));
            }
            let next = offset.saturating_add(bytes.len() as u64);
            Ok(Some((bytes, (inner, artifact, next, chunk.eof, None))))
        },
    )
}

async fn read_all(
    inner: &Arc<dyn RuntimeTransport>,
    artifact: &ArtifactId,
) -> Result<Vec<u8>, HttpError> {
    let mut output = Vec::new();
    loop {
        let chunk = inner
            .artifact_chunk(artifact, output.len() as u64, ARTIFACT_CHUNK_BYTES)
            .await
            .map_err(HttpError::new)?;
        let bytes = decode_chunk(&chunk).map_err(HttpError::new)?;
        if bytes.is_empty() && !chunk.eof {
            return Err(HttpError::new(MuzenError::internal(
                "artifact transport returned an empty non-terminal chunk",
            )));
        }
        output.extend(bytes);
        if chunk.eof {
            return Ok(output);
        }
    }
}

fn decode_chunk(chunk: &ArtifactChunk) -> Result<Vec<u8>, MuzenError> {
    base64::engine::general_purpose::STANDARD
        .decode(&chunk.data)
        .map_err(|_| MuzenError::internal("artifact chunk contains invalid base64"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange {
    From { start: u64, end: Option<u64> },
    Suffix(u64),
}

impl ByteRange {
    fn resolve(self, length: u64) -> Result<(u64, u64), MuzenError> {
        if length == 0 {
            return Err(MuzenError::invalid_input(
                "range is unsatisfiable for an empty artifact",
            ));
        }
        match self {
            Self::From { start, end } => {
                if start >= length || end.is_some_and(|end| end < start) {
                    return Err(MuzenError::invalid_input("range is unsatisfiable"));
                }
                Ok((start, end.unwrap_or(length - 1).min(length - 1)))
            }
            Self::Suffix(count) => {
                if count == 0 {
                    return Err(MuzenError::invalid_input("suffix range must be positive"));
                }
                Ok((length.saturating_sub(count), length - 1))
            }
        }
    }
}

fn parse_range(value: &HeaderValue) -> Result<ByteRange, MuzenError> {
    let value = value
        .to_str()
        .map_err(|_| MuzenError::invalid_input("Range must be ASCII"))?;
    let value = value
        .strip_prefix("bytes=")
        .ok_or_else(|| MuzenError::invalid_input("only bytes ranges are supported"))?;
    if value.contains(',') {
        return Err(MuzenError::invalid_input(
            "multiple byte ranges are not supported",
        ));
    }
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| MuzenError::invalid_input("invalid byte range"))?;
    if start.is_empty() {
        let count = end
            .parse()
            .map_err(|_| MuzenError::invalid_input("invalid suffix byte range"))?;
        return Ok(ByteRange::Suffix(count));
    }
    let start = start
        .parse()
        .map_err(|_| MuzenError::invalid_input("invalid byte range start"))?;
    let end = (!end.is_empty())
        .then(|| {
            end.parse()
                .map_err(|_| MuzenError::invalid_input("invalid byte range end"))
        })
        .transpose()?;
    Ok(ByteRange::From { start, end })
}

fn parse_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, HttpError> {
    serde_json::from_slice(body).map_err(|error| {
        HttpError::new(MuzenError::invalid_input(format!(
            "invalid JSON request body: {error}"
        )))
    })
}

fn parse_optional_json<T: DeserializeOwned + Default>(body: &[u8]) -> Result<T, HttpError> {
    if body.is_empty() {
        Ok(T::default())
    } else {
        parse_json(body)
    }
}

fn header_key(headers: &HeaderMap) -> Result<Option<IdempotencyKey>, HttpError> {
    headers
        .get(IDEMPOTENCY_HEADER)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| invalid("Idempotency-Key must be ASCII"))
                .and_then(|value| IdempotencyKey::new(value.to_owned()).map_err(invalid))
        })
        .transpose()
}

fn merge_key(headers: &HeaderMap, body_key: &mut Option<IdempotencyKey>) -> Result<(), HttpError> {
    let header = header_key(headers)?;
    if header.is_some() && body_key.is_some() && header != *body_key {
        return Err(HttpError::new(MuzenError::invalid_input(
            "Idempotency-Key header and body idempotencyKey must agree",
        )));
    }
    if header.is_some() {
        *body_key = header;
    }
    Ok(())
}

fn json_result<T: Serialize>(result: Result<T, MuzenError>) -> Result<Json<Value>, HttpError> {
    let value = result.map_err(HttpError::new)?;
    serde_json::to_value(value).map(Json).map_err(|error| {
        HttpError::new(MuzenError::internal(format!(
            "serialization failed: {error}"
        )))
    })
}

fn invalid(message: impl Into<String>) -> HttpError {
    HttpError::new(MuzenError::invalid_input(message))
}

fn query_error(error: QueryRejection) -> HttpError {
    invalid(format!("invalid query parameters: {error}"))
}

async fn route_not_found() -> HttpError {
    HttpError::new(MuzenError::not_found("route not found"))
}

struct HttpError {
    error: MuzenError,
    status: StatusCode,
}

impl HttpError {
    fn new(error: MuzenError) -> Self {
        let status = status_for(error.code());
        Self { error, status }
    }

    fn range(error: MuzenError) -> Self {
        Self {
            error,
            status: StatusCode::RANGE_NOT_SATISFIABLE,
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.error)).into_response()
    }
}

fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidInput => StatusCode::BAD_REQUEST,
        ErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
    }
}

#[cfg(test)]
mod unit {
    use super::{parse_range, ByteRange};
    use axum::http::HeaderValue;

    #[test]
    fn parses_supported_byte_range_forms() {
        assert_eq!(
            parse_range(&HeaderValue::from_static("bytes=2-5")).unwrap(),
            ByteRange::From {
                start: 2,
                end: Some(5)
            }
        );
        assert_eq!(
            parse_range(&HeaderValue::from_static("bytes=2-")).unwrap(),
            ByteRange::From {
                start: 2,
                end: None
            }
        );
        assert_eq!(
            parse_range(&HeaderValue::from_static("bytes=-4")).unwrap(),
            ByteRange::Suffix(4)
        );
    }

    #[test]
    fn resolves_and_rejects_ranges() {
        assert_eq!(
            ByteRange::From {
                start: 2,
                end: Some(99)
            }
            .resolve(8)
            .unwrap(),
            (2, 7)
        );
        assert_eq!(ByteRange::Suffix(3).resolve(8).unwrap(), (5, 7));
        assert!(ByteRange::Suffix(0).resolve(8).is_err());
        assert!(ByteRange::From {
            start: 8,
            end: None
        }
        .resolve(8)
        .is_err());
    }
}
