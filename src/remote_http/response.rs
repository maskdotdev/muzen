use std::collections::BTreeMap;

use serde::Serialize;

use crate::review_sessions::{
    Muzen, MuzenProject, ReviewEvent, ReviewSessionError, ReviewSessionId, WebhookReviewDelivery,
};

pub const HTTP_STATUS_OK: u16 = 200;
pub const HTTP_STATUS_ACCEPTED: u16 = 202;
pub const HTTP_STATUS_NO_CONTENT: u16 = 204;
pub const HTTP_STATUS_BAD_REQUEST: u16 = 400;
pub const HTTP_STATUS_NOT_FOUND: u16 = 404;
pub const HTTP_STATUS_METHOD_NOT_ALLOWED: u16 = 405;
pub const CONTENT_TYPE_JSON: &str = "application/json";
pub const CONTENT_TYPE_EVENT_STREAM: &str = "text/event-stream";
pub const CONTENT_TYPE_TEXT: &str = "text/plain; charset=utf-8";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewHttpResponse {
    pub status_code: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl ReviewHttpResponse {
    pub fn json<T: Serialize>(status_code: u16, body: &T) -> Result<Self, ReviewSessionError> {
        let body = serde_json::to_string(body).map_err(|error| {
            ReviewSessionError::Http(format!("failed to serialize JSON response: {error}"))
        })?;
        Ok(Self::with_body(status_code, CONTENT_TYPE_JSON, body))
    }

    pub fn empty(status_code: u16) -> Self {
        Self {
            status_code,
            headers: BTreeMap::new(),
            body: String::new(),
        }
    }

    pub fn event_stream(stream: &ReviewSseStream) -> Self {
        let mut response =
            Self::with_body(HTTP_STATUS_OK, CONTENT_TYPE_EVENT_STREAM, stream.encode());
        response
            .headers
            .insert("Cache-Control".to_string(), "no-cache".to_string());
        response
            .headers
            .insert("Connection".to_string(), "keep-alive".to_string());
        response
            .headers
            .insert("X-Accel-Buffering".to_string(), "no".to_string());
        response
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn with_body(status_code: u16, content_type: &str, body: String) -> Self {
        let mut headers = BTreeMap::new();
        headers.insert("Content-Type".to_string(), content_type.to_string());
        Self {
            status_code,
            headers,
            body,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSseStream {
    pub frames: Vec<ReviewSseFrame>,
}

impl ReviewSseStream {
    pub fn from_events(events: &[ReviewEvent]) -> Result<Self, ReviewSessionError> {
        let frames = events
            .iter()
            .map(ReviewSseFrame::from_event)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { frames })
    }

    pub fn encode(&self) -> String {
        self.frames
            .iter()
            .map(ReviewSseFrame::encode)
            .collect::<String>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSseFrame {
    pub id: String,
    pub event: String,
    pub data: String,
}

impl ReviewSseFrame {
    pub fn from_event(event: &ReviewEvent) -> Result<Self, ReviewSessionError> {
        let data = serde_json::to_string(event).map_err(|error| {
            ReviewSessionError::Http(format!("failed to serialize SSE event data: {error}"))
        })?;
        Ok(Self {
            id: event.cursor.clone(),
            event: event.event_type.as_str().to_string(),
            data,
        })
    }

    pub fn encode(&self) -> String {
        let mut output = String::new();
        push_sse_field(&mut output, "id", &self.id);
        push_sse_field(&mut output, "event", &self.event);
        push_sse_field(&mut output, "data", &self.data);
        output.push('\n');
        output
    }
}

impl Muzen {
    pub async fn review_events_response(
        &self,
        review_id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<ReviewHttpResponse, ReviewSessionError> {
        review_events_response(self.store.as_ref(), review_id, after).await
    }

    pub async fn review_events_sse_response(
        &self,
        review_id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<ReviewHttpResponse, ReviewSessionError> {
        review_events_sse_response(self.store.as_ref(), review_id, after).await
    }
}

impl MuzenProject {
    pub async fn review_events_response(
        &self,
        review_id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<ReviewHttpResponse, ReviewSessionError> {
        review_events_response(self.store.as_ref(), review_id, after).await
    }

    pub async fn review_events_sse_response(
        &self,
        review_id: &ReviewSessionId,
        after: Option<&str>,
    ) -> Result<ReviewHttpResponse, ReviewSessionError> {
        review_events_sse_response(self.store.as_ref(), review_id, after).await
    }
}

impl WebhookReviewDelivery {
    pub fn http_response(&self) -> Result<ReviewHttpResponse, ReviewSessionError> {
        let status_code = match self {
            Self::ReviewCreated { .. } | Self::Ignored { .. } => HTTP_STATUS_ACCEPTED,
            Self::ReviewDeduped { .. } => HTTP_STATUS_OK,
        };
        ReviewHttpResponse::json(status_code, &self.response_body())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewEventsBody {
    events: Vec<ReviewEvent>,
}

async fn review_events_response(
    store: &dyn crate::review_sessions::ReviewSessionStore,
    review_id: &ReviewSessionId,
    after: Option<&str>,
) -> Result<ReviewHttpResponse, ReviewSessionError> {
    let events = store.events_after(review_id, after).await?;
    ReviewHttpResponse::json(HTTP_STATUS_OK, &ReviewEventsBody { events })
}

async fn review_events_sse_response(
    store: &dyn crate::review_sessions::ReviewSessionStore,
    review_id: &ReviewSessionId,
    after: Option<&str>,
) -> Result<ReviewHttpResponse, ReviewSessionError> {
    let events = store.events_after(review_id, after).await?;
    let stream = ReviewSseStream::from_events(&events)?;
    Ok(ReviewHttpResponse::event_stream(&stream))
}

fn push_sse_field(output: &mut String, name: &str, value: &str) {
    for line in value.split('\n') {
        output.push_str(name);
        output.push_str(": ");
        output.push_str(line.trim_end_matches('\r'));
        output.push('\n');
    }
}
