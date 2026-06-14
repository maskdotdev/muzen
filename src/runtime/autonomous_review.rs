use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::contracts::{
    AgentBudget, ArtifactKind, BudgetSource, ChallengeStatus, EvidenceLocationV1, EvidenceRefV1,
    EvidenceRevision, FileReviewV1, FindingPublishability, FindingSeverity, FindingV1, LineRangeV1,
    RedactionMetadataV1, RedactionState, ReportStatus, ReviewCoverage, ReviewVerdict, Role,
    TokenUsage, ToolCounts, ValidationStatus,
};
use crate::runtime::contracts::*;
use crate::runtime::dispatch::RuntimeEventDispatcher;
use crate::runtime::effects::{ToolResultBatchState, ToolResultEffectProcessor};
use crate::runtime::model::ConcurrentModelRouter;
use crate::runtime::model_retry::complete_model_turn;
use crate::runtime::policy::{ReviewerPolicy, SessionEvidence};
use crate::runtime::repo::RepoSnapshot;
use crate::runtime::session_metrics::{add_model_metrics, elapsed_ms, record_usage};
use crate::runtime::tool_batch::ToolBatchRunner;
use crate::runtime::tools::registry::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOutput, ToolRegistry,
};
use crate::runtime::tools::ToolEngine;
use crate::runtime::transcript::{enforce_prompt_budget, estimate_prompt_tokens};
use crate::util::peak_rss_bytes;

const ORCHESTRATOR_SESSION_ID: &str = "review-orchestrator";
const SEARCH_CODE_TOOL: &str = "search_code";
const EXPLORE_CODE_TOOL: &str = "explore_code";
const VALIDATE_FINDING_TOOL: &str = "validate_finding";
const DEFAULT_MAX_CHILD_SESSIONS: usize = 32;
const DEFAULT_SCHEMA_REPAIR_ATTEMPTS: usize = 1;
const MAX_DIFF_RISK_ENTRIES: usize = 40;
const MIN_ORCHESTRATOR_MODEL_TURN_GUARD: usize = 16;
const MAX_ORCHESTRATOR_MODEL_TURN_GUARD: usize = 256;

#[derive(Clone, Default)]
pub(crate) struct AutonomousDelegateHost {
    states: Arc<Mutex<HashMap<SnapshotId, Arc<AutonomousDelegateState>>>>,
}

impl AutonomousDelegateHost {
    pub(crate) fn register(
        &self,
        snapshot_id: SnapshotId,
        state: Arc<AutonomousDelegateState>,
    ) -> DelegateHostGuard {
        self.states.lock().insert(snapshot_id.clone(), state);
        DelegateHostGuard {
            host: self.clone(),
            snapshot_id,
        }
    }

    fn state_for(&self, snapshot_id: &SnapshotId) -> Option<Arc<AutonomousDelegateState>> {
        self.states.lock().get(snapshot_id).cloned()
    }

    fn unregister(&self, snapshot_id: &SnapshotId) {
        self.states.lock().remove(snapshot_id);
    }
}

pub(crate) struct DelegateHostGuard {
    host: AutonomousDelegateHost,
    snapshot_id: SnapshotId,
}

impl Drop for DelegateHostGuard {
    fn drop(&mut self) {
        self.host.unregister(&self.snapshot_id);
    }
}

pub(crate) fn register_autonomous_delegate_tools(
    registry: &mut ToolRegistry,
    host: AutonomousDelegateHost,
) -> RuntimeResult<()> {
    for kind in [
        DelegateTaskKind::SearchCode,
        DelegateTaskKind::ExploreCode,
        DelegateTaskKind::ValidateFinding,
    ] {
        registry.register_custom_with_effects(
            ToolId::parse(kind.tool_name())?,
            kind.description(),
            kind.parameters_schema(),
            false,
            ToolEffects::review_read_only(),
            Arc::new(DelegateToolHandler {
                host: host.clone(),
                kind,
            }),
        )?;
    }
    Ok(())
}

pub(crate) struct AutonomousReviewRuntime {
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) model_router: Arc<dyn ConcurrentModelRouter>,
    pub(crate) tools: Arc<ToolEngine>,
    pub(crate) policy: Arc<ReviewerPolicy>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) review_revision_id: String,
    pub(crate) events: RuntimeEventDispatcher,
    pub(crate) active_sessions: Arc<Semaphore>,
    pub(crate) delegate_host: AutonomousDelegateHost,
}

pub(crate) struct AutonomousReviewRunReport {
    pub(crate) metrics: ConcurrentRunReport,
    pub(crate) findings: Vec<FindingV1>,
    pub(crate) file_reviews: Vec<FileReviewV1>,
}

pub(crate) struct AutonomousDelegateState {
    snapshot: Arc<RepoSnapshot>,
    model_router: Arc<dyn ConcurrentModelRouter>,
    tools: Arc<ToolEngine>,
    policy: Arc<ReviewerPolicy>,
    limits: Arc<RuntimeLimits>,
    review_revision_id: String,
    events: RuntimeEventDispatcher,
    active_sessions: Arc<Semaphore>,
    child_sequence: AtomicUsize,
    max_child_sessions: usize,
    child_reports: Mutex<Vec<SessionRunReport>>,
}

impl AutonomousDelegateState {
    fn new(runtime: &AutonomousReviewRuntime) -> Self {
        Self {
            snapshot: Arc::clone(&runtime.snapshot),
            model_router: Arc::clone(&runtime.model_router),
            tools: Arc::clone(&runtime.tools),
            policy: Arc::clone(&runtime.policy),
            limits: Arc::clone(&runtime.limits),
            review_revision_id: runtime.review_revision_id.clone(),
            events: runtime.events.clone(),
            active_sessions: Arc::clone(&runtime.active_sessions),
            child_sequence: AtomicUsize::new(1),
            max_child_sessions: runtime
                .limits
                .max_child_sessions
                .unwrap_or(DEFAULT_MAX_CHILD_SESSIONS),
            child_reports: Mutex::new(Vec::new()),
        }
    }

    fn model_profile_id_for(&self, kind: DelegateTaskKind) -> Option<String> {
        match kind {
            DelegateTaskKind::SearchCode => self.limits.search_model_profile_id.clone(),
            DelegateTaskKind::ExploreCode => self.limits.explore_model_profile_id.clone(),
            DelegateTaskKind::ValidateFinding => self.limits.validator_model_profile_id.clone(),
        }
    }

    fn next_child_id(&self, kind: DelegateTaskKind) -> RuntimeResult<SessionId> {
        let sequence = self.child_sequence.fetch_add(1, Ordering::SeqCst);
        if sequence > self.max_child_sessions {
            return Err(RuntimeError::LimitExceeded {
                kind: "child_sessions",
            });
        }
        Ok(SessionId(format!(
            "{ORCHESTRATOR_SESSION_ID}/{}-{sequence:04}",
            kind.slug()
        )))
    }

    fn record_child_report(&self, report: SessionRunReport) {
        self.child_reports.lock().push(report);
    }

    fn child_reports(&self) -> Vec<SessionRunReport> {
        self.child_reports.lock().clone()
    }
}

impl AutonomousReviewRuntime {
    pub(crate) async fn run_with_cancel(
        self: Arc<Self>,
        sessions: Vec<SessionScope>,
        cancel: CancellationToken,
    ) -> AutonomousReviewRunReport {
        let started = Instant::now();
        let state = Arc::new(AutonomousDelegateState::new(&self));
        let _guard = self
            .delegate_host
            .register(self.snapshot.snapshot_id.clone(), Arc::clone(&state));

        let template = sessions.into_iter().next();
        let scope = self.orchestrator_scope(template);
        let report = run_session_loop(
            SessionRunConfig {
                state: Arc::clone(&state),
                scope,
                kind: SessionKind::Orchestrator,
                task_packet: None,
                response_format: orchestrator_response_format(),
                final_instruction: orchestrator_final_instruction(),
            },
            cancel.child_token(),
        )
        .await;
        let child_reports = state.child_reports();
        let parsed = parse_orchestrator_output(report.output.as_deref());
        let validations = self
            .run_mandatory_validations(Arc::clone(&state), &parsed.candidates, &cancel)
            .await;
        let validation_reports = state.child_reports();

        let finding_outcome = build_findings(
            &self.tools,
            &self.snapshot,
            &self.review_revision_id,
            &parsed.candidates,
            &validations,
        );
        let findings = finding_outcome.findings;
        let rejection_reasons = finding_outcome.rejection_reasons;
        for finding in &findings {
            let tool_call_id = ToolCallId(format!("{}-autonomous-review", finding.id));
            self.events.emit_runtime_with_context(
                RuntimeEventContext {
                    session_id: Some(SessionId(ORCHESTRATOR_SESSION_ID.to_string())),
                    tool_call_id: Some(tool_call_id.clone()),
                    finding_id: Some(finding.id.clone()),
                    ..RuntimeEventContext::default()
                },
                RuntimeEvent::FindingRecorded {
                    finding_id: finding.id.clone(),
                    session_id: SessionId(ORCHESTRATOR_SESSION_ID.to_string()),
                    tool_call_id,
                },
            );
        }
        let file_reviews = build_file_reviews(
            &self.snapshot,
            &parsed,
            &findings,
            report.output.as_deref().unwrap_or_default(),
        );
        let mut all_reports = Vec::new();
        all_reports.push(report);
        all_reports.extend(child_reports);
        for validation in validation_reports {
            if !all_reports
                .iter()
                .any(|existing| existing.session_id == validation.session_id)
            {
                all_reports.push(validation);
            }
        }
        let metrics = build_run_metrics(
            "autonomous_review",
            started,
            &self.tools,
            &self.snapshot.snapshot_id,
            &all_reports,
            findings.len(),
            parsed.candidates.len(),
            rejection_reasons,
            parsed.notes.len(),
            parsed.verdict.as_str(),
        );
        AutonomousReviewRunReport {
            metrics,
            findings,
            file_reviews,
        }
    }

    fn orchestrator_scope(&self, template: Option<SessionScope>) -> SessionScope {
        let mut scope = template.unwrap_or_else(|| {
            SessionScope::review_read_only(
                SessionId(ORCHESTRATOR_SESSION_ID.to_string()),
                Role::Generalist,
                "Autonomously review the changed code.",
                AgentBudget::planned_baseline(),
            )
        });
        scope.id = SessionId(ORCHESTRATOR_SESSION_ID.to_string());
        scope.role = Role::Generalist;
        scope.objective = "You are Muzen's autonomous review orchestrator. Review the diff as a senior code reviewer. Use direct read-only tools for decisive evidence. Batch independent search_code, explore_code, and validate_finding delegate calls when separate investigations can run in parallel. Publish no finding unless it is supported by raw code or diff evidence. Infer importance from the raw diff and repository context.".to_string();
        scope.snapshot_id = Some(self.snapshot.snapshot_id.clone());
        scope.budget = autonomous_orchestrator_budget(
            scope.budget,
            self.snapshot.manifest.changed_file_entries.len(),
        );
        if let Some(model_profile_id) = self.limits.orchestrator_model_profile_id.clone() {
            scope.model_profile_id = Some(model_profile_id);
        }
        scope.response_format = None;
        for tool in [SEARCH_CODE_TOOL, EXPLORE_CODE_TOOL, VALIDATE_FINDING_TOOL] {
            if let Ok(tool_id) = ToolId::parse(tool) {
                scope
                    .capabilities
                    .grant_tool(tool_id, ToolGrant::allow_review_read_only());
            }
        }
        let starter = neutral_starter_context(&self.snapshot, &scope.instructions);
        scope.instructions = vec![SessionInstruction {
            kind: "neutral_starter_context".to_string(),
            trusted: true,
            text: starter,
        }];
        scope
    }

    async fn run_mandatory_validations(
        &self,
        state: Arc<AutonomousDelegateState>,
        candidates: &[CandidateFinding],
        cancel: &CancellationToken,
    ) -> Vec<ValidationPacket> {
        let mut validations = Vec::new();
        for candidate in candidates {
            if cancel.is_cancelled() {
                break;
            }
            let task = DelegateTaskRequest {
                objective: format!("Validate candidate finding {}", candidate.id),
                prompt: serde_json::to_string(candidate)
                    .unwrap_or_else(|_| candidate.claim.clone()),
                candidate: Some(serde_json::to_value(candidate).unwrap_or(Value::Null)),
            };
            match run_child_delegate(
                Arc::clone(&state),
                DelegateTaskKind::ValidateFinding,
                task,
                cancel.child_token(),
            )
            .await
            {
                Ok(packet) => validations.push(ValidationPacket {
                    candidate_id: candidate.id.clone(),
                    status: packet.status,
                    summary: packet.summary,
                    artifact_id: packet.artifact_id,
                    child_session_id: Some(packet.session_id),
                }),
                Err(error) => validations.push(ValidationPacket {
                    candidate_id: candidate.id.clone(),
                    status: "insufficient".to_string(),
                    summary: format!("validation failed: {error}"),
                    artifact_id: None,
                    child_session_id: None,
                }),
            }
        }
        validations
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DelegateTaskKind {
    SearchCode,
    ExploreCode,
    ValidateFinding,
}

impl DelegateTaskKind {
    fn tool_name(self) -> &'static str {
        match self {
            Self::SearchCode => SEARCH_CODE_TOOL,
            Self::ExploreCode => EXPLORE_CODE_TOOL,
            Self::ValidateFinding => VALIDATE_FINDING_TOOL,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::SearchCode => "search",
            Self::ExploreCode => "explore",
            Self::ValidateFinding => "validate",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::SearchCode => {
                "Run a bounded deterministic-first repository search for code-review leads. Use for broad discovery; it returns ranked leads and omission counts, not final findings."
            }
            Self::ExploreCode => {
                "Spawn a read-only child review agent to investigate one concrete behavior, hypothesis, caller chain, or evidence question. It returns a structured evidence packet."
            }
            Self::ValidateFinding => {
                "Spawn an adversarial read-only validator for one candidate finding. It tries to refute the claim from raw code and diff evidence."
            }
        }
    }

    fn parameters_schema(self) -> Value {
        match self {
            Self::ValidateFinding => json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "objective": {"type": "string"},
                    "prompt": {"type": "string"},
                    "candidate": candidate_schema()
                },
                "required": ["objective", "prompt", "candidate"]
            }),
            _ => json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "objective": {"type": "string"},
                    "prompt": {"type": "string"}
                },
                "required": ["objective", "prompt"]
            }),
        }
    }

    fn child_prompt(self) -> &'static str {
        match self {
            Self::SearchCode => {
                "You are Muzen's search_code child. Run broad read-only discovery for the requested review objective. Prefer grep/glob/import/test tools. Return ranked leads and omitted counts. Do not publish findings."
            }
            Self::ExploreCode => {
                "You are Muzen's explore_code child. Investigate the requested behavior chain deeply with read-only tools. Return raw evidence, checked paths, candidate findings if any, and open questions."
            }
            Self::ValidateFinding => {
                "You are Muzen's validate_finding child. Be adversarial. Try to refute the candidate from raw code and diff evidence. Return supported only when the changed-code evidence establishes one concrete negative outcome. Return insufficient for correctness/no-issue observations, speculative claims, or bundled claims that combine unrelated behaviors instead of one failing invariant."
            }
        }
    }
}

struct DelegateToolHandler {
    host: AutonomousDelegateHost,
    kind: DelegateTaskKind,
}

#[async_trait]
impl CustomToolHandler for DelegateToolHandler {
    async fn execute(
        &self,
        context: CustomToolContext,
        args: Value,
        cancel: CancellationToken,
    ) -> RuntimeResult<CustomToolOutput> {
        if context.session_id.0 != ORCHESTRATOR_SESSION_ID {
            return Err(RuntimeError::InvalidInput(
                "delegate tools are only available to the review orchestrator".to_string(),
            ));
        }
        let state = self.host.state_for(&context.snapshot_id).ok_or_else(|| {
            RuntimeError::InvalidInput("delegate runtime is unavailable".to_string())
        })?;
        let request = parse_delegate_request(self.kind, args)?;
        let packet = run_child_delegate(state, self.kind, request, cancel).await?;
        let artifact_content =
            serde_json::to_string_pretty(&packet.full).unwrap_or_else(|_| packet.summary.clone());
        Ok(CustomToolOutput {
            data: Some(packet.compact),
            artifact: Some(CustomToolArtifact {
                key: ArtifactKey(stable_id(&[
                    &context.snapshot_id.0,
                    self.kind.tool_name(),
                    &packet.session_id,
                ])),
                content: artifact_content,
            }),
            limits: LimitInfo {
                truncated: false,
                output_bytes: packet.summary.len(),
                ..LimitInfo::default()
            },
        })
    }
}

#[derive(Debug, Clone)]
struct DelegateTaskRequest {
    objective: String,
    prompt: String,
    candidate: Option<Value>,
}

fn parse_delegate_request(
    kind: DelegateTaskKind,
    args: Value,
) -> RuntimeResult<DelegateTaskRequest> {
    let objective = args
        .get("objective")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RuntimeError::InvalidInput("delegate objective is required".to_string()))?
        .to_string();
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or(&objective)
        .to_string();
    let candidate = args.get("candidate").cloned();
    if kind == DelegateTaskKind::ValidateFinding && candidate.is_none() {
        return Err(RuntimeError::InvalidInput(
            "validate_finding requires candidate".to_string(),
        ));
    }
    Ok(DelegateTaskRequest {
        objective,
        prompt,
        candidate,
    })
}

#[derive(Debug, Clone)]
struct DelegateToolPacket {
    session_id: String,
    status: String,
    summary: String,
    artifact_id: Option<ArtifactId>,
    compact: Value,
    full: Value,
}

async fn run_child_delegate(
    state: Arc<AutonomousDelegateState>,
    kind: DelegateTaskKind,
    request: DelegateTaskRequest,
    cancel: CancellationToken,
) -> RuntimeResult<DelegateToolPacket> {
    let _permit = state
        .active_sessions
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| RuntimeError::Cancelled)?;
    if cancel.is_cancelled() {
        return Err(RuntimeError::Cancelled);
    }
    let child_id = state.next_child_id(kind)?;
    let task_packet = child_task_packet(kind, &request, &state.snapshot);
    let scope = SessionScope {
        id: child_id.clone(),
        role: Role::Generalist,
        objective: kind.child_prompt().to_string(),
        instructions: vec![SessionInstruction {
            kind: "delegate_task_packet".to_string(),
            text: task_packet.clone(),
            trusted: true,
        }],
        snapshot_id: Some(state.snapshot.snapshot_id.clone()),
        model_profile_id: state.model_profile_id_for(kind),
        response_format: None,
        capabilities: CapabilitySet::review_read_only(),
        budget: AgentBudget {
            max_turns: 8,
            max_tool_calls: 64,
            max_prompt_tokens: 96_000,
            max_output_tokens: 4_096,
            budget_source: crate::contracts::BudgetSource::AdaptiveReview,
        },
    };
    let report = run_session_loop(
        SessionRunConfig {
            state: Arc::clone(&state),
            scope,
            kind: SessionKind::Child(kind),
            task_packet: Some(task_packet),
            response_format: child_response_format(kind),
            final_instruction: child_final_instruction(kind),
        },
        cancel,
    )
    .await;
    let parsed = parse_child_packet(kind, report.output.as_deref());
    let artifact_content = serde_json::to_string_pretty(&parsed)
        .unwrap_or_else(|_| report.output.clone().unwrap_or_default());
    let artifact_id = state.tools.artifacts.insert(
        ArtifactKey(stable_id(&[
            &state.snapshot.snapshot_id.0,
            "delegate_child_packet",
            &child_id.0,
        ])),
        artifact_content,
    );
    let compact = compact_child_packet(kind, &child_id, &parsed, &artifact_id);
    let summary = parsed
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or(report.status.as_str())
        .to_string();
    let status = parsed
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or(report.status.as_str())
        .to_string();
    let packet = DelegateToolPacket {
        session_id: child_id.0.clone(),
        status,
        summary,
        artifact_id: Some(artifact_id.clone()),
        compact,
        full: parsed,
    };
    state.record_child_report(report);
    Ok(packet)
}

#[derive(Clone)]
struct SessionRunConfig {
    state: Arc<AutonomousDelegateState>,
    scope: SessionScope,
    kind: SessionKind,
    task_packet: Option<String>,
    response_format: ModelResponseFormat,
    final_instruction: String,
}

#[derive(Debug, Clone, Copy)]
enum SessionKind {
    Orchestrator,
    Child(DelegateTaskKind),
}

#[derive(Debug, Clone)]
struct SessionRunReport {
    session_id: String,
    completed: bool,
    status: String,
    output: Option<String>,
    model_calls: usize,
    model_metrics: ModelMetricsSnapshot,
    tool_counts: ToolCounts,
    tokens: TokenUsage,
    diagnostic: SessionCompletionDiagnostic,
}

async fn run_session_loop(config: SessionRunConfig, cancel: CancellationToken) -> SessionRunReport {
    let state = Arc::clone(&config.state);
    let scope = config.scope;
    state
        .events
        .emit_planned_runtime(state.policy.plan_session_started_runtime_event(&scope));
    let model = match state.model_router.client_for(&scope).await {
        Ok(model) => model,
        Err(_) => {
            state.events.emit_planned_runtime(
                state
                    .policy
                    .plan_session_finished_runtime_event(&scope, "failed"),
            );
            return terminal_report(&scope, "failed");
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
    let turn_guard = session_turn_guard(config.kind, &scope.budget);
    let mut next_turn_index = 0usize;

    for turn_index in 0..turn_guard {
        next_turn_index = turn_index + 1;
        if cancel.is_cancelled() {
            status = "cancelled".to_string();
            break;
        }
        let turn_id = TurnId(turn_index as u32);
        let evicted_tool_results =
            enforce_prompt_budget(&mut transcript, scope.budget.max_prompt_tokens);
        if evicted_tool_results > 0 {
            state
                .events
                .emit_planned_runtime(state.policy.plan_agent_trace_event(
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
        let final_turn = should_force_final_turn(
            config.kind,
            turn_index,
            turn_guard,
            tool_calls_used,
            &scope.budget,
        );
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
        state.events.emit_planned_runtime(
            state
                .policy
                .plan_model_started_runtime_event(&scope, turn_id),
        );
        state
            .events
            .emit_planned_runtime(state.policy.plan_agent_trace_event(
                &scope,
                Some(turn_id),
                "model_turn_prepared",
                format!(
                "prepared model turn with {} transcript item(s) and {} exposed tool(s)",
                transcript.len(),
                state
                    .policy
                    .tool_schemas_for_transcript(
                        &state.tools.registry,
                        &transcript,
                        &call_scope.capabilities
                    )
                    .len()
            ),
                json!({
                    "sessionKind": session_kind_name(config.kind),
                    "finalTurn": final_turn,
                    "turnGuard": turn_guard,
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
            &state.policy,
            &state.events,
            &state.limits,
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
        model_metrics.max_latency_ms = model_metrics.max_latency_ms.max(elapsed_ms(model_started));
        match turn {
            ModelTurn::Text { content, usage } => {
                record_usage(&mut tokens, &mut model_metrics, &*model, usage);
                state.events.emit_planned_runtime(
                    state
                        .policy
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
                state
                    .events
                    .emit_planned_runtime(state.policy.plan_model_completed_runtime_event(
                        &scope,
                        turn_id,
                        calls.len(),
                    ));
                if calls.is_empty() {
                    status = "done".to_string();
                    break;
                }
                transcript.push(ConversationItem::AssistantToolCalls {
                    calls: calls.clone(),
                });
                let results = ToolBatchRunner::new(
                    state.policy.as_ref(),
                    state.tools.as_ref(),
                    &state.events,
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
                    state.policy.as_ref(),
                    state.tools.as_ref(),
                    &state.events,
                    &state.review_revision_id,
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
    if status == "done" && !session_output_valid(config.kind, output.as_deref()) {
        for repair_index in 0..DEFAULT_SCHEMA_REPAIR_ATTEMPTS {
            if cancel.is_cancelled() {
                status = "cancelled".to_string();
                break;
            }
            let turn_id = TurnId((next_turn_index + repair_index) as u32);
            let mut repair_scope = scope.clone();
            repair_scope.capabilities.tool_grants.clear();
            repair_scope.response_format = Some(config.response_format.clone());
            transcript.push(ConversationItem::User {
                content: schema_repair_instruction(
                    config.kind,
                    repair_index + 1,
                    DEFAULT_SCHEMA_REPAIR_ATTEMPTS,
                ),
            });
            state
                .events
                .emit_planned_runtime(state.policy.plan_agent_trace_event(
                    &scope,
                    Some(turn_id),
                    "schema_repair",
                    format!("schema repair attempt {}", repair_index + 1),
                    json!({
                        "sessionKind": session_kind_name(config.kind),
                        "attempt": repair_index + 1,
                        "maxAttempts": DEFAULT_SCHEMA_REPAIR_ATTEMPTS,
                        "transcriptItems": transcript.len(),
                        "estimatedPromptTokens": estimate_prompt_tokens(&transcript),
                    }),
                ));
            let model_started = Instant::now();
            let outcome = complete_model_turn(
                &*model,
                &state.policy,
                &state.events,
                &state.limits,
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
                    status = if session_output_valid(config.kind, output.as_deref()) {
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
    if status == "done" && !session_output_valid(config.kind, output.as_deref()) {
        status = "incomplete".to_string();
    }
    let completed = status == "done";
    state.events.emit_planned_runtime(
        state
            .policy
            .plan_session_finished_runtime_event(&scope, &status),
    );
    SessionRunReport {
        diagnostic: session_diagnostic(&scope, completed, &status, model_calls, tool_counts),
        session_id: scope.id.0,
        completed,
        status,
        output,
        model_calls,
        model_metrics,
        tool_counts,
        tokens,
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

fn terminal_report(scope: &SessionScope, status: &str) -> SessionRunReport {
    SessionRunReport {
        session_id: scope.id.0.clone(),
        completed: false,
        status: status.to_string(),
        output: None,
        model_calls: 0,
        model_metrics: ModelMetricsSnapshot::default(),
        tool_counts: ToolCounts::default(),
        tokens: TokenUsage::default(),
        diagnostic: session_diagnostic(scope, false, status, 0, ToolCounts::default()),
    }
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
        completion_kind: Some("autonomous_review_session".to_string()),
        completion_summary: Some(status.to_string()),
        saw_diff: tool_counts.read_diff > 0,
        saw_file: tool_counts.read_file + tool_counts.read_file_range + tool_counts.read_head_file
            > 0,
        saw_search: tool_counts.search_text + tool_counts.list_files > 0,
        model_calls,
        tool_counts,
    }
}

fn session_kind_name(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Orchestrator => "orchestrator",
        SessionKind::Child(DelegateTaskKind::SearchCode) => "search_code",
        SessionKind::Child(DelegateTaskKind::ExploreCode) => "explore_code",
        SessionKind::Child(DelegateTaskKind::ValidateFinding) => "validate_finding",
    }
}

fn session_turn_guard(kind: SessionKind, budget: &AgentBudget) -> usize {
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

fn should_force_final_turn(
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

fn budgeted_tool_result_count(results: &[ToolResultEnvelope]) -> usize {
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

fn neutral_starter_context(snapshot: &RepoSnapshot, instructions: &[SessionInstruction]) -> String {
    let changed_files = snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| format!("- {}", file.summary))
        .collect::<Vec<_>>()
        .join("\n");
    let diff = truncate_chars(&snapshot.diff.content, 24_000);
    let instruction_text = instructions
        .iter()
        .map(|instruction| format!("- [{}] {}", instruction.kind, instruction.text))
        .collect::<Vec<_>>()
        .join("\n");
    let risk_inventory = format_diff_risk_inventory(&snapshot.diff.content, MAX_DIFF_RISK_ENTRIES);
    format!(
        "Neutral review starter context.\n\nChanged files:\n{}\n\nDiff risk inventory:\n{}\n\nRisk inventory instructions:\n- Treat entries as review obligations, not findings.\n- For each listed risk id, inspect enough raw code or diff evidence to support a bug, refute it, or list the unresolved question.\n- Changed async, promise, lazy-loading, callback, and side-effect aggregation code needs caller adaptation, awaited ordering, value shape, and error propagation checked before returning clean.\n- Use search_code or explore_code when independent risk entries can be investigated in parallel.\n\nRaw diff{}:\n{}\n\nProject/review instructions:\n{}",
        if changed_files.is_empty() { "(none)" } else { &changed_files },
        risk_inventory,
        if snapshot.diff.content.chars().count() > 24_000 {
            " (truncated; use diff/read tools for omitted hunks)"
        } else {
            ""
        },
        diff,
        if instruction_text.is_empty() {
            "(none)"
        } else {
            &instruction_text
        }
    )
}

fn autonomous_orchestrator_budget(
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffRiskEntry {
    id: String,
    path: String,
    line: Option<usize>,
    category: &'static str,
    code: String,
    obligation: &'static str,
}

fn format_diff_risk_inventory(diff: &str, max_entries: usize) -> String {
    let entries = diff_risk_inventory(diff, max_entries);
    if entries.is_empty() {
        return "(none detected by the heuristic inventory; still review all changed behavior)"
            .to_string();
    }
    entries
        .iter()
        .map(|entry| {
            let location = entry
                .line
                .map(|line| format!("{}:{line}", entry.path))
                .unwrap_or_else(|| entry.path.clone());
            format!(
                "- {} {} [{}] `{}`: {}",
                entry.id, location, entry.category, entry.code, entry.obligation
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn diff_risk_inventory(diff: &str, max_entries: usize) -> Vec<DiffRiskEntry> {
    let mut entries = Vec::new();
    let mut current_path = String::new();
    let mut head_line = None::<usize>;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = path.to_string();
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_path.clear();
            continue;
        }
        if line.starts_with("@@") {
            head_line = parse_hunk_head_start(line);
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            let changed_line = head_line;
            if let Some(line) = head_line.as_mut() {
                *line += 1;
            }
            if current_path.is_empty() {
                continue;
            }
            let added = line.trim_start_matches('+').trim();
            for (category, obligation) in risk_categories_for_added_line(added) {
                if entries.len() >= max_entries {
                    return entries;
                }
                entries.push(DiffRiskEntry {
                    id: format!("R{}", entries.len() + 1),
                    path: current_path.clone(),
                    line: changed_line,
                    category,
                    code: truncate_chars(added, 140),
                    obligation,
                });
            }
        } else if line.starts_with(' ') {
            if let Some(line) = head_line.as_mut() {
                *line += 1;
            }
        }
    }
    entries
}

fn parse_hunk_head_start(line: &str) -> Option<usize> {
    let plus = line.split_whitespace().find(|part| part.starts_with('+'))?;
    let digits = plus
        .trim_start_matches('+')
        .split(',')
        .next()
        .unwrap_or_default();
    digits.parse().ok()
}

fn risk_categories_for_added_line(line: &str) -> Vec<(&'static str, &'static str)> {
    let mut categories = Vec::new();
    let lowered = line.to_ascii_lowercase();
    let callback_async = [
        ".foreach(async",
        ".map(async",
        ".filter(async",
        ".reduce(async",
        ".flatmap(async",
        ".some(async",
        ".every(async",
    ]
    .iter()
    .any(|pattern| lowered.contains(pattern));
    if callback_async {
        categories.push((
            "async_callback",
            "Verify the outer control flow awaits callback-produced work and side effects cannot complete after the caller reports success.",
        ));
    }
    if lowered.contains("await ")
        || lowered.contains(" async ")
        || lowered.starts_with("async ")
        || lowered.contains("promise<")
        || lowered.contains("promise.")
        || lowered.contains("new promise")
    {
        categories.push((
            "async_boundary",
            "Verify callers, return shape, ordering, cancellation, and error propagation across the new async boundary.",
        ));
    }
    if lowered.contains("import(") || lowered.contains("await appstore[") {
        categories.push((
            "lazy_module_loading",
            "Verify module lookup failures, rejected loads, and changed value shape are handled by consumers.",
        ));
    }
    if lowered.contains("promise.all")
        || lowered.contains(".push(")
            && (lowered.contains("promise")
                || lowered.contains("delete")
                || lowered.contains("update")
                || lowered.contains("send")
                || lowered.contains("write")
                || lowered.contains("create"))
    {
        categories.push((
            "side_effect_aggregation",
            "Verify every produced side-effect promise is included in the awaited aggregate before state changes or success returns.",
        ));
    }
    categories
}

fn child_task_packet(
    kind: DelegateTaskKind,
    request: &DelegateTaskRequest,
    snapshot: &RepoSnapshot,
) -> String {
    let changed_files = snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| format!("- {}", file.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Task type: {}\nObjective: {}\nPrompt: {}\nCandidate: {}\n\nChanged files:\n{}",
        kind.tool_name(),
        request.objective,
        request.prompt,
        request
            .candidate
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_else(|| "none".to_string()),
        if changed_files.is_empty() {
            "(none)"
        } else {
            &changed_files
        }
    )
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str("\n[truncated]");
    }
    output
}

fn build_run_metrics(
    runtime_name: &'static str,
    started: Instant,
    tools: &ToolEngine,
    snapshot_id: &SnapshotId,
    reports: &[SessionRunReport],
    findings: usize,
    candidate_count: usize,
    rejection_reasons: BTreeMap<String, usize>,
    note_count: usize,
    verdict: &str,
) -> ConcurrentRunReport {
    let mut completed_sessions = 0usize;
    let mut model_calls = 0usize;
    let mut model_metrics = ModelMetricsSnapshot::default();
    let mut tool_counts = ToolCounts::default();
    let mut tokens = TokenUsage::default();
    let mut diagnostics = Vec::new();
    for report in reports {
        if report.completed {
            completed_sessions += 1;
        }
        model_calls += report.model_calls;
        add_model_metrics(&mut model_metrics, &report.model_metrics);
        tool_counts.add(report.tool_counts);
        tokens.add(report.tokens);
        diagnostics.push(report.diagnostic.clone());
    }
    diagnostics.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    let (artifacts, artifact_bytes) = tools.artifacts.stats();
    let elapsed_ms = elapsed_ms(started);
    let sessions = reports.len();
    let mut quality_diagnostics = ReviewQualityDiagnostics::default();
    quality_diagnostics.sessions_run = sessions;
    quality_diagnostics.candidate_findings = candidate_count;
    quality_diagnostics.rejected_candidates = rejection_reasons.values().sum();
    quality_diagnostics.rejection_reasons = rejection_reasons;
    quality_diagnostics
        .rejection_reasons
        .insert(format!("verdict:{verdict}"), note_count);
    ConcurrentRunReport {
        runtime: runtime_name,
        sessions,
        completed_sessions,
        model_calls,
        tool_calls: tool_counts.total(),
        tool_counts,
        findings,
        publishable_findings: findings,
        quality_diagnostics,
        elapsed_ms,
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        total_tokens: tokens.total_tokens,
        cached_input_tokens: tokens.cached_input_tokens,
        artifacts,
        artifact_bytes,
        counters: tools.snapshot_counters(),
        tool_metrics: tools.snapshot_tool_metrics(),
        provider_health: tools.snapshot_provider_health(),
        snapshot_metrics: vec![SnapshotMetricsSnapshot {
            snapshot_id: snapshot_id.clone(),
            sessions,
            completed_sessions,
            model_calls,
            tool_calls: tool_counts.total(),
            artifacts,
            artifact_bytes,
            elapsed_ms,
        }],
        model_metrics,
        completion_diagnostics: diagnostics,
        benchmark_valid: sessions > 0 && completed_sessions > 0,
        benchmark_failures: Vec::new(),
    }
}

fn orchestrator_response_format() -> ModelResponseFormat {
    ModelResponseFormat::json_schema(
        "muzen_autonomous_review_result_v1",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["verdict", "summary", "candidates", "notes", "completeness"],
            "properties": {
                "verdict": {"type": "string", "enum": ["issues_found", "clean", "incomplete"]},
                "summary": {"type": "string"},
                "candidates": {"type": "array", "items": candidate_schema()},
                "notes": {"type": "array", "items": {"type": "string"}},
                "completeness": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "reviewedChangedFiles",
                        "reviewedRiskEntries",
                        "unreviewedRiskEntries",
                        "unresolvedQuestions",
                        "incompleteReasons",
                        "ignoredChildCandidates"
                    ],
                    "properties": {
                        "reviewedChangedFiles": {"type": "array", "items": {"type": "string"}},
                        "reviewedRiskEntries": {"type": "array", "items": {"type": "string"}},
                        "unreviewedRiskEntries": {"type": "array", "items": {"type": "string"}},
                        "unresolvedQuestions": {"type": "array", "items": {"type": "string"}},
                        "incompleteReasons": {"type": "array", "items": {"type": "string"}},
                        "ignoredChildCandidates": {"type": "array", "items": {"type": "string"}}
                    }
                }
            }
        }),
    )
}

fn child_response_format(kind: DelegateTaskKind) -> ModelResponseFormat {
    ModelResponseFormat::json_schema(
        format!("muzen_{}_packet_v1", kind.tool_name()),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "status",
                "summary",
                "checkedPaths",
                "evidence",
                "openQuestions",
                "suggestedNextSearches",
                "candidateFindings"
            ],
            "properties": {
                "status": {"type": "string", "enum": ["supported", "refuted", "insufficient", "needs_more_evidence"]},
                "summary": {"type": "string"},
                "checkedPaths": {"type": "array", "items": {"type": "string"}},
                "evidence": {"type": "array", "items": evidence_packet_schema()},
                "openQuestions": {"type": "array", "items": {"type": "string"}},
                "suggestedNextSearches": {"type": "array", "items": {"type": "string"}},
                "candidateFindings": {"type": "array", "items": candidate_schema()}
            }
        }),
    )
}

fn candidate_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "id",
            "title",
            "claim",
            "severity",
            "path",
            "startLine",
            "endLine",
            "behaviorBefore",
            "behaviorAfter",
            "evidenceArtifactIds",
            "relatedPaths"
        ],
        "properties": {
            "id": {"type": "string"},
            "title": {"type": "string"},
            "claim": {"type": "string"},
            "severity": {"type": ["string", "null"]},
            "path": {"type": "string"},
            "startLine": {"type": ["integer", "null"]},
            "endLine": {"type": ["integer", "null"]},
            "behaviorBefore": {"type": ["string", "null"]},
            "behaviorAfter": {"type": ["string", "null"]},
            "evidenceArtifactIds": {"type": "array", "items": {"type": "string"}},
            "relatedPaths": {"type": "array", "items": {"type": "string"}}
        }
    })
}

fn evidence_packet_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "path",
            "startLine",
            "endLine",
            "snippet",
            "artifactId",
            "whyItMatters"
        ],
        "properties": {
            "path": {"type": ["string", "null"]},
            "startLine": {"type": ["integer", "null"]},
            "endLine": {"type": ["integer", "null"]},
            "snippet": {"type": ["string", "null"]},
            "artifactId": {"type": ["string", "null"]},
            "whyItMatters": {"type": ["string", "null"]}
        }
    })
}

fn orchestrator_final_instruction() -> String {
    "Return the final autonomous review result now as strict JSON. Include candidate findings only for concrete changed-code bugs supported by raw code or diff evidence. Each candidate must describe exactly one failing invariant and one concrete negative outcome; split unrelated behaviors into separate candidates. Correctness/no-issue observations, intended behavior, and suspicious but insufficient observations belong in notes or completeness.incompleteReasons, not candidates. Account for every diff risk inventory id in completeness.reviewedRiskEntries or completeness.unreviewedRiskEntries. If a material risk entry remains unreviewed, use verdict=incomplete.".to_string()
}

fn child_final_instruction(kind: DelegateTaskKind) -> String {
    if kind == DelegateTaskKind::ValidateFinding {
        return "Return the final validate_finding packet now as strict JSON. Use supported only when raw code/diff evidence establishes one concrete negative changed-code outcome. Use insufficient for no-issue observations, speculative claims, and bundled multi-behavior claims.".to_string();
    }
    format!(
        "Return the final {} packet now as strict JSON. Use supported only when raw code/diff evidence closes the objective.",
        kind.tool_name()
    )
}

fn schema_repair_instruction(kind: SessionKind, attempt: usize, max_attempts: usize) -> String {
    format!(
        "Your previous final answer did not match the required {} JSON schema. Return corrected strict JSON only. Repair attempt {attempt}/{max_attempts}.",
        session_kind_name(kind)
    )
}

fn session_output_valid(kind: SessionKind, output: Option<&str>) -> bool {
    let Some(output) = output else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    match kind {
        SessionKind::Orchestrator => {
            value.get("verdict").and_then(Value::as_str).is_some()
                && value.get("summary").and_then(Value::as_str).is_some()
                && value.get("candidates").and_then(Value::as_array).is_some()
                && value.get("notes").and_then(Value::as_array).is_some()
                && value
                    .get("completeness")
                    .and_then(Value::as_object)
                    .is_some()
        }
        SessionKind::Child(_) => {
            value.get("status").and_then(Value::as_str).is_some()
                && value.get("summary").and_then(Value::as_str).is_some()
                && value
                    .get("checkedPaths")
                    .and_then(Value::as_array)
                    .is_some()
                && value.get("evidence").and_then(Value::as_array).is_some()
                && value
                    .get("openQuestions")
                    .and_then(Value::as_array)
                    .is_some()
                && value
                    .get("candidateFindings")
                    .and_then(Value::as_array)
                    .is_some()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidateFinding {
    id: String,
    title: String,
    claim: String,
    #[serde(default)]
    severity: Option<String>,
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    behavior_before: Option<String>,
    #[serde(default)]
    behavior_after: Option<String>,
    #[serde(default)]
    evidence_artifact_ids: Vec<String>,
    #[serde(default)]
    related_paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct ParsedOrchestratorOutput {
    verdict: String,
    summary: String,
    candidates: Vec<CandidateFinding>,
    notes: Vec<String>,
    completeness: Value,
}

fn parse_orchestrator_output(output: Option<&str>) -> ParsedOrchestratorOutput {
    let value = output
        .and_then(|output| serde_json::from_str::<Value>(output).ok())
        .unwrap_or_else(|| json!({}));
    let candidates = value
        .get("candidates")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<CandidateFinding>(item.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let notes = value
        .get("notes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ParsedOrchestratorOutput {
        verdict: value
            .get("verdict")
            .and_then(Value::as_str)
            .unwrap_or(if candidates.is_empty() {
                "incomplete"
            } else {
                "issues_found"
            })
            .to_string(),
        summary: value
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("autonomous review completed")
            .to_string(),
        candidates,
        notes,
        completeness: value
            .get("completeness")
            .cloned()
            .unwrap_or_else(|| json!({})),
    }
}

fn parse_child_packet(kind: DelegateTaskKind, output: Option<&str>) -> Value {
    output
        .and_then(|output| serde_json::from_str::<Value>(output).ok())
        .unwrap_or_else(|| {
            json!({
                "status": "insufficient",
                "summary": format!("{} child did not return a valid packet", kind.tool_name()),
                "checkedPaths": [],
                "evidence": [],
                "openQuestions": ["child output was missing or malformed"],
                "suggestedNextSearches": [],
                "candidateFindings": []
            })
        })
}

fn compact_child_packet(
    kind: DelegateTaskKind,
    session_id: &SessionId,
    packet: &Value,
    artifact_id: &ArtifactId,
) -> Value {
    json!({
        "taskType": kind.tool_name(),
        "sessionId": session_id.0,
        "status": packet.get("status").cloned().unwrap_or_else(|| json!("insufficient")),
        "summary": packet.get("summary").cloned().unwrap_or_else(|| json!("")),
        "checkedPaths": compact_string_array(packet.get("checkedPaths"), 40),
        "candidateCount": packet.get("candidateFindings").and_then(Value::as_array).map_or(0, Vec::len),
        "openQuestionCount": packet.get("openQuestions").and_then(Value::as_array).map_or(0, Vec::len),
        "artifactId": artifact_id.0,
    })
}

fn compact_string_array(value: Option<&Value>, max_items: usize) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .take(max_items)
                .map(|item| truncate_chars(item, 240))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct ValidationPacket {
    candidate_id: String,
    status: String,
    summary: String,
    artifact_id: Option<ArtifactId>,
    child_session_id: Option<String>,
}

struct FindingBuildOutcome {
    findings: Vec<FindingV1>,
    rejection_reasons: BTreeMap<String, usize>,
}

fn build_findings(
    tools: &ToolEngine,
    snapshot: &RepoSnapshot,
    review_revision_id: &str,
    candidates: &[CandidateFinding],
    validations: &[ValidationPacket],
) -> FindingBuildOutcome {
    let validation_by_candidate = validations
        .iter()
        .map(|validation| (validation.candidate_id.as_str(), validation))
        .collect::<HashMap<_, _>>();
    let changed_paths = changed_paths_for_snapshot(snapshot);
    let changed_ranges = changed_line_ranges_by_path(&snapshot.diff.content);
    let mut seen_candidate_keys = BTreeSet::new();
    let mut findings = Vec::new();
    let mut rejection_reasons = BTreeMap::new();
    for candidate in candidates {
        let key = stable_id(&[&candidate.path, &candidate.claim, &candidate.title]);
        if !seen_candidate_keys.insert(key) {
            record_candidate_rejection(&mut rejection_reasons, "duplicate_candidate");
            continue;
        }
        let Some(validation) = validation_by_candidate.get(candidate.id.as_str()) else {
            record_candidate_rejection(&mut rejection_reasons, "missing_validation");
            continue;
        };
        if !validation.status.trim().eq_ignore_ascii_case("supported") {
            record_candidate_rejection(
                &mut rejection_reasons,
                validation_rejection_reason(&validation.status),
            );
            continue;
        }
        if let Some(reason) =
            autonomous_candidate_rejection_reason(candidate, &changed_paths, &changed_ranges)
        {
            record_candidate_rejection(&mut rejection_reasons, reason);
            continue;
        }
        let validation_has_summary = !validation.summary.trim().is_empty();
        findings.push(candidate_to_finding(
            tools,
            review_revision_id,
            candidate,
            validation,
            validation_has_summary,
        ));
    }
    FindingBuildOutcome {
        findings,
        rejection_reasons,
    }
}

fn record_candidate_rejection(
    rejection_reasons: &mut BTreeMap<String, usize>,
    reason: impl Into<String>,
) {
    *rejection_reasons.entry(reason.into()).or_insert(0) += 1;
}

fn validation_rejection_reason(status: &str) -> String {
    let normalized = status.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "refuted" => "validator_refuted".to_string(),
        "insufficient" => "validator_insufficient".to_string(),
        "needs_more_evidence" => "validator_needs_more_evidence".to_string(),
        "" => "validator_missing_status".to_string(),
        _ => format!("validator_{normalized}"),
    }
}

fn changed_paths_for_snapshot(snapshot: &RepoSnapshot) -> BTreeSet<String> {
    snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| file.rel_path.display())
        .collect()
}

fn autonomous_candidate_rejection_reason(
    candidate: &CandidateFinding,
    changed_paths: &BTreeSet<String>,
    changed_ranges: &BTreeMap<String, Vec<(usize, usize)>>,
) -> Option<&'static str> {
    let path = candidate.path.trim();
    let title = candidate.title.trim();
    let claim = candidate.claim.trim();
    if path.is_empty() {
        return Some("invalid_path");
    }
    if !changed_paths.contains(path) {
        return Some("unchanged_path");
    }
    if title.is_empty() || claim.is_empty() {
        return Some("empty_title_or_claim");
    }
    let Some((start_line, end_line)) = candidate.start_line.zip(candidate.end_line) else {
        return Some("missing_line_range");
    };
    if let Some(ranges) = changed_ranges.get(path) {
        if !ranges
            .iter()
            .any(|(start, end)| ranges_overlap(start_line, end_line.max(start_line), *start, *end))
        {
            return Some("line_range_not_changed");
        }
    } else if !changed_ranges.is_empty() {
        return Some("path_has_no_changed_lines");
    }

    let behavior_before = candidate.behavior_before.as_deref().unwrap_or_default();
    let behavior_after = candidate.behavior_after.as_deref().unwrap_or_default();
    if behavior_comparison_missing(behavior_before, behavior_after) {
        return Some("missing_behavior_comparison");
    }
    let title_and_claim = format!("{title} {claim}");
    if is_non_finding_text(&title_and_claim)
        || is_non_bug_observation_text(&title_and_claim)
        || is_counterfactual_support_observation_text(&title_and_claim)
    {
        return Some("non_finding_text");
    }
    if is_bundled_finding_text(&title_and_claim) {
        return Some("bundled_claim");
    }
    if is_speculative_finding(&title_and_claim, behavior_before, behavior_after) {
        return Some("speculative_claim");
    }
    let full_text = format!("{title_and_claim} {behavior_before} {behavior_after}");
    if !describes_negative_outcome(&full_text) {
        return Some("missing_negative_outcome");
    }
    None
}

fn behavior_comparison_missing(behavior_before: &str, behavior_after: &str) -> bool {
    behavior_text_is_vague(behavior_before) || behavior_text_is_vague(behavior_after)
}

fn behavior_text_is_vague(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    let normalized = trimmed.to_ascii_lowercase();
    [
        "not inspected",
        "not compared",
        "unknown",
        "unavailable",
        "no behavior",
        "no behavioral",
        "unclear",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_speculative_finding(text: &str, behavior_before: &str, behavior_after: &str) -> bool {
    if is_hypothetical_finding_text(text) {
        return true;
    }
    is_hedged_finding_text(text)
        && (behavior_before.trim().is_empty() || behavior_after.trim().is_empty())
}

fn is_hypothetical_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        " if later ",
        "if a future",
        "future change",
        "could become",
        "may become",
        "hypothetical",
        "speculative",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_hedged_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        " likely ",
        " may ",
        " might ",
        "potential",
        "appears to",
        " probably",
        " fragile",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_non_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "no additional",
        "no actionable",
        "no concrete",
        "no new incompatible",
        "no supported bug",
        "not a bug",
        "does not show",
        "no further issue",
        "no issue",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_non_bug_observation_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    let has_failure_signal = [
        "wrong",
        "fails",
        "failure",
        "broken",
        "missing",
        "undefined",
        "throws",
        "crash",
        "invalid",
        "incorrect",
        "mismatch",
        "does not expose",
        "unparsed",
        "cannot",
        "never",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    let has_clean_signal = [
        "correct",
        "preserve",
        "preserves",
        "continues",
        "still parses",
        "still consumes",
        "still returns",
        "still persists",
        "still writes",
        "consistent",
        "no bug",
        "no issue",
        "no new incompatible",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    has_clean_signal && !has_failure_signal
}

fn is_counterfactual_support_observation_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    if normalized.contains("only safe because") || normalized.contains("safe because") {
        return true;
    }
    let support_scaffolding = [
        "added to support",
        "introduced to support",
        "required to support",
        "needed to support",
        "supports ",
        "supporting ",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    support_scaffolding && (normalized.contains("otherwise ") || normalized.contains("would break"))
}

fn is_bundled_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "multiple issues",
        "multiple bugs",
        "several issues",
        "several bugs",
        "two issues",
        "two bugs",
        "first issue",
        "second issue",
        "another issue",
        "another bug",
        "separate issue",
        "separate bug",
        "independent issue",
        "independent bug",
        " and also ",
        "; also ",
        ". also ",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn describes_negative_outcome(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    if [
        "data loss",
        "does not",
        "can no longer",
        "returns success before",
        "reports success before",
        "outside the",
        "outside tenant",
        "other tenant",
        "unrelated record",
        "unrelated row",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return true;
    }
    let tokens = normalized
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .collect::<BTreeSet<_>>();
    [
        "wrong",
        "incorrect",
        "invalid",
        "fails",
        "failed",
        "failure",
        "failing",
        "breaks",
        "broken",
        "missing",
        "drops",
        "dropped",
        "skips",
        "skipped",
        "omits",
        "omitted",
        "loses",
        "lost",
        "leaks",
        "leaked",
        "throws",
        "throw",
        "thrown",
        "crash",
        "crashes",
        "crashed",
        "panic",
        "panics",
        "undefined",
        "null",
        "cannot",
        "never",
        "unhandled",
        "unawaited",
        "race",
        "deadlock",
        "timeout",
        "hangs",
        "hanging",
        "stale",
        "duplicate",
        "duplicated",
        "outside",
        "unrelated",
        "unauthorized",
        "forbidden",
        "bypass",
        "bypasses",
        "bypassed",
        "unscoped",
        "unchecked",
        "mismatch",
        "regression",
        "corrupt",
    ]
    .iter()
    .any(|needle| tokens.contains(needle))
}

fn changed_line_ranges_by_path(diff: &str) -> BTreeMap<String, Vec<(usize, usize)>> {
    let mut current_path: Option<String> = None;
    let mut current_new_line: Option<usize> = None;
    let mut ranges: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            current_new_line = None;
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_path = None;
            current_new_line = None;
            continue;
        }
        let Some(path) = current_path.as_ref() else {
            continue;
        };
        if line.starts_with("@@") {
            current_new_line = parse_hunk_head_start(line);
            continue;
        }
        let Some(new_line) = current_new_line else {
            continue;
        };
        if line.starts_with('+') && !line.starts_with("+++") {
            push_line_range(ranges.entry(path.clone()).or_default(), new_line);
            current_new_line = Some(new_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            continue;
        } else {
            current_new_line = Some(new_line + 1);
        }
    }
    ranges
}

fn push_line_range(ranges: &mut Vec<(usize, usize)>, line: usize) {
    if let Some((_, end)) = ranges.last_mut() {
        if line == *end + 1 {
            *end = line;
            return;
        }
    }
    ranges.push((line, line));
}

fn ranges_overlap(
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> bool {
    left_start <= right_end && right_start <= left_end
}

fn candidate_to_finding(
    tools: &ToolEngine,
    review_revision_id: &str,
    candidate: &CandidateFinding,
    validation: &ValidationPacket,
    validation_has_summary: bool,
) -> FindingV1 {
    let artifact_ids = candidate
        .evidence_artifact_ids
        .iter()
        .filter_map(|id| {
            let artifact_id = ArtifactId(id.clone());
            tools
                .artifacts
                .get(&artifact_id)
                .map(|artifact| (artifact_id, artifact))
        })
        .collect::<Vec<_>>();
    let fallback_artifact = validation.artifact_id.as_ref().and_then(|artifact_id| {
        tools
            .artifacts
            .get(artifact_id)
            .map(|artifact| (artifact_id.clone(), artifact))
    });
    let evidence = if artifact_ids.is_empty() {
        fallback_artifact.into_iter().collect::<Vec<_>>()
    } else {
        artifact_ids
    }
    .into_iter()
    .enumerate()
    .map(|(index, (artifact_id, artifact))| EvidenceRefV1 {
        evidence_id: format!("ev_{}", stable_id(&[&candidate.id, &index.to_string()])),
        artifact_id: artifact_id.0.clone(),
        kind: ArtifactKind::ToolSummary,
        revision: EvidenceRevision::Review,
        revision_id: review_revision_id.to_string(),
        location: EvidenceLocationV1::SinglePath {
            path: candidate.path.clone(),
        },
        line_range: candidate
            .start_line
            .zip(candidate.end_line)
            .map(|(start_line, end_line)| LineRangeV1 {
                start_line,
                end_line,
            }),
        byte_range: None,
        diff_anchor: None,
        content_hash: artifact.content_hash,
        redaction: RedactionMetadataV1 {
            redaction_state: RedactionState::Partial,
            redaction_policy_id: "runtime-redactor-v1".to_string(),
            contains_repo_content: true,
            contains_prompt_content: false,
            contains_model_output: true,
            contains_secret_material: false,
        },
        producing_tool_call_id: validation
            .child_session_id
            .clone()
            .unwrap_or_else(|| "mandatory-validation".to_string()),
    })
    .collect::<Vec<_>>();
    FindingV1 {
        id: if candidate.id.trim().is_empty() {
            format!(
                "finding_{}",
                stable_id(&[&candidate.path, &candidate.claim])
            )
        } else {
            candidate.id.clone()
        },
        title: candidate.title.clone(),
        claim: candidate.claim.clone(),
        severity: parse_severity(candidate.severity.as_deref()),
        confidence: if validation_has_summary { 0.85 } else { 0.8 },
        validation_status: ValidationStatus::Validated,
        report_status: ReportStatus::Included,
        publishability: FindingPublishability::Publishable,
        challenge_status: ChallengeStatus::Confirmed,
        evidence,
        file_refs: std::iter::once(candidate.path.clone())
            .chain(candidate.related_paths.iter().cloned())
            .map(|path| EvidenceLocationV1::SinglePath { path })
            .collect(),
        location_line_range: candidate.start_line.zip(candidate.end_line).map(
            |(start_line, end_line)| LineRangeV1 {
                start_line,
                end_line,
            },
        ),
        discovered_by: vec![ORCHESTRATOR_SESSION_ID.to_string()],
        challenged_by: validation
            .child_session_id
            .clone()
            .map(|id| vec![id])
            .unwrap_or_default(),
    }
}

fn parse_severity(value: Option<&str>) -> FindingSeverity {
    match value
        .unwrap_or("medium")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "blocker" => FindingSeverity::Blocker,
        "high" => FindingSeverity::High,
        "low" => FindingSeverity::Low,
        "nit" => FindingSeverity::Nit,
        _ => FindingSeverity::Medium,
    }
}

fn build_file_reviews(
    snapshot: &RepoSnapshot,
    parsed: &ParsedOrchestratorOutput,
    findings: &[FindingV1],
    output: &str,
) -> Vec<FileReviewV1> {
    let completeness_incomplete = parsed
        .completeness
        .get("incompleteReasons")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    let unreviewed_risks = parsed
        .completeness
        .get("unreviewedRiskEntries")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    let unpublished_candidates = parsed.candidates.len() > findings.len();
    let incomplete = parsed.verdict == "incomplete"
        || completeness_incomplete
        || unreviewed_risks
        || unpublished_candidates;
    let finding_paths = findings
        .iter()
        .flat_map(|finding| finding.file_refs.iter())
        .filter_map(|location| match location {
            EvidenceLocationV1::SinglePath { path } => Some(path.clone()),
            EvidenceLocationV1::Rename { new_path, .. } => Some(new_path.clone()),
        })
        .collect::<std::collections::BTreeSet<_>>();
    snapshot
        .manifest
        .changed_file_entries
        .iter()
        .map(|file| {
            let path = file.rel_path.display();
            let issue_found = finding_paths.contains(&path);
            let review_verdict = if issue_found {
                ReviewVerdict::IssueFound
            } else if incomplete {
                ReviewVerdict::NeedsReview
            } else {
                ReviewVerdict::Clean
            };
            FileReviewV1 {
                path: path.clone(),
                verdict: match review_verdict {
                    ReviewVerdict::IssueFound => "issue_found",
                    ReviewVerdict::NeedsReview => "needs_review",
                    ReviewVerdict::Clean => "clean",
                }
                .to_string(),
                coverage: if incomplete {
                    ReviewCoverage::Insufficient
                } else {
                    ReviewCoverage::Standard
                },
                review_verdict,
                summary: truncate_chars(
                    if parsed.summary.is_empty() {
                        output
                    } else {
                        &parsed.summary
                    },
                    500,
                ),
                related_paths: Vec::new(),
                evidence_artifact_ids: Vec::new(),
                evidence_count: 0,
                session_id: ORCHESTRATOR_SESSION_ID.to_string(),
                unit_id: "autonomous-review".to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
