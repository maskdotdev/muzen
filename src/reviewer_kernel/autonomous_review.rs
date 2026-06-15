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

mod diff_risk;
mod finding_filters;
mod schemas;

#[cfg(test)]
use diff_risk::diff_risk_inventory;
use diff_risk::{format_diff_risk_inventory, truncate_chars};
use finding_filters::{autonomous_candidate_rejection_reason, changed_line_ranges_by_path};
use schemas::{
    candidate_schema, child_final_instruction, child_response_format,
    orchestrator_final_instruction, orchestrator_response_format, schema_repair_instruction,
    session_output_valid,
};

use crate::reviewer_kernel::agent_loop::{AgentLoopConfig, AgentLoopReport, AgentLoopRuntime};
use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::model::ConcurrentModelRouter;
use crate::reviewer_kernel::policy::ReviewerPolicy;
use crate::reviewer_kernel::review_contract::{
    AgentBudget, ArtifactKind, BudgetSource, ChallengeStatus, EvidenceLocationV1, EvidenceRefV1,
    EvidenceRevision, FileReviewV1, FindingPublishability, FindingSeverity, FindingV1, LineRangeV1,
    RedactionMetadataV1, RedactionState, ReportStatus, ReviewCoverage, ReviewVerdict, Role,
    TokenUsage, ToolCounts, ValidationStatus,
};
use crate::reviewer_kernel::session_metrics::add_model_metrics;
use crate::reviewer_kernel::session_metrics::elapsed_ms;
use crate::reviewer_kernel::tool_engine::registry::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOutput, ToolRegistry,
};
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::workspace::RepoSnapshot;

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
    child_reports: Mutex<Vec<AgentLoopReport>>,
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

    fn record_child_report(&self, report: AgentLoopReport) {
        self.child_reports.lock().push(report);
    }

    fn child_reports(&self) -> Vec<AgentLoopReport> {
        self.child_reports.lock().clone()
    }

    fn agent_loop_runtime(&self) -> AgentLoopRuntime {
        AgentLoopRuntime {
            model_router: Arc::clone(&self.model_router),
            tools: Arc::clone(&self.tools),
            policy: Arc::clone(&self.policy),
            limits: Arc::clone(&self.limits),
            review_revision_id: self.review_revision_id.clone(),
            events: self.events.clone(),
        }
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
            Arc::clone(&state),
            SessionRunConfig {
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
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::AdaptiveReview,
        },
    };
    let report = run_session_loop(
        Arc::clone(&state),
        SessionRunConfig {
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

async fn run_session_loop(
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

fn build_run_metrics(
    runtime_name: &'static str,
    started: Instant,
    tools: &ToolEngine,
    snapshot_id: &SnapshotId,
    reports: &[AgentLoopReport],
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
