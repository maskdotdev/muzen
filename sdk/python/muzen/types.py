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


@dataclass(frozen=True)
class ReviewSource:
    type: Literal["local", "github_pull_request", "gitlab_merge_request"]
    repo: str
    owner: Optional[str] = None
    number: Optional[int] = None
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
    model_profile_id: Optional[str] = None
    budget: Optional[ReviewAgentBudget] = None


@dataclass(frozen=True)
class ReviewLimits:
    max_active_sessions: Optional[int] = None
    max_file_bytes: Optional[int] = None
    max_search_matches: Optional[int] = None


@dataclass(frozen=True)
class ReviewOptions:
    dedupe: Union[str, Dict[str, str]] = "none"
    cancel_superseded: bool = False
    model: Optional[str] = None
    scope_files: List[str] = field(default_factory=list)
    scope_include: List[str] = field(default_factory=list)
    scope_exclude: List[str] = field(default_factory=list)
    metadata: Dict[str, Any] = field(default_factory=dict)
    sessions: List[ReviewAgentSession] = field(default_factory=list)
    limits: Optional[ReviewLimits] = None


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
class ReviewSessionSnapshot:
    id: str
    status: ReviewStatus
    source: ReviewSource
    result: Optional[ReviewResult] = None
