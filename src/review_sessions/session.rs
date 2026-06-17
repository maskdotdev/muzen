use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::reviewer_kernel::system::timestamp_utc;

use super::{
    EffectiveConfigSnapshot, ReviewArtifact, ReviewArtifactExport, ReviewArtifactExportOptions,
    ReviewArtifactReadOptions, ReviewArtifactView, ReviewCancelOptions, ReviewEvent,
    ReviewEventType, ReviewOptions, ReviewResult, ReviewSessionError, ReviewSessionId,
    ReviewSessionRecord, ReviewSessionSnapshot, ReviewSource, ReviewSourceLike, ReviewStatus,
};

#[derive(Debug, Clone)]
pub struct ReviewSession {
    id: ReviewSessionId,
    status: ReviewStatus,
    source: ReviewSource,
    options: ReviewOptions,
    user_id: Option<String>,
    events: Vec<ReviewEvent>,
    result: Option<ReviewResult>,
    redacted_artifacts: Vec<ReviewArtifact>,
    raw_artifacts: Vec<ReviewArtifact>,
    config_snapshot: Option<EffectiveConfigSnapshot>,
}

impl ReviewSession {
    pub(super) async fn execute_local_async(
        id: ReviewSessionId,
        input: CreateReviewSessionInput,
    ) -> Result<Self, ReviewSessionError> {
        tokio::task::spawn_blocking(move || Self::execute_local(id, input))
            .await
            .map_err(|error| ReviewSessionError::Runner(error.to_string()))?
    }

    pub(super) fn execute_local(
        id: ReviewSessionId,
        input: CreateReviewSessionInput,
    ) -> Result<Self, ReviewSessionError> {
        let source = input.source.clone();
        let options = input.options.clone();
        let config_snapshot = input.options.config_snapshot.clone();
        let user_id = input.options.user_id.clone();
        let executed = super::execution::execute_local_review(&id, &source, &options)?;
        let result = executed.result;
        let status = result.status;
        Ok(Self {
            id,
            status,
            source,
            options,
            user_id,
            events: executed.events,
            result: Some(result),
            redacted_artifacts: executed.redacted_artifacts,
            raw_artifacts: executed.raw_artifacts,
            config_snapshot,
        })
    }

    pub(super) fn queued(id: ReviewSessionId, input: CreateReviewSessionInput) -> Self {
        let source = input.source;
        let options = input.options;
        let user_id = options.user_id.clone();
        let config_snapshot = options.config_snapshot.clone();
        let now = timestamp_utc();
        Self {
            id: id.clone(),
            status: ReviewStatus::Queued,
            source,
            options,
            user_id,
            events: vec![ReviewEvent {
                cursor: "1".to_string(),
                event_type: ReviewEventType::SessionQueued,
                review_id: id,
                timestamp_utc: now,
                payload: json!({}),
            }],
            result: None,
            redacted_artifacts: Vec::new(),
            raw_artifacts: Vec::new(),
            config_snapshot,
        }
    }

    pub(crate) fn from_record(record: ReviewSessionRecord) -> Self {
        Self {
            id: record.id,
            status: record.status,
            source: record.source,
            options: record.options,
            user_id: record.user_id,
            events: record.events,
            result: record.result,
            redacted_artifacts: record.redacted_artifacts,
            raw_artifacts: record.raw_artifacts,
            config_snapshot: record.config_snapshot,
        }
    }

    pub(super) fn to_record(
        &self,
        dedupe_key: Option<String>,
        project_id: Option<String>,
    ) -> ReviewSessionRecord {
        ReviewSessionRecord {
            id: self.id.clone(),
            project_id,
            user_id: self.user_id.clone(),
            status: self.status,
            source: self.source.clone(),
            options: self.options.clone(),
            result: self.result.clone(),
            events: self.events.clone(),
            redacted_artifacts: self.redacted_artifacts.clone(),
            raw_artifacts: self.raw_artifacts.clone(),
            config_snapshot: self.config_snapshot.clone(),
            attempt: 0,
            run_after_unix_seconds: 0,
            lease: None,
            cancellation: None,
            last_error: None,
            dedupe_key,
            created_at_utc: timestamp_utc(),
            updated_at_utc: timestamp_utc(),
        }
    }

    pub(super) fn persisted_execution_parts(
        &self,
    ) -> (Vec<ReviewEvent>, Vec<ReviewArtifact>, Vec<ReviewArtifact>) {
        (
            self.events.clone(),
            self.redacted_artifacts.clone(),
            self.raw_artifacts.clone(),
        )
    }

    pub fn id(&self) -> &ReviewSessionId {
        &self.id
    }

    pub fn status(&self) -> ReviewStatus {
        self.status
    }

    pub fn source(&self) -> &ReviewSource {
        &self.source
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn config_snapshot(&self) -> Option<&EffectiveConfigSnapshot> {
        self.config_snapshot.as_ref()
    }

    pub fn subscribe(&self, mut listener: impl FnMut(&ReviewEvent)) {
        for event in &self.events {
            listener(event);
        }
    }

    pub fn events(&self) -> impl Iterator<Item = &ReviewEvent> {
        self.events.iter()
    }

    pub fn event_records(&self) -> &[ReviewEvent] {
        &self.events
    }

    pub fn wait(&self) -> Result<ReviewResult, ReviewSessionError> {
        self.result
            .clone()
            .ok_or_else(|| ReviewSessionError::ResultUnavailable {
                review_id: self.id.clone(),
            })
    }

    pub fn result(&self) -> Option<&ReviewResult> {
        self.result.as_ref()
    }

    pub fn read_artifact(
        &self,
        artifact_id: &str,
        options: ReviewArtifactReadOptions,
    ) -> Result<ReviewArtifact, ReviewSessionError> {
        self.artifacts_for_view(options.view)
            .iter()
            .find(|artifact| artifact.artifact_id == artifact_id)
            .cloned()
            .ok_or_else(|| ReviewSessionError::UnknownArtifactId {
                review_id: self.id.clone(),
                artifact_id: artifact_id.to_string(),
            })
    }

    pub fn export_artifacts(
        &self,
        options: ReviewArtifactExportOptions,
    ) -> Result<ReviewArtifactExport, ReviewSessionError> {
        let mut artifacts = self.artifacts_for_view(options.view).to_vec();
        if !options.artifact_ids.is_empty() {
            artifacts.retain(|artifact| {
                options
                    .artifact_ids
                    .iter()
                    .any(|artifact_id| artifact_id == &artifact.artifact_id)
            });
        }
        let total_bytes = artifacts
            .iter()
            .map(|artifact| artifact.bytes)
            .sum::<usize>();
        if options
            .max_artifacts
            .is_some_and(|max_artifacts| artifacts.len() > max_artifacts)
        {
            return Err(ReviewSessionError::ArtifactLimitExceeded {
                kind: "artifact_count".to_string(),
            });
        }
        if options
            .max_bytes
            .is_some_and(|max_bytes| total_bytes > max_bytes)
        {
            return Err(ReviewSessionError::ArtifactLimitExceeded {
                kind: "artifact_bytes".to_string(),
            });
        }
        Ok(ReviewArtifactExport {
            view: options.view,
            artifact_count: artifacts.len(),
            total_bytes,
            artifacts,
        })
    }

    pub fn cancel(
        &mut self,
        options: impl Into<ReviewCancelOptions>,
    ) -> Result<(), ReviewSessionError> {
        if self.status.is_terminal() {
            return Ok(());
        }
        let options = options.into();
        self.status = ReviewStatus::Cancelled;
        self.events.push(ReviewEvent {
            cursor: (self.events.len() + 1).to_string(),
            event_type: ReviewEventType::SessionCancelled,
            review_id: self.id.clone(),
            timestamp_utc: timestamp_utc(),
            payload: json!({ "reason": options.reason }),
        });
        Ok(())
    }

    pub fn refresh(&self) -> ReviewSessionSnapshot {
        ReviewSessionSnapshot {
            id: self.id.clone(),
            status: self.status,
            source: self.source.clone(),
            result: self.result.clone(),
        }
    }

    fn artifacts_for_view(&self, view: ReviewArtifactView) -> &[ReviewArtifact] {
        match view {
            ReviewArtifactView::Redacted => &self.redacted_artifacts,
            ReviewArtifactView::Raw => &self.raw_artifacts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReviewSessionInput {
    pub source: ReviewSource,
    #[serde(default)]
    pub options: ReviewOptions,
}

impl CreateReviewSessionInput {
    pub fn new(source: impl Into<ReviewSourceLike>) -> Result<Self, ReviewSessionError> {
        Ok(Self {
            source: source.into().resolve()?,
            options: ReviewOptions::default(),
        })
    }

    pub fn with_options(
        source: impl Into<ReviewSourceLike>,
        options: ReviewOptions,
    ) -> Result<Self, ReviewSessionError> {
        Ok(Self {
            source: source.into().resolve()?,
            options,
        })
    }
}
