from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Literal, Optional, Union

ReviewStatus = Literal[
    "created",
    "queued",
    "running",
    "completed",
    "failed",
    "cancelled",
]

SwarmAgentStatus = Literal["done", "failed", "cancelled", "partial"]
SwarmRunStatus = Literal["completed", "partial", "failed", "cancelled"]

ReviewConclusion = Literal["approved", "commented", "changes_requested"]

ReviewRole = Literal[
    "generalist",
    "security",
    "performance",
    "maintainability",
    "correctness",
    "architecture",
    "validator",
]

ReviewEventType = Literal[
    "session.created",
    "session.queued",
    "session.started",
    "source.resolved",
    "scope.inferred",
    "scope.overridden",
    "repo.materialized",
    "plan.created",
    "agent.started",
    "agent.completed",
    "tool.started",
    "tool.completed",
    "finding.created",
    "finding.updated",
    "review.result_created",
    "session.completed",
    "session.failed",
    "session.cancelled",
    "runner.event",
]

ReviewArtifactView = Literal["redacted", "raw"]
ModelProviderKind = Literal["openai", "anthropic", "openai_compatible"]
SourceProviderKind = Literal["github", "gitlab", "perforce", "custom"]
WebhookDeliveryType = Literal["review_created", "review_deduped", "ignored"]
ModelApiProtocol = Literal["responses", "chat_completions"]
ContextEngineMode = Literal["disabled", "snapshot_v0"]
ContextEvidenceKind = Literal[
    "diff",
    "file_span",
    "symbol",
    "test",
    "config",
    "doc",
    "repository_rule",
    "organization_rule",
    "ticket",
    "historical_pr",
    "prior_finding",
    "ci_failure",
    "dependency",
    "cross_repo_contract",
    "tool_output",
    "pack_summary",
]
ContextEvidenceSource = Literal["snapshot", "host", "history", "memory", "tool", "external"]
ContextTrust = Literal[
    "kernel",
    "host_trusted",
    "organization_trusted",
    "repository_untrusted",
    "user_untrusted",
    "external_untrusted",
    "tool_provider",
]
ContextSensitivity = Literal["public", "private", "secret_redacted", "restricted"]
ContextScope = Literal["run", "snapshot", "workspace", "repository", "organization", "external"]
ContextRelationshipKind = Literal[
    "imports",
    "calls",
    "implements",
    "tests",
    "configures",
    "documents",
    "depends_on",
    "same_symbol",
    "similar_history",
    "violates_rule",
    "satisfies_ticket",
    "contradicts",
    "cross_repo_contract",
]
ContextOmissionReason = Literal[
    "budget_exhausted",
    "duplicate",
    "low_relevance",
    "lower_trust",
    "generated_file",
    "binary_file",
    "secret_redacted",
    "outside_scope",
    "superseded_by_summary",
    "requires_ungranted_capability",
]
ContextPackPurpose = Literal[
    "general_review",
    "correctness",
    "security",
    "tests",
    "architecture",
    "performance",
    "validator",
    "standalone_query",
]
ContextSufficiencyStatus = Literal[
    "sufficient",
    "probably_sufficient",
    "insufficient",
]
ContextQueryKind = Literal[
    "search_text",
    "read_span",
    "explain_pack",
    "related_tests",
    "related_symbols",
    "ticket_requirements",
    "history_similar",
    "cross_repo_contracts",
    "sufficiency_check",
]
ContextLearningSource = Literal[
    "accepted_finding",
    "dismissed_finding",
    "human_feedback",
    "merged_pr",
    "manual_rule",
]
ContextLearningStatus = Literal[
    "proposed",
    "approved",
    "expired",
    "rejected",
]
ContextLearningScope = Literal[
    "repository",
    "workspace",
    "organization",
]


@dataclass(frozen=True)
class ContextEngineConfig:
    mode: ContextEngineMode
    max_indexed_files: int
    max_indexed_bytes: int
    max_evidence_items: int
    max_pack_tokens: int
    max_query_results: int
    include_repository_guidance: bool
    include_host_context: bool
    strict_evidence_required: bool


@dataclass(frozen=True)
class ContextRange:
    start_line: int
    end_line: int


@dataclass(frozen=True)
class ContextProvenance:
    provider: str
    query: Optional[str] = None
    tool_call_id: Optional[str] = None
    snapshot_id: Optional[str] = None
    original_url: Optional[str] = None


@dataclass(frozen=True)
class ContextEvidence:
    id: str
    kind: ContextEvidenceKind
    source: ContextEvidenceSource
    trust: ContextTrust
    sensitivity: ContextSensitivity
    scope: ContextScope
    token_estimate: int
    provenance: ContextProvenance
    path: Optional[str] = None
    revision: Optional[str] = None
    range: Optional[ContextRange] = None
    content_hash: Optional[str] = None
    summary: Optional[str] = None
    created_at_utc: Optional[str] = None
    expires_at_utc: Optional[str] = None


@dataclass(frozen=True)
class ContextRelationship:
    from_id: str
    to: str
    kind: ContextRelationshipKind
    confidence: float
    reason: str


@dataclass(frozen=True)
class OmittedContextCandidate:
    evidence_id: str
    kind: ContextEvidenceKind
    score: float
    token_estimate: int
    reason: ContextOmissionReason
    path: Optional[str] = None


@dataclass(frozen=True)
class ContextBudgetUsage:
    max_tokens: int
    used_tokens: int


@dataclass(frozen=True)
class ContextSufficiency:
    status: ContextSufficiencyStatus
    missing: List[str] = field(default_factory=list)


@dataclass(frozen=True)
class ContextLearning:
    id: str
    snapshot_id: str
    source: ContextLearningSource
    status: ContextLearningStatus
    scope: ContextLearningScope
    evidence_ids: List[str]
    summary: str
    created_at_utc: str
    expires_at_utc: Optional[str] = None


@dataclass(frozen=True)
class ContextFeedback:
    snapshot_id: str
    evidence_ids: List[str]
    feedback: str
    source: Optional[ContextLearningSource] = None
    scope: Optional[ContextLearningScope] = None


@dataclass(frozen=True)
class ContextFeedbackReceipt:
    accepted: bool
    message: str
    proposed_learning: Optional[ContextLearning] = None


@dataclass(frozen=True)
class ContextLearningApproval:
    learning_id: str
    approve: bool = False
    expires_at_utc: Optional[str] = None


@dataclass(frozen=True)
class ContextLearningApprovalReceipt:
    accepted: bool
    learning: ContextLearning


@dataclass(frozen=True)
class ContextPack:
    id: str
    snapshot_id: str
    purpose: ContextPackPurpose
    evidence: List[ContextEvidence]
    relationships: List[ContextRelationship]
    omitted_candidates: List[OmittedContextCandidate]
    budget: ContextBudgetUsage
    sufficiency: ContextSufficiency
    compiler_version: str
    created_at_utc: str
    run_id: Optional[str] = None
    session_id: Optional[str] = None


@dataclass(frozen=True)
class ContextQueryLimits:
    max_results: int
    max_tokens: int


@dataclass(frozen=True)
class ContextQuery:
    snapshot_id: str
    kind: ContextQueryKind
    arguments: Any
    limits: ContextQueryLimits
    run_id: Optional[str] = None
    session_id: Optional[str] = None
    purpose: Optional[ContextPackPurpose] = None
    current_evidence: List[str] = field(default_factory=list)


@dataclass(frozen=True)
class ContextQueryResult:
    kind: ContextQueryKind
    evidence: List[ContextEvidence]
    omitted: int
    sufficiency: Optional[ContextSufficiency] = None
    data: Optional[Any] = None


@dataclass(frozen=True)
class ContextFindingEvidence:
    finding_id: str
    primary_evidence: List[str]
    supporting_evidence: List[str]
    contradicted_by: List[str]
    sufficiency: ContextSufficiencyStatus
    artifact_id: Optional[str] = None


@dataclass(frozen=True)
class ContextFindingsEvidenceArtifact:
    schema_version: str
    run_id: str
    findings: List[ContextFindingEvidence]


@dataclass(frozen=True)
class ContextManifest:
    schema_version: str
    engine_version: str
    snapshot_id: str
    rule_count: int
    evidence_count: int
    relationship_count: int
    skipped_count: int
    created_at_utc: str


@dataclass(frozen=True)
class ReviewSource:
    type: Literal[
        "local",
        "raw_snapshot",
        "github_pull_request",
        "gitlab_merge_request",
        "perforce_changelist",
        "custom",
    ]
    repo: Optional[str] = None
    root: Optional[str] = None
    owner: Optional[str] = None
    number: Optional[int] = None
    server: Optional[str] = None
    changelist: Optional[str] = None
    client: Optional[str] = None
    depot_paths: List[str] = field(default_factory=list)
    provider: Optional[str] = None
    id: Optional[str] = None
    changed_files: List[str] = field(default_factory=list)


ReviewSourceLike = Union[ReviewSource, str]


@dataclass(frozen=True)
class ReviewAgentBudget:
    max_turns: int
    max_tool_calls: int
    max_prompt_tokens: int
    max_output_tokens: int


@dataclass(frozen=True)
class ReviewAgentSession:
    id: str
    role: ReviewRole
    objective: str
    cwd: Optional[str] = None
    model: Optional["ReviewModelSpec"] = None
    instructions: List["ReviewInstruction"] = field(default_factory=list)
    tool_grants: List[str] = field(default_factory=list)
    budget: Optional[ReviewAgentBudget] = None


@dataclass(frozen=True)
class ReviewModelCredential:
    env: Optional[str] = None
    secret_ref: Optional[str] = None


@dataclass(frozen=True)
class OpenAIReviewModelSpec:
    kind: Literal["provider"]
    provider: Literal["openai"]
    model: str
    credential: ReviewModelCredential = field(
        default_factory=lambda: ReviewModelCredential(env="OPENAI_API_KEY")
    )
    base_url: Optional[str] = None
    api_protocol: ModelApiProtocol = "responses"
    max_input_tokens: Optional[int] = None
    max_output_tokens: Optional[int] = None
    temperature: Optional[float] = None
    top_p: Optional[float] = None


@dataclass(frozen=True)
class AnthropicReviewModelSpec:
    kind: Literal["provider"]
    provider: Literal["anthropic"]
    model: str
    credential: ReviewModelCredential = field(
        default_factory=lambda: ReviewModelCredential(env="ANTHROPIC_API_KEY")
    )
    base_url: Optional[str] = None
    api_protocol: Literal["messages"] = "messages"
    max_input_tokens: Optional[int] = None
    max_output_tokens: Optional[int] = None
    temperature: Optional[float] = None
    top_p: Optional[float] = None


ReviewModelSpec = Union[OpenAIReviewModelSpec, AnthropicReviewModelSpec]


@dataclass(frozen=True)
class ReviewLimits:
    max_active_sessions: Optional[int] = None
    max_file_bytes: Optional[int] = None
    max_search_matches: Optional[int] = None


@dataclass(frozen=True)
class ModelProfileInput:
    provider: ModelProviderKind
    model: str
    secret_ref: Optional[str] = None
    base_url: Optional[str] = None
    routing: Dict[str, str] = field(default_factory=dict)


@dataclass(frozen=True)
class ProviderProfileInput:
    provider: SourceProviderKind
    secret_ref: Optional[str] = None
    base_url: Optional[str] = None
    routing: Dict[str, str] = field(default_factory=dict)


@dataclass(frozen=True)
class ModelProfile:
    workspace_id: str
    name: str
    version: str
    provider: ModelProviderKind
    model: str
    secret_ref: Optional[str] = None
    base_url: Optional[str] = None
    routing: Dict[str, str] = field(default_factory=dict)
    updated_at_utc: str = ""


@dataclass(frozen=True)
class ProviderProfile:
    workspace_id: str
    name: str
    version: str
    provider: SourceProviderKind
    secret_ref: Optional[str] = None
    base_url: Optional[str] = None
    routing: Dict[str, str] = field(default_factory=dict)
    updated_at_utc: str = ""


@dataclass(frozen=True)
class ReviewOptions:
    dedupe: Union[str, Dict[str, str]] = "none"
    cancel_superseded: bool = False
    model: Optional[ReviewModelSpec] = None
    change: Optional["ReviewChangeSpec"] = None
    scope_files: List[str] = field(default_factory=list)
    scope_include: List[str] = field(default_factory=list)
    scope_exclude: List[str] = field(default_factory=list)
    metadata: Dict[str, Any] = field(default_factory=dict)
    instructions: List["ReviewInstruction"] = field(default_factory=list)
    tools: List["ReviewTool"] = field(default_factory=list)
    sessions: List[ReviewAgentSession] = field(default_factory=list)
    limits: Optional[ReviewLimits] = None
    context_engine: Optional[ContextEngineConfig] = None


@dataclass(frozen=True)
class SwarmAgent:
    id: str
    objective: str
    instructions: List["ReviewInstruction"] = field(default_factory=list)
    model: Optional[ReviewModelSpec] = None
    tool_grants: List[str] = field(default_factory=list)
    budget: Optional[ReviewAgentBudget] = None


@dataclass(frozen=True)
class SwarmOptions:
    agents: List[SwarmAgent]
    repo: str
    files: List[str] = field(default_factory=list)
    model: Optional[ReviewModelSpec] = None
    tools: List["ReviewTool"] = field(default_factory=list)
    limits: Optional[ReviewLimits] = None
    metadata: Dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class ReviewChangedFile:
    path: str
    status: Optional[str] = None


@dataclass(frozen=True)
class ReviewChangeSpec:
    kind: str
    base_revision: Optional[str] = None
    start_revision: Optional[str] = None
    head_revision: Optional[str] = None
    changed_files: List[ReviewChangedFile] = field(default_factory=list)
    diff: Optional[str] = None
    review_target: Optional[str] = None
    metadata: Dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class ReviewInstruction:
    kind: str
    text: str
    trusted: bool = False


ReviewToolEffect = Literal[
    "read_repo",
    "read_diff",
    "read_artifact",
    "read_host",
    "read_network",
    "read_scratch",
    "write_artifact",
    "write_scratch",
]


@dataclass(frozen=True)
class ReviewTool:
    id: str
    description: str
    parameters: Any
    effects: List[ReviewToolEffect] = field(default_factory=list)
    cacheable: bool = False
    provider_resources: List[str] = field(default_factory=list)


@dataclass(frozen=True)
class ReviewFinding:
    id: str
    severity: Literal["info", "warning", "error"]
    category: Literal[
        "bug",
        "security",
        "performance",
        "maintainability",
        "style",
        "test",
        "docs",
        "other",
    ]
    title: str
    message: str
    location: Optional[Dict[str, Any]] = None
    suggested_fix: Optional[Dict[str, Any]] = None
    confidence: Optional[float] = None
    validation_status: Optional[str] = None
    evidence: List["ReviewFindingEvidence"] = field(default_factory=list)
    discovered_by: List[str] = field(default_factory=list)
    validated_by: List[str] = field(default_factory=list)
    challenged_by: List[str] = field(default_factory=list)


@dataclass(frozen=True)
class ReviewFindingEvidence:
    evidence_id: str
    artifact_id: str
    kind: str
    content_hash: str
    producing_tool_call_id: str


@dataclass(frozen=True)
class ReviewCoverage:
    files_considered: int
    files_reviewed: int
    files_skipped: int


@dataclass(frozen=True)
class ReviewResult:
    review_id: str
    session_id: str
    status: ReviewStatus
    conclusion: ReviewConclusion
    summary: str
    findings: List[ReviewFinding]
    coverage: ReviewCoverage
    metadata: Dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class SwarmAgentOutput:
    agent_id: str
    status: SwarmAgentStatus
    completed: bool
    output: Optional[str] = None


@dataclass(frozen=True)
class SwarmUsage:
    agents: int
    completed_agents: int
    model_calls: int
    tool_calls: int
    input_tokens: int
    output_tokens: int
    total_tokens: int


@dataclass(frozen=True)
class SwarmResult:
    run_id: str
    status: SwarmRunStatus
    outputs: List[SwarmAgentOutput]
    usage: SwarmUsage
    metadata: Dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class ReviewEvent:
    cursor: str
    type: ReviewEventType
    review_id: str
    timestamp_utc: str
    payload: Any = None


@dataclass(frozen=True)
class ReviewCancelOptions:
    reason: Optional[str] = None


@dataclass(frozen=True)
class ReviewArtifactReadOptions:
    view: ReviewArtifactView = "redacted"


@dataclass(frozen=True)
class ReviewArtifactExportOptions:
    view: ReviewArtifactView = "redacted"
    artifact_ids: List[str] = field(default_factory=list)
    max_artifacts: Optional[int] = None
    max_bytes: Optional[int] = None


@dataclass(frozen=True)
class ReviewArtifact:
    artifact_id: str
    bytes: int
    content_hash: str
    content: str


@dataclass(frozen=True)
class ReviewArtifactExport:
    view: ReviewArtifactView
    artifact_count: int
    total_bytes: int
    artifacts: List[ReviewArtifact]


@dataclass(frozen=True)
class ReviewSessionSnapshot:
    id: str
    status: ReviewStatus
    source: ReviewSource
    result: Optional[ReviewResult] = None


@dataclass(frozen=True)
class WebhookDelivery:
    type: WebhookDeliveryType
    delivery_id: Optional[str] = None
    review_id: Optional[str] = None
    status: Optional[ReviewStatus] = None
    reason: Optional[str] = None


@dataclass(frozen=True)
class WebhookHttpResponse:
    status_code: int
    headers: Dict[str, str]
    body: str
