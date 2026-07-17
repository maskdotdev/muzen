use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::reviewer_kernel::agent_loop::{AgentLoopReport, AgentLoopRuntime};
use crate::reviewer_kernel::dispatch::RuntimeEventDispatcher;
use crate::reviewer_kernel::kernel_types::*;
use crate::reviewer_kernel::model::ConcurrentModelRouter;
use crate::reviewer_kernel::policy::ReviewerPolicy;
use crate::reviewer_kernel::review_contract::{AgentBudget, BudgetSource, Role};
use crate::reviewer_kernel::tool_engine::registry::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOutput, ToolRegistry,
};
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::workspace::RepoSnapshot;

use super::findings::{
    compact_child_packet, invalid_child_packet, parse_child_packet, CandidateFinding,
};
use super::prompts::child_task_packet;
use super::schemas::{child_final_instruction, child_response_format};
use super::sessions::{run_session_loop, SessionKind, SessionRunConfig};
use super::tasks::{parse_delegate_request, DelegateTaskKind, DelegateTaskRequest};
use super::{AutonomousReviewRuntime, DEFAULT_MAX_CHILD_SESSIONS, ORCHESTRATOR_SESSION_ID};

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

pub(crate) struct AutonomousDelegateState {
    snapshot: Arc<RepoSnapshot>,
    model_router: Arc<dyn ConcurrentModelRouter>,
    tools: Arc<ToolEngine>,
    policy: Arc<ReviewerPolicy>,
    limits: Arc<RuntimeLimits>,
    review_revision_id: String,
    events: RuntimeEventDispatcher,
    pub(super) active_sessions: Arc<Semaphore>,
    child_sequence: AtomicUsize,
    max_child_sessions: usize,
    child_reports: Mutex<Vec<AgentLoopReport>>,
    child_candidates: Mutex<Vec<ChildCandidateDiscovery>>,
}

impl AutonomousDelegateState {
    pub(super) fn new(runtime: &AutonomousReviewRuntime) -> Self {
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
            child_candidates: Mutex::new(Vec::new()),
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

    pub(super) fn child_reports(&self) -> Vec<AgentLoopReport> {
        self.child_reports.lock().clone()
    }

    fn record_child_candidates(&self, candidates: Vec<ChildCandidateDiscovery>) {
        self.child_candidates.lock().extend(candidates);
    }

    pub(super) fn child_candidate_discoveries(&self) -> Vec<ChildCandidateDiscovery> {
        self.child_candidates.lock().clone()
    }

    pub(super) fn agent_loop_runtime(&self) -> AgentLoopRuntime {
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
pub(super) struct DelegateToolPacket {
    pub(super) session_id: String,
    pub(super) status: String,
    pub(super) summary: String,
    pub(super) artifact_id: Option<ArtifactId>,
    pub(super) candidate_count: usize,
    pub(super) completed: bool,
    pub(super) finalization_reason: String,
    pub(super) schema_validation_success: bool,
    pub(super) compact: Value,
    full: Value,
}

#[derive(Debug, Clone)]
pub(super) struct ChildCandidateDiscovery {
    pub(super) candidate: CandidateFinding,
    pub(super) child_session_id: String,
    pub(super) task_type: &'static str,
    pub(super) artifact_id: Option<ArtifactId>,
}

pub(super) async fn run_child_delegate(
    state: Arc<AutonomousDelegateState>,
    kind: DelegateTaskKind,
    request: DelegateTaskRequest,
    cancel: CancellationToken,
) -> RuntimeResult<DelegateToolPacket> {
    run_child_delegate_with_budget(state, kind, request, child_budget(kind), cancel).await
}

pub(super) async fn run_child_delegate_with_budget(
    state: Arc<AutonomousDelegateState>,
    kind: DelegateTaskKind,
    request: DelegateTaskRequest,
    budget: AgentBudget,
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
        budget,
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
    let child_output_valid =
        report.completed && report.diagnostic.final_output.schema_validation_success;
    let parsed = if child_output_valid {
        parse_child_packet(kind, report.output.as_deref())
    } else {
        invalid_child_packet(
            kind,
            format!(
                "child session status={} completed={} schemaValidationSuccess={}",
                report.status,
                report.completed,
                report.diagnostic.final_output.schema_validation_success
            ),
        )
    };
    let artifact_content = serde_json::to_string_pretty(&parsed)
        .unwrap_or_else(|_| report.output.clone().unwrap_or_default());
    let artifact_id = state.tools.insert_artifact(
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
    let candidate_count = parsed
        .get("candidateFindings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let completed = report.completed;
    let finalization_reason = report.diagnostic.finalization_reason.clone();
    let schema_validation_success = report.diagnostic.final_output.schema_validation_success;
    if kind != DelegateTaskKind::ValidateFinding {
        state.record_child_candidates(extract_child_candidates(
            kind,
            &child_id,
            &parsed,
            &artifact_id,
        ));
    }
    let packet = DelegateToolPacket {
        session_id: child_id.0.clone(),
        status,
        summary,
        artifact_id: Some(artifact_id.clone()),
        candidate_count,
        completed,
        finalization_reason,
        schema_validation_success,
        compact,
        full: parsed,
    };
    state.record_child_report(report);
    Ok(packet)
}

pub(super) fn child_budget(kind: DelegateTaskKind) -> AgentBudget {
    match kind {
        DelegateTaskKind::SearchCode => AgentBudget {
            max_turns: 6,
            max_tool_calls: 32,
            max_prompt_tokens: 64_000,
            max_output_tokens: 3_072,
            budget_source: BudgetSource::AdaptiveReview,
        },
        DelegateTaskKind::ExploreCode => AgentBudget {
            max_turns: 8,
            max_tool_calls: 64,
            max_prompt_tokens: 96_000,
            max_output_tokens: 4_096,
            budget_source: BudgetSource::AdaptiveReview,
        },
        DelegateTaskKind::ValidateFinding => AgentBudget {
            max_turns: 4,
            max_tool_calls: 8,
            max_prompt_tokens: 32_000,
            max_output_tokens: 1_536,
            budget_source: BudgetSource::AdaptiveReview,
        },
    }
}

pub(super) fn lead_generation_child_budget() -> AgentBudget {
    AgentBudget {
        max_turns: 4,
        max_tool_calls: 6,
        max_prompt_tokens: 32_000,
        max_output_tokens: 1_536,
        budget_source: BudgetSource::AdaptiveReview,
    }
}

fn extract_child_candidates(
    kind: DelegateTaskKind,
    child_id: &SessionId,
    packet: &Value,
    artifact_id: &ArtifactId,
) -> Vec<ChildCandidateDiscovery> {
    packet
        .get("candidateFindings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<CandidateFinding>(item.clone()).ok())
                .map(|candidate| ChildCandidateDiscovery {
                    candidate,
                    child_session_id: child_id.0.clone(),
                    task_type: kind.tool_name(),
                    artifact_id: Some(artifact_id.clone()),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}
