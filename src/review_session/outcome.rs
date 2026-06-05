use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::reviewer::{
    ReviewEvent as InternalReviewEvent, ReviewEventRecord as InternalReviewEventRecord,
};
use crate::runner::{
    RunnerArtifact, RunnerArtifactView as RunnerWireArtifactView, RunnerFinding, RunnerRunResult,
    RunnerSnapshotSummary,
};

use super::{ReviewSessionError, ReviewSource};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewSessionId(pub(super) String);

impl ReviewSessionId {
    pub fn new(id: impl Into<String>) -> Result<Self, ReviewSessionError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(ReviewSessionError::EmptyReviewSessionId);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReviewSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Created,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ReviewStatus {
    pub fn from_runner_status(status: &str) -> Self {
        match status {
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "running" => Self::Running,
            "created" => Self::Created,
            "queued" => Self::Queued,
            "failed" | "partial" => Self::Failed,
            _ => Self::Failed,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCancelOptions {
    #[serde(default)]
    pub reason: Option<String>,
}

impl ReviewCancelOptions {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
        }
    }
}

impl From<String> for ReviewCancelOptions {
    fn from(reason: String) -> Self {
        Self::new(reason)
    }
}

impl From<&str> for ReviewCancelOptions {
    fn from(reason: &str) -> Self {
        Self::new(reason)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewArtifactView {
    Redacted,
    Raw,
}

impl Default for ReviewArtifactView {
    fn default() -> Self {
        Self::Redacted
    }
}

impl From<ReviewArtifactView> for RunnerWireArtifactView {
    fn from(value: ReviewArtifactView) -> Self {
        match value {
            ReviewArtifactView::Redacted => Self::Redacted,
            ReviewArtifactView::Raw => Self::Raw,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifactReadOptions {
    #[serde(default)]
    pub view: ReviewArtifactView,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifactExportOptions {
    #[serde(default)]
    pub view: ReviewArtifactView,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub max_artifacts: Option<usize>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifactExport {
    pub view: ReviewArtifactView,
    pub artifact_count: usize,
    pub total_bytes: usize,
    pub artifacts: Vec<ReviewArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewArtifact {
    pub artifact_id: String,
    pub bytes: usize,
    pub content_hash: String,
    pub content: String,
}

impl ReviewArtifact {
    pub(super) fn from_runner_artifact(artifact: &RunnerArtifact) -> Self {
        Self {
            artifact_id: artifact.artifact_id.clone(),
            bytes: artifact.bytes,
            content_hash: artifact.content_hash.clone(),
            content: artifact.content.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSessionSnapshot {
    pub id: ReviewSessionId,
    pub status: ReviewStatus,
    pub source: ReviewSource,
    #[serde(default)]
    pub result: Option<ReviewResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewResult {
    pub review_id: ReviewSessionId,
    pub session_id: ReviewSessionId,
    pub status: ReviewStatus,
    pub conclusion: ReviewConclusion,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub coverage: ReviewCoverage,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl ReviewResult {
    pub fn from_runner_result(
        review_id: ReviewSessionId,
        source: &ReviewSource,
        result: RunnerRunResult,
    ) -> Self {
        let findings = result
            .findings
            .iter()
            .map(ReviewFinding::from_runner_finding)
            .collect::<Vec<_>>();
        let conclusion = ReviewConclusion::from_findings(&findings);
        let coverage = ReviewCoverage::from_runner_snapshots(&result.snapshots);
        let status = ReviewStatus::from_runner_status(&result.status);
        let mut metadata = BTreeMap::new();
        metadata.insert("runnerRunId".to_string(), json!(result.run_id));
        metadata.insert("runnerStatus".to_string(), json!(result.status));
        metadata.insert("source".to_string(), json!(source.source_key()));
        Self {
            review_id: review_id.clone(),
            session_id: review_id,
            status,
            conclusion,
            summary: review_summary(&result.summary, findings.len()),
            findings,
            coverage,
            metadata,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewConclusion {
    Approved,
    Commented,
    ChangesRequested,
}

impl ReviewConclusion {
    fn from_findings(findings: &[ReviewFinding]) -> Self {
        if findings
            .iter()
            .any(|finding| finding.severity == ReviewFindingSeverity::Error)
        {
            return Self::ChangesRequested;
        }
        if findings.is_empty() {
            Self::Approved
        } else {
            Self::Commented
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFinding {
    pub id: String,
    pub severity: ReviewFindingSeverity,
    pub category: ReviewFindingCategory,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub location: Option<ReviewFindingLocation>,
    #[serde(default)]
    pub suggested_fix: Option<ReviewSuggestedFix>,
    #[serde(default)]
    pub confidence: Option<f32>,
}

impl ReviewFinding {
    fn from_runner_finding(finding: &RunnerFinding) -> Self {
        Self {
            id: finding.id.clone(),
            severity: if finding.publishable {
                ReviewFindingSeverity::Error
            } else {
                ReviewFindingSeverity::Info
            },
            category: ReviewFindingCategory::Other,
            title: finding.title.clone(),
            message: finding.claim.clone(),
            location: None,
            suggested_fix: None,
            confidence: None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingCategory {
    Bug,
    Security,
    Performance,
    Maintainability,
    Style,
    Test,
    Docs,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFindingLocation {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub start_column: Option<usize>,
    #[serde(default)]
    pub end_column: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSuggestedFix {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCoverage {
    pub files_considered: usize,
    pub files_reviewed: usize,
    pub files_skipped: usize,
}

impl ReviewCoverage {
    fn from_runner_snapshots(snapshots: &[RunnerSnapshotSummary]) -> Self {
        let files_considered = snapshots.iter().map(|snapshot| snapshot.files).sum();
        let files_reviewed = snapshots
            .iter()
            .map(|snapshot| snapshot.captured_files)
            .sum();
        Self {
            files_considered,
            files_reviewed,
            files_skipped: files_considered.saturating_sub(files_reviewed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewEvent {
    pub cursor: String,
    #[serde(rename = "type")]
    pub event_type: ReviewEventType,
    pub review_id: ReviewSessionId,
    pub timestamp_utc: String,
    #[serde(default)]
    pub payload: Value,
}

impl ReviewEvent {
    pub fn from_internal_record(record: InternalReviewEventRecord) -> Self {
        let review_id = ReviewSessionId(record.run_id.unwrap_or_else(|| "unknown".to_string()));
        let event_type = ReviewEventType::from_internal(&record.event);
        let payload = serde_json::to_value(&record.event).unwrap_or(Value::Null);
        Self {
            cursor: record.seq.to_string(),
            event_type,
            review_id,
            timestamp_utc: record.timestamp_utc,
            payload,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewEventType {
    #[serde(rename = "session.created")]
    SessionCreated,
    #[serde(rename = "session.queued")]
    SessionQueued,
    #[serde(rename = "session.claimed")]
    SessionClaimed,
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "source.resolved")]
    SourceResolved,
    #[serde(rename = "scope.inferred")]
    ScopeInferred,
    #[serde(rename = "scope.overridden")]
    ScopeOverridden,
    #[serde(rename = "repo.materialized")]
    RepoMaterialized,
    #[serde(rename = "plan.created")]
    PlanCreated,
    #[serde(rename = "agent.started")]
    AgentStarted,
    #[serde(rename = "agent.completed")]
    AgentCompleted,
    #[serde(rename = "tool.started")]
    ToolStarted,
    #[serde(rename = "tool.completed")]
    ToolCompleted,
    #[serde(rename = "finding.created")]
    FindingCreated,
    #[serde(rename = "finding.updated")]
    FindingUpdated,
    #[serde(rename = "review.result_created")]
    ReviewResultCreated,
    #[serde(rename = "session.completed")]
    SessionCompleted,
    #[serde(rename = "session.failed")]
    SessionFailed,
    #[serde(rename = "session.cancelled")]
    SessionCancelled,
    #[serde(rename = "runner.event")]
    RunnerEvent,
}

impl ReviewEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreated => "session.created",
            Self::SessionQueued => "session.queued",
            Self::SessionClaimed => "session.claimed",
            Self::SessionStarted => "session.started",
            Self::SourceResolved => "source.resolved",
            Self::ScopeInferred => "scope.inferred",
            Self::ScopeOverridden => "scope.overridden",
            Self::RepoMaterialized => "repo.materialized",
            Self::PlanCreated => "plan.created",
            Self::AgentStarted => "agent.started",
            Self::AgentCompleted => "agent.completed",
            Self::ToolStarted => "tool.started",
            Self::ToolCompleted => "tool.completed",
            Self::FindingCreated => "finding.created",
            Self::FindingUpdated => "finding.updated",
            Self::ReviewResultCreated => "review.result_created",
            Self::SessionCompleted => "session.completed",
            Self::SessionFailed => "session.failed",
            Self::SessionCancelled => "session.cancelled",
            Self::RunnerEvent => "runner.event",
        }
    }

    fn from_internal(event: &InternalReviewEvent) -> Self {
        match event {
            InternalReviewEvent::RunStarted { .. } => Self::SessionStarted,
            InternalReviewEvent::RepoManifestCompleted { .. } => Self::ScopeInferred,
            InternalReviewEvent::SnapshotStarted { .. } => Self::RunnerEvent,
            InternalReviewEvent::SessionStarted { .. } => Self::AgentStarted,
            InternalReviewEvent::ModelStarted { .. } => Self::RunnerEvent,
            InternalReviewEvent::ModelCompleted { .. } => Self::RunnerEvent,
            InternalReviewEvent::ToolBatchStarted { .. } => Self::ToolStarted,
            InternalReviewEvent::ToolCallCompleted { .. }
            | InternalReviewEvent::ToolCallDenied { .. } => Self::ToolCompleted,
            InternalReviewEvent::ArtifactCreated { .. } => Self::RunnerEvent,
            InternalReviewEvent::FindingRecorded { .. } => Self::FindingCreated,
            InternalReviewEvent::SearchBatchCompleted { .. } => Self::RunnerEvent,
            InternalReviewEvent::SessionFinished { .. } => Self::AgentCompleted,
            InternalReviewEvent::SnapshotFinished { .. } => Self::RepoMaterialized,
            InternalReviewEvent::RunFinished { status } if status == "completed" => {
                Self::SessionCompleted
            }
            InternalReviewEvent::RunFinished { status } if status == "cancelled" => {
                Self::SessionCancelled
            }
            InternalReviewEvent::RunFinished { .. } => Self::SessionFailed,
        }
    }
}

fn review_summary(summary: &crate::runner::RunnerRunSummary, findings: usize) -> String {
    format!(
        "Review completed {}/{} session(s), produced {} finding(s), used {} model call(s), {} tool call(s), and {} total token(s).",
        summary.completed_sessions,
        summary.sessions,
        findings,
        summary.model_calls,
        summary.tool_calls,
        summary.total_tokens
    )
}
