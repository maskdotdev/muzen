//! Minimal server-sent-events reader over a streaming HTTP response. Both
//! streaming model protocols (OpenAI chat completions and Anthropic
//! messages) frame every event as a single complete JSON document on one
//! `data:` line, so the reader yields raw `data:` payloads and leaves event
//! semantics to the caller.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::runtime::contracts::{RuntimeError, RuntimeResult};

/// A stalled stream is treated like a transient provider failure: nothing on
/// the wire for this long fails the attempt as retryable.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Streaming requests override the client-wide timeout (which would cut off
/// long generations); the per-turn timeout in the retry layer is the real
/// bound.
pub(crate) const STREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(3_600);

/// Next event payload, racing the cancel token (true mid-stream
/// cancellation) and the idle timeout.
pub(crate) async fn next_streaming_data(
    sse: &mut SseStream,
    cancel: &CancellationToken,
) -> RuntimeResult<Option<String>> {
    tokio::select! {
        _ = cancel.cancelled() => Err(RuntimeError::Cancelled),
        next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, sse.next_data()) => {
            next.map_err(|_| RuntimeError::Provider {
                status: None,
                retryable: true,
            })?
        }
    }
}

pub(crate) struct SseStream {
    response: reqwest::Response,
    buffer: Vec<u8>,
    /// Everything read from the wire, kept so callers can fall back to
    /// parsing the body as a plain JSON response when a provider ignored
    /// `stream: true` and never produced an SSE event.
    raw: Vec<u8>,
    yielded_events: bool,
}

impl SseStream {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            raw: Vec::new(),
            yielded_events: false,
        }
    }

    /// Next `data:` payload, or `None` when the stream ends.
    pub(crate) async fn next_data(&mut self) -> RuntimeResult<Option<String>> {
        loop {
            if let Some(line) = self.take_line() {
                if let Some(payload) = data_payload(&line) {
                    self.yielded_events = true;
                    return Ok(Some(payload));
                }
                continue;
            }
            let chunk = self
                .response
                .chunk()
                .await
                .map_err(|_| RuntimeError::Provider {
                    status: None,
                    retryable: true,
                })?;
            let Some(chunk) = chunk else {
                // Trailing data line without a final newline still counts.
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let line = String::from_utf8_lossy(&self.buffer).into_owned();
                self.buffer.clear();
                if let Some(payload) = data_payload(&line) {
                    self.yielded_events = true;
                    return Ok(Some(payload));
                }
                return Ok(None);
            };
            self.buffer.extend_from_slice(&chunk);
            self.raw.extend_from_slice(&chunk);
        }
    }

    pub(crate) fn yielded_events(&self) -> bool {
        self.yielded_events
    }

    pub(crate) fn into_raw_body(self) -> Vec<u8> {
        self.raw
    }

    fn take_line(&mut self) -> Option<String> {
        let newline = self.buffer.iter().position(|byte| *byte == b'\n')?;
        let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(String::from_utf8_lossy(&line).into_owned())
    }
}

fn data_payload(line: &str) -> Option<String> {
    let payload = line.strip_prefix("data:")?;
    Some(payload.strip_prefix(' ').unwrap_or(payload).to_string())
}
