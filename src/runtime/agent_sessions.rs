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
use crate::runtime::contracts::{stable_id, *};
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::effects::{ToolResultBatchState, ToolResultEffectProcessor};
use crate::runtime::model::ConcurrentModelRouter;
use crate::runtime::model_retry::complete_model_turn;
use crate::runtime::planned_units::{add_model_metrics, elapsed_ms, record_usage};
use crate::runtime::policy::{ReviewerPolicy, SessionEvidence};
use crate::runtime::tool_batch::ToolBatchRunner;
use crate::runtime::tools::{ToolEngine, ToolRegistry};
use crate::runtime::transcript::{enforce_prompt_budget, estimate_prompt_tokens};
use crate::util::peak_rss_bytes;

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
            quality_diagnostics: ReviewQualityDiagnostics::default(),
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
            Err(_error) => {
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
            let evicted_tool_results =
                enforce_prompt_budget(&mut transcript, scope.budget.max_prompt_tokens);
            if evicted_tool_results > 0 {
                self.events
                    .emit_planned_runtime(self.policy.plan_agent_trace_event(
                        &scope,
                        Some(turn_id),
                        "transcript_compacted",
                        format!(
                            "evicted {evicted_tool_results} old tool result(s) before model turn"
                        ),
                        serde_json::json!({
                            "evictedToolResults": evicted_tool_results,
                            "transcriptItemsAfter": transcript.len(),
                            "estimatedPromptTokensAfter": estimate_prompt_tokens(&transcript),
                            "maxPromptTokens": scope.budget.max_prompt_tokens,
                        }),
                    ));
            }
            self.events.emit_planned_runtime(
                self.policy
                    .plan_model_started_runtime_event(&scope, turn_id),
            );
            let model_started = Instant::now();
            let final_turn =
                turn_index + 1 >= turn_limit || tool_counts.total() >= scope.budget.max_tool_calls;
            if final_turn && scope.response_format.is_some() {
                transcript.push(ConversationItem::User {
                    content: "Return the final structured answer now. Include every distinct supported bug found during exploration, especially separate changed predicates, date/boundary checks, and contract violations. Return findings: [] only if no supported bug remains after considering the gathered evidence.".to_string(),
                });
                self.events
                    .emit_planned_runtime(self.policy.plan_agent_trace_event(
                        &scope,
                        Some(turn_id),
                        "structured_final_instruction_added",
                        "added final structured-output instruction",
                        serde_json::json!({
                            "responseFormatName": scope
                                .response_format
                                .as_ref()
                                .map(|format| format.name.as_str()),
                            "transcriptItemsAfter": transcript.len(),
                        }),
                    ));
            }
            let model_scope = if final_turn {
                final_text_scope(&scope)
            } else {
                tool_turn_scope(&scope)
            };
            let response_format = model_scope.response_format.as_ref();
            self.events
                .emit_planned_runtime(self.policy.plan_agent_trace_event(
                &scope,
                Some(turn_id),
                "model_turn_prepared",
                format!(
                        "prepared model turn with {} transcript item(s) and {} exposed tool(s)",
                        transcript.len(),
                        exposed_tool_names(
                            self.policy.as_ref(),
                            &self.tools.registry,
                            &transcript,
                            &model_scope.capabilities,
                        )
                        .len()
                    ),
                serde_json::json!({
                    "finalTurn": final_turn,
                    "callScopeHasTools": !model_scope.capabilities.tool_grants.is_empty(),
                    "transcriptItems": transcript.len(),
                    "estimatedPromptTokens": estimate_prompt_tokens(&transcript),
                    "maxPromptTokens": scope.budget.max_prompt_tokens,
                    "peakRssBytes": peak_rss_bytes(),
                    "structuredOutputRequested": response_format.is_some(),
                    "responseFormatName": response_format.map(|format| format.name.as_str()),
                    "responseFormatStrict": response_format.map(|format| format.strict),
                    "responseFormatSchemaDigest": response_format_schema_digest(response_format),
                    "exposedTools": exposed_tool_names(
                        self.policy.as_ref(),
                        &self.tools.registry,
                        &transcript,
                        &model_scope.capabilities,
                    ),
                }),
            ));
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
            self.events
                .emit_planned_runtime(self.policy.plan_agent_trace_event(
                    &scope,
                    Some(turn_id),
                    model_turn_trace_kind(&turn),
                    model_turn_trace_summary(&turn),
                    model_turn_trace_details(&turn, outcome.attempts),
                ));
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
                    if !final_turn && scope.response_format.is_some() {
                        self.events
                            .emit_planned_runtime(self.policy.plan_agent_trace_event(
                                &scope,
                                Some(turn_id),
                                "early_text_deferred_for_structured_final",
                                "deferred early text output so final turn can use response format",
                                serde_json::json!({
                                    "textBytes": content.len(),
                                    "textDigest": stable_id(&[&content]),
                                    "responseFormatName": scope
                                        .response_format
                                        .as_ref()
                                        .map(|format| format.name.as_str()),
                                }),
                            ));
                        transcript.push(ConversationItem::AssistantText {
                            content: content.clone(),
                        });
                        transcript.push(ConversationItem::User {
                            content:
                                "Return the final answer using the required structured JSON schema."
                                    .to_string(),
                        });
                        continue;
                    }
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
fn final_text_scope(scope: &SessionScope) -> SessionScope {
    let mut scope = scope.clone();
    scope.capabilities.tool_grants.clear();
    scope
}

/// During exploration, tools stay available and final-output schemas are withheld.
fn tool_turn_scope(scope: &SessionScope) -> SessionScope {
    let mut scope = scope.clone();
    scope.response_format = None;
    scope
}

fn response_format_schema_digest(response_format: Option<&ModelResponseFormat>) -> Option<String> {
    response_format.map(|format| stable_id(&[&format.schema.to_string()]))
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

fn exposed_tool_names(
    policy: &ReviewerPolicy,
    registry: &ToolRegistry,
    transcript: &[ConversationItem],
    capabilities: &CapabilitySet,
) -> Vec<serde_json::Value> {
    policy
        .tool_schemas_for_transcript(registry, transcript, capabilities)
        .into_iter()
        .filter_map(|schema| {
            let function = schema.get("function")?;
            let name = function.get("name")?.as_str()?;
            Some(serde_json::json!({ "modelName": name }))
        })
        .collect()
}

fn model_turn_trace_kind(turn: &ModelTurn) -> &'static str {
    match turn {
        ModelTurn::Text { .. } => "model_turn_completed.text",
        ModelTurn::ToolCalls { .. } => "model_turn_completed.tool_calls",
    }
}

fn model_turn_trace_summary(turn: &ModelTurn) -> String {
    match turn {
        ModelTurn::Text { content, .. } => {
            format!("model returned text output ({} bytes)", content.len())
        }
        ModelTurn::ToolCalls { calls, .. } => {
            format!("model returned {} tool call(s)", calls.len())
        }
    }
}

fn model_turn_trace_details(turn: &ModelTurn, attempts: usize) -> serde_json::Value {
    match turn {
        ModelTurn::Text { content, usage } => serde_json::json!({
            "attempts": attempts,
            "outputKind": "text",
            "textBytes": content.len(),
            "textDigest": stable_id(&[content]),
            "usage": usage,
        }),
        ModelTurn::ToolCalls { calls, usage } => serde_json::json!({
            "attempts": attempts,
            "outputKind": "tool_calls",
            "toolCallCount": calls.len(),
            "toolCalls": calls.iter().map(|call| serde_json::json!({
                "callId": call.call_id.0,
                "index": call.index,
                "toolName": call.name.as_str(),
                "argumentBytes": call.raw_arguments.len(),
                "argumentHash": blake3::hash(call.raw_arguments.as_bytes()).to_hex().to_string(),
                "argumentSummary": call.redacted_argument_summary(),
            })).collect::<Vec<_>>(),
            "usage": usage,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{AgentBudget, Role};
    use serde_json::json;

    #[test]
    fn final_text_scope_withholds_tools_but_preserves_response_format() {
        let scope = SessionScope::review_read_only(
            SessionId("direct".to_string()),
            Role::Generalist,
            "return findings",
            AgentBudget::planned_baseline(),
        )
        .with_response_format(ModelResponseFormat::json_schema(
            "direct_findings",
            json!({
                "type": "object",
                "required": ["findings"],
                "properties": {
                    "findings": {"type": "array"}
                }
            }),
        ));
        assert!(!scope.capabilities.tool_grants.is_empty());

        let final_scope = final_text_scope(&scope);

        assert!(final_scope.capabilities.tool_grants.is_empty());
        assert_eq!(
            final_scope
                .response_format
                .as_ref()
                .map(|format| format.name.as_str()),
            Some("direct_findings")
        );
        assert!(!scope.capabilities.tool_grants.is_empty());
    }

    #[test]
    fn tool_turn_scope_preserves_tools_but_withholds_response_format() {
        let scope = SessionScope::review_read_only(
            SessionId("direct".to_string()),
            Role::Generalist,
            "explore first",
            AgentBudget::planned_baseline(),
        )
        .with_response_format(ModelResponseFormat::json_schema(
            "direct_findings",
            json!({
                "type": "object",
                "required": ["findings"],
                "properties": {
                    "findings": {"type": "array"}
                }
            }),
        ));

        let tool_scope = tool_turn_scope(&scope);

        assert!(!tool_scope.capabilities.tool_grants.is_empty());
        assert!(tool_scope.response_format.is_none());
        assert!(scope.response_format.is_some());
    }
}
