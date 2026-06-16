use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::agent_loop::{AgentLoopConfig, AgentLoopReport};
use crate::reviewer_kernel::kernel_types::{ModelResponseFormat, SessionScope};
use crate::reviewer_kernel::review_contract::{AgentBudget, BudgetSource};

use super::delegates::AutonomousDelegateState;
use super::schemas::{schema_repair_instruction, session_output_valid};
use super::tasks::DelegateTaskKind;

const DEFAULT_SCHEMA_REPAIR_ATTEMPTS: usize = 1;
const MIN_ORCHESTRATOR_MODEL_TURN_GUARD: usize = 16;
const MAX_ORCHESTRATOR_MODEL_TURN_GUARD: usize = 256;

pub(super) struct SessionRunConfig {
    pub(super) scope: SessionScope,
    pub(super) kind: SessionKind,
    pub(super) task_packet: Option<String>,
    pub(super) response_format: ModelResponseFormat,
    pub(super) final_instruction: String,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum SessionKind {
    Orchestrator,
    Child(DelegateTaskKind),
}

pub(super) async fn run_session_loop(
    state: Arc<AutonomousDelegateState>,
    config: SessionRunConfig,
    cancel: CancellationToken,
) -> AgentLoopReport {
    let kind = config.kind;
    let turn_guard = session_turn_guard(kind, &config.scope.budget);
    state
        .agent_loop_runtime()
        .run_session_loop(
            AgentLoopConfig {
                scope: config.scope,
                task_packet: config.task_packet,
                trace_kind: session_kind_name(kind),
                completion_kind: "autonomous_review_session",
                response_format: config.response_format,
                final_instruction: config.final_instruction,
                turn_guard,
                should_force_final_turn: Box::new(move |turn_index, tool_calls_used, budget| {
                    should_force_final_turn(kind, turn_index, turn_guard, tool_calls_used, budget)
                }),
                output_valid: Box::new(move |output| session_output_valid(kind, output)),
                schema_repair_instruction: Box::new(move |attempt, max_attempts| {
                    schema_repair_instruction(kind, attempt, max_attempts)
                }),
                schema_repair_attempts: DEFAULT_SCHEMA_REPAIR_ATTEMPTS,
            },
            cancel,
        )
        .await
}

pub(super) fn session_kind_name(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Orchestrator => "orchestrator",
        SessionKind::Child(DelegateTaskKind::SearchCode) => "search_code",
        SessionKind::Child(DelegateTaskKind::ExploreCode) => "explore_code",
        SessionKind::Child(DelegateTaskKind::ValidateFinding) => "validate_finding",
    }
}

pub(super) fn session_turn_guard(kind: SessionKind, budget: &AgentBudget) -> usize {
    match kind {
        SessionKind::Orchestrator => budget
            .max_turns
            .max(
                budget
                    .max_tool_calls
                    .saturating_add(1 + DEFAULT_SCHEMA_REPAIR_ATTEMPTS),
            )
            .clamp(
                MIN_ORCHESTRATOR_MODEL_TURN_GUARD,
                MAX_ORCHESTRATOR_MODEL_TURN_GUARD,
            ),
        SessionKind::Child(_) => budget.max_turns.max(1),
    }
}

pub(super) fn should_force_final_turn(
    kind: SessionKind,
    turn_index: usize,
    turn_guard: usize,
    tool_calls_used: usize,
    budget: &AgentBudget,
) -> bool {
    if tool_calls_used >= budget.max_tool_calls {
        return true;
    }
    match kind {
        SessionKind::Orchestrator => turn_index >= turn_guard.saturating_sub(1),
        SessionKind::Child(_) => {
            let turn_limit = budget.max_turns.max(1);
            let reserved_final_turns = (1 + DEFAULT_SCHEMA_REPAIR_ATTEMPTS).min(turn_limit);
            let finalization_start = turn_limit.saturating_sub(reserved_final_turns);
            turn_index >= finalization_start
        }
    }
}

pub(super) fn autonomous_orchestrator_budget(
    mut budget: AgentBudget,
    changed_file_count: usize,
) -> AgentBudget {
    if budget.budget_source == BudgetSource::CallerHardCap {
        return budget;
    }
    let target_tool_calls = changed_file_count.saturating_mul(4).clamp(48, 96);
    budget.max_tool_calls = budget.max_tool_calls.max(target_tool_calls);
    budget.max_prompt_tokens = budget.max_prompt_tokens.max(96_000);
    budget.budget_source = BudgetSource::AdaptiveReview;
    budget
}
