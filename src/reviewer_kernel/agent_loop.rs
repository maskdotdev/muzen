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
    pub(crate) tool_calls_used: usize,
    pub(crate) exhausted_tool_budget: bool,
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
        let mut finalization_reason = "max_turns".to_string();
        let mut repair_attempt_count = 0usize;

        for turn_index in 0..config.turn_guard {
            next_turn_index = turn_index + 1;
            if cancel.is_cancelled() {
                status = "cancelled".to_string();
                finalization_reason = "cancelled".to_string();
                break;
            }
            let turn_id = TurnId(turn_index as u32);
            let compaction = enforce_prompt_budget(&mut transcript, scope.budget.max_prompt_tokens);
            if let Some(compaction) = compaction {
                self.events
                    .emit_planned_runtime(self.policy.plan_agent_trace_event(
                        &scope,
                        Some(turn_id),
                        "transcript_compacted",
                        format!(
                            "evicted {} transcript item reference(s)",
                            compaction.evicted_total()
                        ),
                        json!({
                            "sessionId": scope.id.0,
                            "turnId": turn_id.0,
                            "nextModelTurnId": turn_id.0,
                            "evictedToolResults": compaction.evicted.tool_results,
                            "evictedItemCounts": {
                                "toolResult": compaction.evicted.tool_results,
                                "modelText": compaction.evicted.model_text,
                                "artifactRef": compaction.evicted.artifact_refs,
                                "candidateEvidence": compaction.evicted.candidate_evidence,
                            },
                            "transcriptItemsBefore": compaction.transcript_items_before,
                            "transcriptItemsAfter": compaction.transcript_items_after,
                            "estimatedPromptTokensBefore": compaction.token_estimate_before,
                            "estimatedPromptTokensAfter": compaction.token_estimate_after,
                            "maxPromptTokens": scope.budget.max_prompt_tokens,
                        }),
                    ));
            }
            let final_turn =
                (config.should_force_final_turn)(turn_index, tool_calls_used, &scope.budget);
            let final_turn_reason = final_turn
                .then(|| forced_finalization_reason(turn_index, tool_calls_used, &scope.budget));
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
                        "finalTurnReason": final_turn_reason,
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
            model_metrics.retries += outcome.attempts.saturating_sub(1);
            model_metrics.limiter_wait_ms += outcome.limiter_wait.total_wait_ms;
            model_metrics.max_limiter_wait_ms = model_metrics
                .max_limiter_wait_ms
                .max(outcome.limiter_wait.max_bucket_wait_ms);
            let turn = match outcome.result {
                Ok(turn) => {
                    model_metrics.errors += outcome.attempts - 1;
                    turn
                }
                Err(_) => {
                    model_metrics.errors += outcome.attempts;
                    status = "failed".to_string();
                    finalization_reason = "model_failed".to_string();
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
                    finalization_reason = final_turn_reason
                        .unwrap_or("model_no_tool_calls")
                        .to_string();
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
                        finalization_reason = final_turn_reason
                            .unwrap_or("model_no_tool_calls")
                            .to_string();
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
                    finalization_reason = "cancelled".to_string();
                    break;
                }
                repair_attempt_count += 1;
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
                model_metrics.retries += outcome.attempts.saturating_sub(1);
                model_metrics.limiter_wait_ms += outcome.limiter_wait.total_wait_ms;
                model_metrics.max_limiter_wait_ms = model_metrics
                    .max_limiter_wait_ms
                    .max(outcome.limiter_wait.max_bucket_wait_ms);
                let turn = match outcome.result {
                    Ok(turn) => {
                        model_metrics.errors += outcome.attempts - 1;
                        turn
                    }
                    Err(_) => {
                        model_metrics.errors += outcome.attempts;
                        status = "incomplete".to_string();
                        finalization_reason = "model_failed".to_string();
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
                        if status != "done" {
                            finalization_reason = "schema_repair_exhausted".to_string();
                        }
                        if status == "done" {
                            break;
                        }
                    }
                    ModelTurn::ToolCalls { usage, .. } => {
                        record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                        status = "incomplete".to_string();
                        finalization_reason = "schema_repair_exhausted".to_string();
                        break;
                    }
                }
            }
        }
        if status == "done" && !(config.output_valid)(output.as_deref()) {
            status = "incomplete".to_string();
            finalization_reason = "schema_repair_exhausted".to_string();
        }
        let final_output = final_output_diagnostic(
            output.as_deref(),
            &config.output_valid,
            repair_attempt_count,
        );
        if status == "partial" {
            finalization_reason = "max_turns".to_string();
        }
        let completed = status == "done";
        if completed && config.trace_kind == "validate_finding" {
            finalization_reason = "validation_complete".to_string();
        }
        self.events
            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                &scope,
                None,
                "session_finalization",
                format!("session finalized: {finalization_reason}"),
                json!({
                    "sessionKind": config.trace_kind,
                    "status": status,
                    "completed": completed,
                    "finalizationReason": finalization_reason,
                    "finalOutput": final_output,
                    "modelCalls": model_calls,
                    "toolCallsUsed": tool_calls_used,
                    "maxTurns": scope.budget.max_turns,
                    "maxToolCalls": scope.budget.max_tool_calls,
                }),
            ));
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
                &finalization_reason,
                final_output.clone(),
                model_calls,
                tool_calls_used,
                tool_counts,
            ),
            completed,
            status,
            output,
            model_calls,
            tool_calls_used,
            exhausted_tool_budget: tool_calls_used >= scope.budget.max_tool_calls,
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
        tool_calls_used: 0,
        exhausted_tool_budget: false,
        model_metrics: ModelMetricsSnapshot::default(),
        tool_counts: ToolCounts::default(),
        tokens: TokenUsage::default(),
        diagnostic: session_diagnostic(
            scope,
            completion_kind,
            false,
            status,
            if status == "cancelled" {
                "cancelled"
            } else {
                "model_failed"
            },
            FinalOutputDiagnostic::default(),
            0,
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
    finalization_reason: &str,
    final_output: FinalOutputDiagnostic,
    model_calls: usize,
    tool_calls_used: usize,
    tool_counts: ToolCounts,
) -> SessionCompletionDiagnostic {
    SessionCompletionDiagnostic {
        session_id: scope.id.0.clone(),
        completed,
        completion_kind: Some(completion_kind.to_string()),
        completion_summary: Some(status.to_string()),
        finalization_reason: finalization_reason.to_string(),
        final_output,
        tool_calls_used,
        max_tool_calls: scope.budget.max_tool_calls,
        exhausted_tool_budget: tool_calls_used >= scope.budget.max_tool_calls,
        saw_diff: tool_counts.read_diff > 0,
        saw_file: tool_counts.read_file + tool_counts.read_file_range + tool_counts.read_head_file
            > 0,
        saw_search: tool_counts.search_text + tool_counts.list_files > 0,
        model_calls,
        tool_counts,
    }
}

fn forced_finalization_reason(
    turn_index: usize,
    tool_calls_used: usize,
    budget: &AgentBudget,
) -> &'static str {
    if tool_calls_used >= budget.max_tool_calls {
        "max_tool_calls"
    } else if turn_index >= budget.max_turns.saturating_sub(1) {
        "max_turns"
    } else {
        "max_turns"
    }
}

fn final_output_diagnostic(
    output: Option<&str>,
    output_valid: &(dyn Fn(Option<&str>) -> bool + Send + Sync),
    repair_attempt_count: usize,
) -> FinalOutputDiagnostic {
    let attempted = output.is_some();
    let parse_success = output
        .map(|output| serde_json::from_str::<serde_json::Value>(output).is_ok())
        .unwrap_or(false);
    let schema_validation_success = parse_success && output_valid(output);
    FinalOutputDiagnostic {
        attempted,
        parse_success,
        schema_validation_success,
        repair_attempt_count,
        accepted: schema_validation_success,
        rejected: attempted && !schema_validation_success,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer_kernel::review_contract::BudgetSource;

    #[test]
    fn final_output_diagnostic_records_parse_and_schema_success() {
        let valid = |output: Option<&str>| {
            output
                .and_then(|output| serde_json::from_str::<serde_json::Value>(output).ok())
                .and_then(|value| value.get("ok").and_then(serde_json::Value::as_bool))
                .unwrap_or(false)
        };

        let diagnostic = final_output_diagnostic(Some(r#"{"ok":true}"#), &valid, 1);

        assert!(diagnostic.attempted);
        assert!(diagnostic.parse_success);
        assert!(diagnostic.schema_validation_success);
        assert_eq!(diagnostic.repair_attempt_count, 1);
        assert!(diagnostic.accepted);
        assert!(!diagnostic.rejected);
    }

    #[test]
    fn final_output_diagnostic_separates_parse_failure_from_schema_rejection() {
        let valid = |output: Option<&str>| output == Some(r#"{"ok":true}"#);

        let parse_failure = final_output_diagnostic(Some("not json"), &valid, 0);
        assert!(parse_failure.attempted);
        assert!(!parse_failure.parse_success);
        assert!(!parse_failure.schema_validation_success);
        assert!(!parse_failure.accepted);
        assert!(parse_failure.rejected);

        let schema_rejection = final_output_diagnostic(Some(r#"{"ok":false}"#), &valid, 2);
        assert!(schema_rejection.parse_success);
        assert!(!schema_rejection.schema_validation_success);
        assert_eq!(schema_rejection.repair_attempt_count, 2);
        assert!(schema_rejection.rejected);
    }

    #[test]
    fn forced_finalization_reason_prefers_tool_budget_exhaustion() {
        let budget = AgentBudget {
            max_turns: 4,
            max_tool_calls: 3,
            max_prompt_tokens: 10_000,
            max_output_tokens: 512,
            budget_source: BudgetSource::PlannedDefault,
        };

        assert_eq!(forced_finalization_reason(1, 3, &budget), "max_tool_calls");
        assert_eq!(forced_finalization_reason(3, 0, &budget), "max_turns");
    }
}
