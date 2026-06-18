use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::effects::{ToolResultBatchState, ToolResultEffectProcessor};
use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::model::ConcurrentModelRouter;
use crate::reviewer_kernel::model_retry::complete_model_turn;
use crate::reviewer_kernel::policy::{ReviewerPolicy, SessionEvidence};
use crate::reviewer_kernel::review_contract::{AgentBudget, TokenUsage, ToolCounts};
use crate::reviewer_kernel::session_metrics::{elapsed_ms, record_usage};
use crate::reviewer_kernel::system::peak_rss_bytes;
use crate::reviewer_kernel::tool_batch::ToolBatchRunner;
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::reviewer_kernel::transcript::{enforce_prompt_budget, estimate_prompt_tokens};

pub(crate) struct AgentLoopRuntime {
    pub(crate) model_router: Arc<dyn ConcurrentModelRouter>,
    pub(crate) tools: Arc<ToolEngine>,
    pub(crate) policy: Arc<ReviewerPolicy>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) review_revision_id: String,
    pub(crate) events: RuntimeEventDispatcher,
}

pub(crate) struct AgentLoopConfig {
    pub(crate) scope: SessionScope,
    pub(crate) task_packet: Option<String>,
    pub(crate) trace_kind: &'static str,
    pub(crate) completion_kind: &'static str,
    pub(crate) response_format: ModelResponseFormat,
    pub(crate) final_instruction: String,
    pub(crate) turn_guard: usize,
    pub(crate) should_force_final_turn:
        Box<dyn Fn(usize, usize, &AgentBudget) -> bool + Send + Sync>,
    pub(crate) output_valid: Box<dyn Fn(Option<&str>) -> bool + Send + Sync>,
    pub(crate) schema_repair_instruction: Box<dyn Fn(usize, usize) -> String + Send + Sync>,
    pub(crate) schema_repair_attempts: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentLoopReport {
    pub(crate) completed: bool,
    pub(crate) status: String,
    pub(crate) output: Option<String>,
    pub(crate) model_calls: usize,
    pub(crate) model_metrics: ModelMetricsSnapshot,
    pub(crate) tool_counts: ToolCounts,
    pub(crate) tokens: TokenUsage,
    pub(crate) diagnostic: SessionCompletionDiagnostic,
}

impl AgentLoopRuntime {
    pub(crate) async fn run_session_loop(
        &self,
        config: AgentLoopConfig,
        cancel: CancellationToken,
    ) -> AgentLoopReport {
        let scope = config.scope;
        self.events
            .emit_planned_runtime(self.policy.plan_session_started_runtime_event(&scope));
        let model = match self.model_router.client_for(&scope).await {
            Ok(model) => model,
            Err(_) => {
                self.events.emit_planned_runtime(
                    self.policy
                        .plan_session_finished_runtime_event(&scope, "failed"),
                );
                return terminal_report(&scope, config.completion_kind, "failed");
            }
        };

        let mut transcript = initial_transcript(&scope);
        if let Some(task_packet) = config.task_packet {
            transcript.push(ConversationItem::User {
                content: task_packet,
            });
        }
        let mut evidence = SessionEvidence::for_scope(&scope);
        let mut tool_counts = ToolCounts::default();
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut tokens = TokenUsage::default();
        let mut model_calls = 0usize;
        let mut tool_calls_used = 0usize;
        let mut status = "partial".to_string();
        let mut output = None;
        let mut next_turn_index = 0usize;

        for turn_index in 0..config.turn_guard {
            next_turn_index = turn_index + 1;
            if cancel.is_cancelled() {
                status = "cancelled".to_string();
                break;
            }
            let turn_id = TurnId(turn_index as u32);
            let evicted_tool_results =
                enforce_prompt_budget(&mut transcript, scope.budget.max_prompt_tokens);
            if evicted_tool_results > 0 {
                self.events
                    .emit_planned_runtime(self.policy.plan_agent_trace_event(
                        &scope,
                        Some(turn_id),
                        "transcript_compacted",
                        format!("evicted {evicted_tool_results} old tool result(s)"),
                        json!({
                            "evictedToolResults": evicted_tool_results,
                            "transcriptItemsAfter": transcript.len(),
                            "estimatedPromptTokensAfter": estimate_prompt_tokens(&transcript),
                            "maxPromptTokens": scope.budget.max_prompt_tokens,
                        }),
                    ));
            }
            let final_turn =
                (config.should_force_final_turn)(turn_index, tool_calls_used, &scope.budget);
            let mut call_scope = scope.clone();
            if final_turn {
                call_scope.capabilities.tool_grants.clear();
                call_scope.response_format = Some(config.response_format.clone());
                transcript.push(ConversationItem::User {
                    content: config.final_instruction.clone(),
                });
            } else {
                call_scope.response_format = None;
            }
            self.events.emit_planned_runtime(
                self.policy
                    .plan_model_started_runtime_event(&scope, turn_id),
            );
            self.events
                .emit_planned_runtime(self.policy.plan_agent_trace_event(
                    &scope,
                    Some(turn_id),
                    "model_turn_prepared",
                    format!(
                        "prepared model turn with {} transcript item(s) and {} exposed tool(s)",
                        transcript.len(),
                        self.policy
                            .tool_schemas_for_transcript(
                                self.tools.registry(),
                                &transcript,
                                &call_scope.capabilities
                            )
                            .len()
                    ),
                    json!({
                        "sessionKind": config.trace_kind,
                        "finalTurn": final_turn,
                        "turnGuard": config.turn_guard,
                        "maxTurns": scope.budget.max_turns,
                        "maxToolCalls": scope.budget.max_tool_calls,
                        "toolCallsUsed": tool_calls_used,
                        "builtinToolCallsUsed": tool_counts.total(),
                        "transcriptItems": transcript.len(),
                        "estimatedPromptTokens": estimate_prompt_tokens(&transcript),
                        "maxPromptTokens": scope.budget.max_prompt_tokens,
                        "peakRssBytes": peak_rss_bytes(),
                    }),
                ));
            let model_started = Instant::now();
            let outcome = complete_model_turn(
                &*model,
                &self.policy,
                &self.events,
                &self.limits,
                &scope,
                &call_scope,
                &transcript,
                turn_id,
                &cancel,
            )
            .await;
            model_calls += outcome.attempts;
            model_metrics.calls += outcome.attempts;
            let turn = match outcome.result {
                Ok(turn) => {
                    model_metrics.errors += outcome.attempts - 1;
                    turn
                }
                Err(_) => {
                    model_metrics.errors += outcome.attempts;
                    status = "failed".to_string();
                    break;
                }
            };
            model_metrics.successes += 1;
            model_metrics.latency_ms += elapsed_ms(model_started);
            model_metrics.max_latency_ms =
                model_metrics.max_latency_ms.max(elapsed_ms(model_started));
            match turn {
                ModelTurn::Text { content, usage } => {
                    record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                    self.events.emit_planned_runtime(
                        self.policy
                            .plan_model_completed_runtime_event(&scope, turn_id, 0),
                    );
                    transcript.push(ConversationItem::AssistantText {
                        content: content.clone(),
                    });
                    output = Some(content);
                    status = "done".to_string();
                    break;
                }
                ModelTurn::ToolCalls { calls, usage } => {
                    record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                    self.events.emit_planned_runtime(
                        self.policy.plan_model_completed_runtime_event(
                            &scope,
                            turn_id,
                            calls.len(),
                        ),
                    );
                    if calls.is_empty() {
                        status = "done".to_string();
                        break;
                    }
                    transcript.push(ConversationItem::AssistantToolCalls {
                        calls: calls.clone(),
                    });
                    let results = ToolBatchRunner::new(
                        self.policy.as_ref(),
                        self.tools.as_ref(),
                        &self.events,
                    )
                    .execute(
                        scope.clone(),
                        turn_id,
                        calls,
                        &evidence,
                        scope.budget.max_tool_calls.saturating_sub(tool_calls_used),
                        cancel.child_token(),
                    )
                    .await;
                    tool_calls_used = tool_calls_used
                        .saturating_add(budgeted_tool_result_count(&results))
                        .min(scope.budget.max_tool_calls);
                    ToolResultEffectProcessor::new(
                        self.policy.as_ref(),
                        self.tools.as_ref(),
                        &self.events,
                        &self.review_revision_id,
                    )
                    .apply_batch(
                        &scope,
                        turn_id,
                        results,
                        ToolResultBatchState {
                            evidence: &mut evidence,
                            tool_counts: &mut tool_counts,
                            transcript: &mut transcript,
                        },
                    );
                }
            }
        }
        if status == "done" && !(config.output_valid)(output.as_deref()) {
            for repair_index in 0..config.schema_repair_attempts {
                if cancel.is_cancelled() {
                    status = "cancelled".to_string();
                    break;
                }
                let turn_id = TurnId((next_turn_index + repair_index) as u32);
                let mut repair_scope = scope.clone();
                repair_scope.capabilities.tool_grants.clear();
                repair_scope.response_format = Some(config.response_format.clone());
                transcript.push(ConversationItem::User {
                    content: (config.schema_repair_instruction)(
                        repair_index + 1,
                        config.schema_repair_attempts,
                    ),
                });
                self.events
                    .emit_planned_runtime(self.policy.plan_agent_trace_event(
                        &scope,
                        Some(turn_id),
                        "schema_repair",
                        format!("schema repair attempt {}", repair_index + 1),
                        json!({
                            "sessionKind": config.trace_kind,
                            "attempt": repair_index + 1,
                            "maxAttempts": config.schema_repair_attempts,
                            "transcriptItems": transcript.len(),
                            "estimatedPromptTokens": estimate_prompt_tokens(&transcript),
                        }),
                    ));
                let model_started = Instant::now();
                let outcome = complete_model_turn(
                    &*model,
                    &self.policy,
                    &self.events,
                    &self.limits,
                    &scope,
                    &repair_scope,
                    &transcript,
                    turn_id,
                    &cancel,
                )
                .await;
                model_calls += outcome.attempts;
                model_metrics.calls += outcome.attempts;
                let turn = match outcome.result {
                    Ok(turn) => {
                        model_metrics.errors += outcome.attempts - 1;
                        turn
                    }
                    Err(_) => {
                        model_metrics.errors += outcome.attempts;
                        status = "incomplete".to_string();
                        break;
                    }
                };
                model_metrics.successes += 1;
                model_metrics.latency_ms += elapsed_ms(model_started);
                model_metrics.max_latency_ms =
                    model_metrics.max_latency_ms.max(elapsed_ms(model_started));
                match turn {
                    ModelTurn::Text { content, usage } => {
                        record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                        transcript.push(ConversationItem::AssistantText {
                            content: content.clone(),
                        });
                        output = Some(content);
                        status = if (config.output_valid)(output.as_deref()) {
                            "done".to_string()
                        } else {
                            "incomplete".to_string()
                        };
                        if status == "done" {
                            break;
                        }
                    }
                    ModelTurn::ToolCalls { usage, .. } => {
                        record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                        status = "incomplete".to_string();
                        break;
                    }
                }
            }
        }
        if status == "done" && !(config.output_valid)(output.as_deref()) {
            status = "incomplete".to_string();
        }
        let completed = status == "done";
        self.events.emit_planned_runtime(
            self.policy
                .plan_session_finished_runtime_event(&scope, &status),
        );
        AgentLoopReport {
            diagnostic: session_diagnostic(
                &scope,
                config.completion_kind,
                completed,
                &status,
                model_calls,
                tool_counts,
            ),
            completed,
            status,
            output,
            model_calls,
            model_metrics,
            tool_counts,
            tokens,
        }
    }
}

fn initial_transcript(scope: &SessionScope) -> Vec<ConversationItem> {
    let mut items = vec![ConversationItem::System {
        content: scope.objective.clone(),
    }];
    for instruction in &scope.instructions {
        items.push(ConversationItem::User {
            content: instruction.text.clone(),
        });
    }
    items
}

fn terminal_report(
    scope: &SessionScope,
    completion_kind: &'static str,
    status: &str,
) -> AgentLoopReport {
    AgentLoopReport {
        completed: false,
        status: status.to_string(),
        output: None,
        model_calls: 0,
        model_metrics: ModelMetricsSnapshot::default(),
        tool_counts: ToolCounts::default(),
        tokens: TokenUsage::default(),
        diagnostic: session_diagnostic(
            scope,
            completion_kind,
            false,
            status,
            0,
            ToolCounts::default(),
        ),
    }
}

fn session_diagnostic(
    scope: &SessionScope,
    completion_kind: &'static str,
    completed: bool,
    status: &str,
    model_calls: usize,
    tool_counts: ToolCounts,
) -> SessionCompletionDiagnostic {
    SessionCompletionDiagnostic {
        session_id: scope.id.0.clone(),
        completed,
        completion_kind: Some(completion_kind.to_string()),
        completion_summary: Some(status.to_string()),
        saw_diff: tool_counts.read_diff > 0,
        saw_file: tool_counts.read_file + tool_counts.read_file_range + tool_counts.read_head_file
            > 0,
        saw_search: tool_counts.search_text + tool_counts.list_files > 0,
        model_calls,
        tool_counts,
    }
}

pub(crate) fn budgeted_tool_result_count(results: &[ToolResultEnvelope]) -> usize {
    results
        .iter()
        .filter(|result| {
            !matches!(
                result.error.as_ref().map(|error| error.code),
                Some(ToolErrorCode::BudgetExceeded)
            )
        })
        .count()
}
