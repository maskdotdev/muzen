use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeLimits {
    pub max_active_sessions: usize,
    pub max_model_concurrency_global: usize,
    pub max_model_concurrency_per_key: usize,
    #[serde(default = "default_max_model_turn_ms")]
    pub max_model_turn_ms: u64,
    /// Total attempts per model turn, including the first; `max_model_turn_ms`
    /// bounds each attempt separately, so a turn's worst-case wall time is
    /// `attempts * max_model_turn_ms` plus backoff.
    #[serde(default = "default_model_retry_max_attempts")]
    pub model_retry_max_attempts: usize,
    #[serde(default = "default_model_retry_base_delay_ms")]
    pub model_retry_base_delay_ms: u64,
    #[serde(default = "default_model_retry_max_delay_ms")]
    pub model_retry_max_delay_ms: u64,
    pub max_tool_calls_per_turn: usize,
    pub max_tool_parallelism_per_session: usize,
    pub max_tool_provider_concurrency_per_provider: usize,
    pub max_tool_provider_ms: u64,
    #[serde(default = "default_max_tool_output_bytes")]
    pub max_tool_output_bytes: usize,
    #[serde(default = "default_max_tool_artifact_bytes")]
    pub max_tool_artifact_bytes: usize,
    pub max_read_concurrency_global: usize,
    pub max_search_jobs_global: usize,
    pub max_search_queue_depth: usize,
    pub max_file_bytes_read: usize,
    pub max_file_bytes_search: usize,
    pub max_search_matches: usize,
    pub max_search_pattern_bytes: usize,
    pub file_content_cache_bytes: u64,
    pub search_result_cache_bytes: u64,
    pub search_threads: usize,
    #[serde(default)]
    pub max_child_sessions: Option<usize>,
    #[serde(default)]
    pub orchestrator_model_profile_id: Option<String>,
    #[serde(default)]
    pub search_model_profile_id: Option<String>,
    #[serde(default)]
    pub explore_model_profile_id: Option<String>,
    #[serde(default)]
    pub validator_model_profile_id: Option<String>,
}

impl RuntimeLimits {
    pub fn standard(sessions: usize, max_file_bytes: usize, max_search_matches: usize) -> Self {
        Self {
            max_active_sessions: sessions.max(1),
            max_model_concurrency_global: 16,
            max_model_concurrency_per_key: 4,
            max_model_turn_ms: default_max_model_turn_ms(),
            model_retry_max_attempts: default_model_retry_max_attempts(),
            model_retry_base_delay_ms: default_model_retry_base_delay_ms(),
            model_retry_max_delay_ms: default_model_retry_max_delay_ms(),
            max_tool_calls_per_turn: 8,
            max_tool_parallelism_per_session: 4,
            max_tool_provider_concurrency_per_provider: 8,
            max_tool_provider_ms: 30_000,
            max_tool_output_bytes: default_max_tool_output_bytes(),
            max_tool_artifact_bytes: default_max_tool_artifact_bytes(),
            max_read_concurrency_global: 32,
            max_search_jobs_global: 1,
            max_search_queue_depth: 128,
            max_file_bytes_read: max_file_bytes,
            max_file_bytes_search: max_file_bytes,
            max_search_matches,
            max_search_pattern_bytes: 512,
            file_content_cache_bytes: 32_000_000,
            search_result_cache_bytes: 16_000_000,
            search_threads: num_cpus::get().clamp(2, 8),
            max_child_sessions: None,
            orchestrator_model_profile_id: None,
            search_model_profile_id: None,
            explore_model_profile_id: None,
            validator_model_profile_id: None,
        }
    }
}

fn default_max_model_turn_ms() -> u64 {
    180_000
}

fn default_model_retry_max_attempts() -> usize {
    3
}

fn default_model_retry_base_delay_ms() -> u64 {
    500
}

fn default_model_retry_max_delay_ms() -> u64 {
    10_000
}

fn default_max_tool_output_bytes() -> usize {
    2 * 1024 * 1024
}

fn default_max_tool_artifact_bytes() -> usize {
    8 * 1024 * 1024
}
