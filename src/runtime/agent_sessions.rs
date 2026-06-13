//! Direct session execution: runs user-supplied agent sessions through the
//! generic loop (model turn -> tool batch -> model turn) with no review
//! planning, evidence obligations, or findings synthesis. This is the swarm
//! primitive; the planned review pipeline is one consumer-style alternative
//! built on the same engine.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::contracts::{TokenUsage, ToolCounts};
use crate::runtime::contracts::*;
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::effects::{ToolResultBatchState, ToolResultEffectProcessor};
use crate::runtime::model::ConcurrentModelRouter;
use crate::runtime::model_retry::complete_model_turn;
use crate::runtime::planned_units::{add_model_metrics, elapsed_ms, record_usage};
use crate::runtime::policy::{ReviewerPolicy, SessionEvidence};
use crate::runtime::tool_batch::ToolBatchRunner;
use crate::runtime::tools::ToolEngine;
use crate::runtime::transcript::enforce_prompt_budget;

pub(crate) struct AgentSessionRuntime {
    pub(crate) model_router: Arc<dyn ConcurrentModelRouter>,
    pub(crate) tools: Arc<ToolEngine>,
    pub(crate) policy: Arc<ReviewerPolicy>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) review_revision_id: String,
    pub(crate) events: RuntimeEventDispatcher,
    pub(crate) active_sessions: Arc<Semaphore>,
}

pub(crate) struct AgentSessionsRunReport {
    pub(crate) metrics: ConcurrentRunReport,
    pub(crate) outputs: Vec<AgentSessionOutput>,
}

struct AgentSessionReport {
    output: AgentSessionOutput,
    model_calls: usize,
    model_metrics: ModelMetricsSnapshot,
    tool_counts: ToolCounts,
    tokens: TokenUsage,
    diagnostic: SessionCompletionDiagnostic,
}

impl AgentSessionReport {
    fn terminal(scope: &SessionScope, status: &str) -> Self {
        Self {
            output: AgentSessionOutput {
                session_id: scope.id.0.clone(),
                status: status.to_string(),
                completed: false,
                output: None,
            },
            model_calls: 0,
            model_metrics: ModelMetricsSnapshot::default(),
            tool_counts: ToolCounts::default(),
            tokens: TokenUsage::default(),
            diagnostic: session_diagnostic(scope, false, status, 0, ToolCounts::default()),
        }
    }
}

impl AgentSessionRuntime {
    pub(crate) async fn run_with_cancel(
        self: Arc<Self>,
        sessions: Vec<SessionScope>,
        cancel: CancellationToken,
    ) -> AgentSessionsRunReport {
        let started = Instant::now();
        let session_count = sessions.len();
        let mut joins = JoinSet::new();
        for (index, scope) in sessions.into_iter().enumerate() {
            let runtime = Arc::clone(&self);
            let active = Arc::clone(&self.active_sessions);
            let cancel = cancel.child_token();
            joins.spawn(async move {
                let Ok(_permit) = active.acquire_owned().await else {
                    return (index, AgentSessionReport::terminal(&scope, "cancelled"));
                };
                if cancel.is_cancelled() {
                    return (index, AgentSessionReport::terminal(&scope, "cancelled"));
                }
                let report = runtime.run_session(scope, cancel).await;
                (index, report)
            });
        }
        let mut session_reports = Vec::with_capacity(session_count);
        while let Some(result) = joins.join_next().await {
            if let Ok(indexed) = result {
                session_reports.push(indexed);
            }
        }
        session_reports.sort_by_key(|(index, _)| *index);

        let mut completed_sessions = 0usize;
        let mut model_calls = 0usize;
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut tool_counts = ToolCounts::default();
        let mut tokens = TokenUsage::default();
        let mut completion_diagnostics = Vec::new();
        let mut outputs = Vec::with_capacity(session_count);
        for (_, report) in session_reports {
            if report.output.completed {
                completed_sessions += 1;
            }
            model_calls += report.model_calls;
            add_model_metrics(&mut model_metrics, &report.model_metrics);
            tool_counts.add(report.tool_counts);
            tokens.add(report.tokens);
            completion_diagnostics.push(report.diagnostic);
            outputs.push(report.output);
        }

        let (artifacts, artifact_bytes) = self.tools.artifacts.stats();
        let elapsed_ms = elapsed_ms(started);
        let metrics = ConcurrentRunReport {
            runtime: "agent_sessions",
            sessions: session_count,
            completed_sessions,
            model_calls,
            tool_calls: tool_counts.total(),
            tool_counts,
            findings: 0,
            publishable_findings: 0,
            elapsed_ms,
            input_tokens: tokens.input_tokens,
            output_tokens: tokens.output_tokens,
            total_tokens: tokens.total_tokens,
            cached_input_tokens: tokens.cached_input_tokens,
            artifacts,
            artifact_bytes,
            counters: self.tools.snapshot_counters(),
            tool_metrics: self.tools.snapshot_tool_metrics(),
            provider_health: self.tools.snapshot_provider_health(),
            snapshot_metrics: vec![SnapshotMetricsSnapshot {
                snapshot_id: self.tools.snapshot.snapshot_id.clone(),
                sessions: session_count,
                completed_sessions,
                model_calls,
                tool_calls: tool_counts.total(),
                artifacts,
                artifact_bytes,
                elapsed_ms,
            }],
            model_metrics,
            completion_diagnostics,
            benchmark_valid: session_count > 0 && completed_sessions == session_count,
            benchmark_failures: Vec::new(),
        };
        AgentSessionsRunReport { metrics, outputs }
    }

    async fn run_session(
        &self,
        scope: SessionScope,
        cancel: CancellationToken,
    ) -> AgentSessionReport {
        self.events
            .emit_planned_runtime(self.policy.plan_session_started_runtime_event(&scope));
        let model = match self.model_router.client_for(&scope).await {
            Ok(model) => model,
            Err(error) => {
                self.events
                    .emit_legacy(self.policy.plan_model_router_error_event(&scope, &error));
                self.events.emit_planned_runtime(
                    self.policy
                        .plan_session_finished_runtime_event(&scope, "failed"),
                );
                return AgentSessionReport::terminal(&scope, "failed");
            }
        };

        let mut transcript = initial_transcript(&scope);
        let mut evidence = SessionEvidence::for_scope(&scope);
        let mut tool_counts = ToolCounts::default();
        let mut model_metrics = ModelMetricsSnapshot::default();
        let mut tokens = TokenUsage::default();
        let mut model_calls = 0usize;
        let mut status = "partial";
        let mut output = None;
        let turn_limit = scope.budget.max_turns.max(1);

        for turn_index in 0..turn_limit {
            if cancel.is_cancelled() {
                status = "cancelled";
                break;
            }
            let turn_id = TurnId(turn_index as u32);
            enforce_prompt_budget(&mut transcript, scope.budget.max_prompt_tokens);
            self.events.emit_planned_runtime(
                self.policy
                    .plan_model_started_runtime_event(&scope, turn_id),
            );
            let model_started = Instant::now();
            let final_turn =
                turn_index + 1 >= turn_limit || tool_counts.total() >= scope.budget.max_tool_calls;
            let model_scope = if final_turn {
                text_only_scope(&scope)
            } else {
                scope.clone()
            };
            let outcome = complete_model_turn(
                &*model,
                &self.policy,
                &self.events,
                &self.limits,
                &scope,
                &model_scope,
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
                    status = "failed";
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
                    status = "done";
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
                        status = "done";
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
                        scope
                            .budget
                            .max_tool_calls
                            .saturating_sub(tool_counts.total()),
                        cancel.child_token(),
                    )
                    .await;
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

        let completed = status == "done";
        self.events.emit_planned_runtime(
            self.policy
                .plan_session_finished_runtime_event(&scope, status),
        );
        AgentSessionReport {
            diagnostic: session_diagnostic(&scope, completed, status, model_calls, tool_counts),
            output: AgentSessionOutput {
                session_id: scope.id.0,
                status: status.to_string(),
                completed,
                output,
            },
            model_calls,
            model_metrics,
            tool_counts,
            tokens,
        }
    }
}

/// The session prompt is exactly what the host supplied: the objective plus
/// any session instructions, in order.
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

/// On the final turn the session must answer in text, so tools are withheld.
fn text_only_scope(scope: &SessionScope) -> SessionScope {
    let mut scope = scope.clone();
    scope.capabilities.tool_grants.clear();
    scope
}

fn session_diagnostic(
    scope: &SessionScope,
    completed: bool,
    status: &str,
    model_calls: usize,
    tool_counts: ToolCounts,
) -> SessionCompletionDiagnostic {
    SessionCompletionDiagnostic {
        session_id: scope.id.0.clone(),
        completed,
        completion_kind: Some("agent_session_output".to_string()),
        completion_summary: Some(status.to_string()),
        saw_diff: false,
        saw_file: false,
        saw_search: false,
        model_calls,
        tool_counts,
    }
}
