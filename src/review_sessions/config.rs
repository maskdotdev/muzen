use serde::{Deserialize, Serialize};

use super::{ReviewRetryPolicy, ReviewWorkerClaimOptions, ReviewWorkerConcurrencyLimits};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfiguration {
    #[serde(default)]
    pub scheduling: HostSchedulingConfiguration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSchedulingConfiguration {
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
    #[serde(default)]
    pub default_retry_policy: ReviewRetryPolicy,
    #[serde(default)]
    pub concurrency: ReviewWorkerConcurrencyLimits,
    #[serde(default)]
    pub fairness: SchedulingFairnessStrategy,
}

impl Default for HostSchedulingConfiguration {
    fn default() -> Self {
        Self {
            lease_seconds: default_lease_seconds(),
            default_retry_policy: ReviewRetryPolicy::default(),
            concurrency: ReviewWorkerConcurrencyLimits::default(),
            fairness: SchedulingFairnessStrategy::default(),
        }
    }
}

impl HostSchedulingConfiguration {
    pub(crate) fn claim_options(
        &self,
        worker_id: impl Into<String>,
        max_sessions: usize,
    ) -> ReviewWorkerClaimOptions {
        ReviewWorkerClaimOptions {
            worker_id: worker_id.into(),
            max_sessions,
            lease_seconds: self.lease_seconds,
            now_unix_seconds: None,
            concurrency: self.concurrency,
        }
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingFairnessStrategy {
    #[default]
    Fifo,
    RoundRobinByProject,
}

fn default_lease_seconds() -> u64 {
    60
}
