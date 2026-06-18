use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;

use super::{MuzenProject, ReviewOptions, ReviewSession, ReviewSessionError, ReviewSource};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebhookHeaders {
    values: BTreeMap<String, String>,
}

impl WebhookHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl AsRef<str>, value: impl Into<String>) {
        self.values
            .insert(normalize_header_name(name.as_ref()), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&normalize_header_name(name))
            .map(String::as_str)
    }
}

impl<const N: usize> From<[(&str, &str); N]> for WebhookHeaders {
    fn from(headers: [(&str, &str); N]) -> Self {
        let mut result = Self::new();
        for (name, value) in headers {
            result.insert(name, value);
        }
        result
    }
}

impl FromIterator<(String, String)> for WebhookHeaders {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        let mut result = Self::new();
        for (name, value) in iter {
            result.insert(name, value);
        }
        result
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookReviewOptions {
    #[serde(default)]
    pub review_options: ReviewOptions,
}

impl WebhookReviewOptions {
    pub fn new(review_options: ReviewOptions) -> Self {
        Self { review_options }
    }
}

impl From<ReviewOptions> for WebhookReviewOptions {
    fn from(review_options: ReviewOptions) -> Self {
        Self::new(review_options)
    }
}

#[derive(Debug, Clone)]
pub enum WebhookReviewDelivery {
    ReviewCreated {
        review: ReviewSession,
        delivery_id: String,
    },
    ReviewDeduped {
        review: ReviewSession,
        delivery_id: String,
    },
    Ignored {
        reason: String,
        delivery_id: Option<String>,
    },
}

impl WebhookReviewDelivery {
    pub fn response_body(&self) -> Value {
        match self {
            Self::ReviewCreated {
                review,
                delivery_id,
            } => json!({
                "type": "review_created",
                "deliveryId": delivery_id,
                "reviewId": review.id().as_str(),
                "status": review.status()
            }),
            Self::ReviewDeduped {
                review,
                delivery_id,
            } => json!({
                "type": "review_deduped",
                "deliveryId": delivery_id,
                "reviewId": review.id().as_str(),
                "status": review.status()
            }),
            Self::Ignored {
                reason,
                delivery_id,
            } => json!({
                "type": "ignored",
                "deliveryId": delivery_id,
                "reason": reason
            }),
        }
    }
}

impl MuzenProject {
    pub(crate) async fn handle_github_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        secret: Option<&str>,
        options: impl Into<WebhookReviewOptions>,
    ) -> Result<WebhookReviewDelivery, ReviewSessionError> {
        if let Some(secret) = secret {
            let signature = required_header(headers, "x-hub-signature-256")?;
            verify_github_webhook_signature(secret, body, signature)?;
        }
        let event = headers.get("x-github-event").unwrap_or_default();
        let delivery_id = required_header(headers, "x-github-delivery")?.to_string();
        let mapped = map_github_webhook_source(event, body)?;
        let Some(mapped) = mapped else {
            return Ok(WebhookReviewDelivery::Ignored {
                reason: format!("unsupported GitHub webhook event `{event}`"),
                delivery_id: Some(delivery_id),
            });
        };
        if !is_supported_github_action(mapped.action.as_deref()) {
            return Ok(WebhookReviewDelivery::Ignored {
                reason: format!(
                    "unsupported GitHub pull_request action `{}`",
                    mapped.action.unwrap_or_else(|| "unknown".to_string())
                ),
                delivery_id: Some(delivery_id),
            });
        }
        self.schedule_webhook_review(
            mapped.source,
            "github",
            event,
            mapped.action.as_deref(),
            mapped.head_sha.as_deref(),
            delivery_id,
            options.into(),
        )
        .await
    }

    pub(crate) async fn handle_gitlab_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        secret: Option<&str>,
        options: impl Into<WebhookReviewOptions>,
    ) -> Result<WebhookReviewDelivery, ReviewSessionError> {
        if let Some(secret) = secret {
            let token = required_header(headers, "x-gitlab-token")?;
            verify_gitlab_webhook_token(secret, token)?;
        }
        let event = headers.get("x-gitlab-event").unwrap_or_default();
        let delivery_id = headers
            .get("x-gitlab-event-uuid")
            .or_else(|| headers.get("x-request-id"))
            .unwrap_or("unknown")
            .to_string();
        let mapped = map_gitlab_webhook_source(body)?;
        let Some(mapped) = mapped else {
            return Ok(WebhookReviewDelivery::Ignored {
                reason: format!("unsupported GitLab webhook event `{event}`"),
                delivery_id: Some(delivery_id),
            });
        };
        if !is_supported_gitlab_action(mapped.action.as_deref()) {
            return Ok(WebhookReviewDelivery::Ignored {
                reason: format!(
                    "unsupported GitLab merge_request action `{}`",
                    mapped.action.unwrap_or_else(|| "unknown".to_string())
                ),
                delivery_id: Some(delivery_id),
            });
        }
        self.schedule_webhook_review(
            mapped.source,
            "gitlab",
            event,
            mapped.action.as_deref(),
            mapped.head_sha.as_deref(),
            delivery_id,
            options.into(),
        )
        .await
    }

    async fn schedule_webhook_review(
        &self,
        source: ReviewSource,
        provider: &str,
        event: &str,
        action: Option<&str>,
        head_sha: Option<&str>,
        delivery_id: String,
        options: WebhookReviewOptions,
    ) -> Result<WebhookReviewDelivery, ReviewSessionError> {
        let mut review_options = options.review_options;
        review_options
            .metadata
            .insert("webhook.provider".to_string(), json!(provider));
        review_options
            .metadata
            .insert("webhook.deliveryId".to_string(), json!(delivery_id.clone()));
        if !event.is_empty() {
            review_options
                .metadata
                .insert("webhook.event".to_string(), json!(event));
        }
        if let Some(action) = action {
            review_options
                .metadata
                .insert("webhook.action".to_string(), json!(action));
        }
        if let Some(head_sha) = head_sha
            .map(str::trim)
            .filter(|head_sha| !head_sha.is_empty())
        {
            review_options
                .metadata
                .insert("source.headSha".to_string(), json!(head_sha));
        }
        let dedupe_key = review_options.dedupe_key(&source);
        let was_deduped = if let Some(key) = &dedupe_key {
            self.store.get_by_dedupe_key(key).await?.is_some()
        } else {
            false
        };
        let review = self
            .schedule_review_with_options(source, review_options)
            .await?;
        if was_deduped {
            Ok(WebhookReviewDelivery::ReviewDeduped {
                review,
                delivery_id,
            })
        } else {
            Ok(WebhookReviewDelivery::ReviewCreated {
                review,
                delivery_id,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookMappedSource {
    pub source: ReviewSource,
    pub action: Option<String>,
    pub head_sha: Option<String>,
}

pub fn verify_github_webhook_signature(
    secret: &str,
    body: &[u8],
    signature: &str,
) -> Result<(), ReviewSessionError> {
    let expected = github_webhook_signature(secret, body)?;
    if constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
        Ok(())
    } else {
        Err(ReviewSessionError::Webhook(
            "GitHub webhook signature verification failed".to_string(),
        ))
    }
}

pub fn github_webhook_signature(secret: &str, body: &[u8]) -> Result<String, ReviewSessionError> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| ReviewSessionError::Webhook("invalid GitHub webhook secret".to_string()))?;
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    Ok(format!("sha256={}", lower_hex(&digest)))
}

pub fn verify_gitlab_webhook_token(secret: &str, token: &str) -> Result<(), ReviewSessionError> {
    if constant_time_eq(secret.as_bytes(), token.as_bytes()) {
        Ok(())
    } else {
        Err(ReviewSessionError::Webhook(
            "GitLab webhook token verification failed".to_string(),
        ))
    }
}

pub fn map_github_webhook_source(
    event: &str,
    body: &[u8],
) -> Result<Option<WebhookMappedSource>, ReviewSessionError> {
    if event != "pull_request" {
        return Ok(None);
    }
    let payload = parse_payload(body)?;
    let Some(number) = payload
        .get("pull_request")
        .and_then(|pull_request| pull_request.get("number"))
        .and_then(Value::as_u64)
        .or_else(|| payload.get("number").and_then(Value::as_u64))
    else {
        return Err(ReviewSessionError::Webhook(
            "GitHub pull_request webhook missing pull request number".to_string(),
        ));
    };
    let Some((owner, repo)) = github_repository_parts(&payload) else {
        return Err(ReviewSessionError::Webhook(
            "GitHub pull_request webhook missing repository identity".to_string(),
        ));
    };
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_string);
    let head_sha = payload
        .get("pull_request")
        .and_then(|pull_request| pull_request.get("head"))
        .and_then(|head| head.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(WebhookMappedSource {
        source: ReviewSource::github_pull_request(owner, repo, number)?,
        action,
        head_sha,
    }))
}

pub fn map_gitlab_webhook_source(
    body: &[u8],
) -> Result<Option<WebhookMappedSource>, ReviewSessionError> {
    let payload = parse_payload(body)?;
    let object_kind = payload
        .get("object_kind")
        .or_else(|| payload.get("event_type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if object_kind != "merge_request" {
        return Ok(None);
    }
    let attributes = payload.get("object_attributes").ok_or_else(|| {
        ReviewSessionError::Webhook(
            "GitLab merge_request webhook missing object_attributes".to_string(),
        )
    })?;
    let number = attributes
        .get("iid")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ReviewSessionError::Webhook(
                "GitLab merge_request webhook missing merge request iid".to_string(),
            )
        })?;
    let project = payload.get("project").ok_or_else(|| {
        ReviewSessionError::Webhook("GitLab merge_request webhook missing project".to_string())
    })?;
    let slug = project
        .get("path_with_namespace")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ReviewSessionError::Webhook(
                "GitLab merge_request webhook missing project path_with_namespace".to_string(),
            )
        })?;
    let Some((owner, repo)) = slug.rsplit_once('/') else {
        return Err(ReviewSessionError::Webhook(
            "GitLab project path_with_namespace must include namespace and repository".to_string(),
        ));
    };
    let action = attributes
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_string);
    let head_sha = attributes
        .get("last_commit")
        .and_then(|last_commit| last_commit.get("id"))
        .or_else(|| {
            payload
                .get("last_commit")
                .and_then(|last_commit| last_commit.get("id"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(WebhookMappedSource {
        source: ReviewSource::gitlab_merge_request(owner, repo, number)?,
        action,
        head_sha,
    }))
}

fn parse_payload(body: &[u8]) -> Result<Value, ReviewSessionError> {
    serde_json::from_slice(body)
        .map_err(|error| ReviewSessionError::Webhook(format!("invalid webhook JSON: {error}")))
}

fn github_repository_parts(payload: &Value) -> Option<(String, String)> {
    let repository = payload.get("repository")?;
    if let Some(full_name) = repository.get("full_name").and_then(Value::as_str) {
        let (owner, repo) = full_name.split_once('/')?;
        return Some((owner.to_string(), repo.to_string()));
    }
    let owner = repository
        .get("owner")
        .and_then(|owner| owner.get("login"))
        .and_then(Value::as_str)?;
    let repo = repository.get("name").and_then(Value::as_str)?;
    Some((owner.to_string(), repo.to_string()))
}

fn is_supported_github_action(action: Option<&str>) -> bool {
    matches!(
        action,
        Some("opened" | "reopened" | "synchronize" | "edited" | "ready_for_review")
    )
}

fn is_supported_gitlab_action(action: Option<&str>) -> bool {
    matches!(action, Some("open" | "reopen" | "update"))
}

fn required_header<'a>(
    headers: &'a WebhookHeaders,
    name: &str,
) -> Result<&'a str, ReviewSessionError> {
    headers
        .get(name)
        .ok_or_else(|| ReviewSessionError::Webhook(format!("missing webhook header `{name}`")))
}

fn normalize_header_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}
