use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{
    CreateReviewSessionInput, HostConfiguration, ReviewAttemptFailure, ReviewSession,
    ReviewSessionError, ReviewSessionStore, ReviewStatus,
};

#[derive(Clone)]
pub struct ReviewWorker {
    worker_id: String,
    store: Arc<dyn ReviewSessionStore>,
    host_config: HostConfiguration,
}

impl std::fmt::Debug for ReviewWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReviewWorker")
            .field("worker_id", &self.worker_id)
            .field("host_config", &self.host_config)
            .finish_non_exhaustive()
    }
}

impl ReviewWorker {
    pub(crate) fn new(
        worker_id: impl Into<String>,
        store: Arc<dyn ReviewSessionStore>,
        host_config: HostConfiguration,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            store,
            host_config,
        }
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub async fn run_once(
        &self,
        max_sessions: usize,
    ) -> Result<ReviewWorkerRun, ReviewSessionError> {
        let claims = self
            .store
            .claim_ready(
                self.host_config
                    .scheduling
                    .claim_options(self.worker_id.clone(), max_sessions),
            )
            .await?;
        let mut run = ReviewWorkerRun {
            claimed: claims.len(),
            ..ReviewWorkerRun::default()
        };

        for claim in claims {
            let Some(record) = self.store.get(&claim.review_id).await? else {
                return Err(ReviewSessionError::Store(format!(
                    "claimed review session {} disappeared before execution",
                    claim.review_id
                )));
            };
            if record.status.is_terminal() {
                run.skipped += 1;
                continue;
            }

            let input = CreateReviewSessionInput {
                source: record.source.clone(),
                options: record.options.clone(),
            };
            match ReviewSession::execute_local_async(record.id.clone(), input).await {
                Ok(session) => {
                    let result = session.wait()?;
                    let status = session.status();
                    let (events, redacted_artifacts, raw_artifacts) =
                        session.persisted_execution_parts();
                    let updated = self
                        .store
                        .write_execution_result(
                            session.id(),
                            status,
                            result,
                            events,
                            redacted_artifacts,
                            raw_artifacts,
                        )
                        .await?;
                    match updated.status {
                        ReviewStatus::Completed => run.completed += 1,
                        ReviewStatus::Failed | ReviewStatus::Cancelled => run.failed += 1,
                        ReviewStatus::Created | ReviewStatus::Queued | ReviewStatus::Running => {
                            run.skipped += 1;
                        }
                    }
                }
                Err(error) => {
                    let updated = self
                        .store
                        .record_attempt_failure(
                            &claim.review_id,
                            ReviewAttemptFailure {
                                error: error.to_string(),
                                retry_policy: self.host_config.scheduling.default_retry_policy,
                                now_unix_seconds: None,
                            },
                        )
                        .await?;
                    if updated.status == ReviewStatus::Failed {
                        run.failed += 1;
                    } else {
                        run.retried += 1;
                    }
                }
            }
        }

        Ok(run)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewWorkerRun {
    pub claimed: usize,
    pub completed: usize,
    pub retried: usize,
    pub failed: usize,
    pub skipped: usize,
}
