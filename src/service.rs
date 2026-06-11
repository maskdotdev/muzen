use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use crate::review_session::{
    Muzen, ReviewHttpRequest, ReviewHttpResponse, ReviewHttpRouter, ReviewHttpRouterOptions,
};

#[derive(Debug, Clone)]
pub struct MuzenHttpService {
    router: Arc<ReviewHttpRouter>,
}

impl MuzenHttpService {
    pub fn new(router: ReviewHttpRouter) -> Self {
        Self {
            router: Arc::new(router),
        }
    }

    pub fn in_memory(options: ReviewHttpRouterOptions) -> Self {
        Self::new(ReviewHttpRouter::with_options(Muzen::new(), options))
    }

    pub fn into_router(self) -> Router {
        Router::new()
            .fallback(any(handle_muzen_request))
            .with_state(self.router)
    }
}

pub async fn serve(addr: SocketAddr, service: MuzenHttpService) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind Muzen HTTP service on {addr}"))?;
    axum::serve(listener, service.into_router())
        .await
        .context("Muzen HTTP service stopped with an error")
}

async fn handle_muzen_request(
    State(router): State<Arc<ReviewHttpRouter>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    review_response_to_axum(
        router
            .handle(review_request_from_axum(method, uri, headers, body))
            .await,
    )
}

fn review_request_from_axum(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ReviewHttpRequest {
    let target = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or_else(|| uri.path());
    let mut request = ReviewHttpRequest::new(method.as_str(), target).body(body.to_vec());
    for (name, value) in headers {
        let Some(name) = name else {
            continue;
        };
        if let Ok(value) = value.to_str() {
            request = request.header(name.as_str(), value.to_string());
        }
    }
    request
}

fn review_response_to_axum(response: ReviewHttpResponse) -> Response {
    let status = StatusCode::from_u16(response.status_code).unwrap_or(StatusCode::BAD_REQUEST);
    let mut builder = axum::http::Response::builder().status(status);
    for (name, value) in response.headers {
        let Ok(name) = HeaderName::try_from(name) else {
            continue;
        };
        let Ok(value) = HeaderValue::try_from(value) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::from(response.body))
        .unwrap_or_else(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build Muzen HTTP response: {error}"),
            )
                .into_response()
        })
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn axum_adapter_serves_review_creation_route() {
        let app = MuzenHttpService::in_memory(ReviewHttpRouterOptions::default()).into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/reviews")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "source": {
                        "type": "local",
                        "repo": ".",
                        "changedFiles": ["Cargo.toml"]
                    },
                    "options": {
                        "dedupe": "source"
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(payload["review"]["id"], json!("review-1"));
        assert_eq!(payload["review"]["status"], json!("queued"));
    }
}
