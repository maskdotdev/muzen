from __future__ import annotations

from dataclasses import dataclass, field, fields, is_dataclass
from typing import (
    Any,
    AsyncIterator,
    Dict,
    Generic,
    Literal,
    Mapping,
    Optional,
    Protocol,
    Sequence,
    Tuple,
    TypeVar,
    Union,
)

SessionId = str
RunId = str
ArtifactId = str
SecretRef = str
ModelProfileId = str
ToolProviderId = str
AgentName = str
IdempotencyKey = str
JsonObject = Dict[str, Any]


@dataclass(frozen=True)
class TextBlock:
    text: str
    type: Literal["text"] = field(default="text", init=False)


@dataclass(frozen=True)
class ArtifactBlock:
    artifact_id: ArtifactId = field(metadata={"wire": "artifactId"})
    type: Literal["artifact"] = field(default="artifact", init=False)


@dataclass(frozen=True)
class ImageBlock:
    media_type: str = field(metadata={"wire": "mediaType"})
    data: str
    type: Literal["image"] = field(default="image", init=False)


ContentBlock = Union[TextBlock, ArtifactBlock, ImageBlock]


@dataclass(frozen=True)
class AgentInput:
    content: Tuple[ContentBlock, ...]


AgentInputLike = Union[AgentInput, str]


@dataclass(frozen=True)
class AgentBudget:
    max_turns: int = field(metadata={"wire": "maxTurns"})
    max_tool_calls: int = field(metadata={"wire": "maxToolCalls"})
    max_prompt_tokens: int = field(metadata={"wire": "maxPromptTokens"})
    max_output_tokens: int = field(metadata={"wire": "maxOutputTokens"})


@dataclass(frozen=True)
class OutputContract:
    schema: Union[bool, JsonObject]
    name: Optional[str] = None


ToolEffect = Literal[
    "workspace_read",
    "workspace_write",
    "network_read",
    "network_write",
    "artifact_write",
    "agent_spawn",
    "agent_message",
]


@dataclass(frozen=True)
class ToolGrant:
    provider: ToolProviderId
    tool: str
    effects: Tuple[ToolEffect, ...]
    description: Optional[str] = None
    input_schema: Optional[JsonObject] = field(
        default=None, metadata={"wire": "inputSchema"}
    )
    max_calls: Optional[int] = field(default=None, metadata={"wire": "maxCalls"})


@dataclass(frozen=True)
class AgentDefinition:
    name: AgentName
    instructions: Tuple[ContentBlock, ...]
    model: ModelProfileId
    tools: Tuple[ToolGrant, ...]
    budget: Optional[AgentBudget] = None
    output: Optional[OutputContract] = None
    metadata: Optional[JsonObject] = None


@dataclass(frozen=True)
class ModelProfile:
    id: ModelProfileId
    provider: Literal["openai_compatible", "anthropic"]
    protocol: Literal["responses", "chat_completions", "messages"]
    model: str
    credential: SecretRef
    max_input_tokens: int = field(metadata={"wire": "maxInputTokens"})
    max_output_tokens: int = field(metadata={"wire": "maxOutputTokens"})
    base_url: Optional[str] = field(default=None, metadata={"wire": "baseUrl"})
    temperature: Optional[float] = None
    top_p: Optional[float] = field(default=None, metadata={"wire": "topP"})


@dataclass(frozen=True)
class SecretHeader:
    secret: SecretRef


HeaderValue = Union[str, SecretHeader]


@dataclass(frozen=True)
class BuiltinToolProvider:
    id: ToolProviderId
    kind: Literal["builtin"] = field(default="builtin", init=False)


@dataclass(frozen=True)
class ClientToolProvider:
    id: ToolProviderId
    timeout_ms: Optional[int] = field(default=None, metadata={"wire": "timeoutMs"})
    kind: Literal["client"] = field(default="client", init=False)


@dataclass(frozen=True)
class McpToolProvider:
    id: ToolProviderId
    url: str
    credential: Optional[SecretRef] = None
    headers: Optional[Mapping[str, HeaderValue]] = None
    kind: Literal["mcp_http"] = field(default="mcp_http", init=False)


ToolProvider = Union[BuiltinToolProvider, ClientToolProvider, McpToolProvider]


@dataclass(frozen=True)
class PathWorkspaceBase:
    root: str
    kind: Literal["path"] = field(default="path", init=False)


@dataclass(frozen=True)
class GitWorkspaceBase:
    url: str
    revision: str
    credential: Optional[SecretRef] = None
    kind: Literal["git"] = field(default="git", init=False)


@dataclass(frozen=True)
class SnapshotWorkspaceBase:
    id: str
    kind: Literal["snapshot"] = field(default="snapshot", init=False)


WorkspaceBase = Union[PathWorkspaceBase, GitWorkspaceBase, SnapshotWorkspaceBase]


@dataclass(frozen=True)
class WorkspaceLimits:
    max_files: Optional[int] = field(default=None, metadata={"wire": "maxFiles"})
    max_file_bytes: Optional[int] = field(default=None, metadata={"wire": "maxFileBytes"})
    max_base_bytes: Optional[int] = field(default=None, metadata={"wire": "maxBaseBytes"})
    max_overlay_bytes: Optional[int] = field(default=None, metadata={"wire": "maxOverlayBytes"})


@dataclass(frozen=True)
class WorkspaceSpec:
    base: WorkspaceBase
    limits: Optional[WorkspaceLimits] = None


@dataclass(frozen=True)
class SessionBudget:
    max_runs: Optional[int] = field(default=None, metadata={"wire": "maxRuns"})
    max_lifetime_tokens: Optional[int] = field(
        default=None, metadata={"wire": "maxLifetimeTokens"}
    )
    max_lifetime_tool_calls: Optional[int] = field(
        default=None, metadata={"wire": "maxLifetimeToolCalls"}
    )


@dataclass(frozen=True)
class SessionSpec:
    agent: AgentDefinition
    models: Tuple[ModelProfile, ...]
    tool_providers: Tuple[ToolProvider, ...] = field(metadata={"wire": "toolProviders"})
    workspace: WorkspaceSpec
    session_budget: Optional[SessionBudget] = field(
        default=None, metadata={"wire": "sessionBudget"}
    )
    metadata: Optional[JsonObject] = None


@dataclass(frozen=True)
class ExistingSessionRoot:
    session_id: SessionId = field(metadata={"wire": "sessionId"})
    input: AgentInput


@dataclass(frozen=True)
class NewSessionRoot:
    session: SessionSpec
    input: AgentInput
    idempotency_key: Optional[IdempotencyKey] = field(
        default=None, metadata={"wire": "idempotencyKey"}
    )


RunRoot = Union[ExistingSessionRoot, NewSessionRoot]


@dataclass(frozen=True)
class RunLimits:
    max_active_agents: int = field(metadata={"wire": "maxActiveAgents"})
    max_agents: int = field(metadata={"wire": "maxAgents"})
    max_depth: int = field(metadata={"wire": "maxDepth"})
    max_input_bytes: int = field(metadata={"wire": "maxInputBytes"})
    max_total_tokens: Optional[int] = field(default=None, metadata={"wire": "maxTotalTokens"})
    max_total_tool_calls: Optional[int] = field(
        default=None, metadata={"wire": "maxTotalToolCalls"}
    )
    deadline_ms: Optional[int] = field(default=None, metadata={"wire": "deadlineMs"})


@dataclass(frozen=True)
class RunSpec:
    roots: Tuple[RunRoot, ...]
    limits: RunLimits
    idempotency_key: Optional[IdempotencyKey] = field(
        default=None, metadata={"wire": "idempotencyKey"}
    )
    metadata: Optional[JsonObject] = None


@dataclass(frozen=True)
class PutSecretInput:
    value: str = field(repr=False)
    idempotency_key: Optional[IdempotencyKey] = None


@dataclass(frozen=True)
class Usage:
    input_tokens: int
    output_tokens: int
    tool_calls: int


SessionStatus = Literal["open", "archived"]
AgentStatus = Literal[
    "queued",
    "running",
    "waiting",
    "completed",
    "failed",
    "cancelled",
    "budget_exhausted",
]
TerminalAgentStatus = Literal["completed", "failed", "cancelled", "budget_exhausted"]
RunStatus = Literal[
    "queued", "running", "waiting", "completed", "partial", "failed", "cancelled"
]
TerminalRunStatus = Literal["completed", "partial", "failed", "cancelled"]


@dataclass(frozen=True)
class SessionSnapshot:
    id: SessionId
    status: SessionStatus
    created_at: str
    updated_at: str
    metadata: JsonObject
    active_run_id: Optional[RunId] = None


@dataclass(frozen=True)
class AgentSnapshot:
    session_id: SessionId
    path: Tuple[int, ...]
    status: AgentStatus
    model: ModelProfileId
    usage: Usage
    parent_session_id: Optional[SessionId] = None


@dataclass(frozen=True)
class RunSnapshot:
    id: RunId
    status: RunStatus
    roots: Tuple[SessionId, ...]
    agents: Tuple[AgentSnapshot, ...]
    last_sequence: int
    created_at: str
    updated_at: str


ExecutionErrorCode = Literal[
    "model_error",
    "tool_error",
    "secret_unavailable",
    "workspace_error",
    "budget_exhausted",
    "cancelled",
]


@dataclass(frozen=True)
class ExecutionError:
    code: ExecutionErrorCode
    message: str
    retryable: bool
    details: Optional[JsonObject] = None


@dataclass(frozen=True)
class AgentOutput:
    session_id: SessionId
    path: Tuple[int, ...]
    status: TerminalAgentStatus
    usage: Usage
    output: Any = None
    error: Optional[ExecutionError] = None


@dataclass(frozen=True)
class ArtifactRef:
    id: ArtifactId
    media_type: str
    bytes: int


@dataclass(frozen=True)
class RunResult:
    run_id: RunId
    status: TerminalRunStatus
    outputs: Tuple[AgentOutput, ...]
    usage: Usage
    artifacts: Tuple[ArtifactRef, ...]
    metadata: JsonObject


@dataclass(frozen=True)
class AgentMessage:
    id: str
    session_id: SessionId
    role: Literal["system", "user", "assistant", "tool"]
    content: Tuple[ContentBlock, ...]
    created_at: str


@dataclass(frozen=True)
class AgentEvent:
    run_id: RunId
    sequence: int
    type: str
    timestamp: str
    payload: JsonObject
    session_id: Optional[SessionId] = None


@dataclass(frozen=True)
class AnswerToolCallInput:
    call_id: str = field(metadata={"wire": "callId"})
    outcome: JsonObject


@dataclass(frozen=True)
class CommandReceipt:
    sequence: int


@dataclass(frozen=True)
class SendCommand:
    session_id: SessionId
    input: AgentInput
    delivery: Literal["steer", "follow_up"]
    idempotency_key: Optional[IdempotencyKey] = None


@dataclass(frozen=True)
class SpawnCommand:
    parent_session_id: SessionId
    agent: AgentDefinition
    input: AgentInput
    idempotency_key: Optional[IdempotencyKey] = None


@dataclass(frozen=True)
class Capabilities:
    protocol_version: str
    workspace_bases: Tuple[Literal["path", "git", "snapshot"], ...]
    tool_provider_kinds: Tuple[Literal["builtin", "client", "mcp_http"], ...]
    model_protocols: Tuple[Literal["responses", "chat_completions", "messages"], ...]
    max_replay_batch: int


ErrorCode = Literal[
    "invalid_input",
    "not_found",
    "conflict",
    "unauthenticated",
    "permission_denied",
    "resource_exhausted",
    "unsupported",
    "unavailable",
    "deadline_exceeded",
    "internal",
]


class MuzenError(Exception):
    def __init__(
        self,
        code: ErrorCode,
        message: str,
        retryable: bool,
        details: Optional[JsonObject] = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.retryable = retryable
        self.details = details


T = TypeVar("T")


@dataclass(frozen=True)
class Page(Generic[T]):
    items: Tuple[T, ...]
    next: Optional[str] = None


class Artifact(Protocol):
    ref: ArtifactRef

    def data(self) -> AsyncIterator[bytes]: ...


class Run(Protocol):
    id: RunId

    async def snapshot(self) -> RunSnapshot: ...
    def events(self, *, after: Optional[int] = None) -> AsyncIterator[AgentEvent]: ...
    async def wait(self) -> RunResult: ...
    async def result(self) -> Optional[RunResult]: ...
    async def send(self, command: SendCommand) -> CommandReceipt: ...
    async def spawn(self, command: SpawnCommand) -> AgentSession: ...
    async def cancel(
        self,
        *,
        reason: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> CommandReceipt: ...
    async def artifact(self, artifact_id: ArtifactId) -> Artifact: ...


class AgentSession(Protocol):
    id: SessionId

    async def snapshot(self) -> SessionSnapshot: ...
    async def messages(
        self, *, after: Optional[str] = None, limit: Optional[int] = None
    ) -> Page[AgentMessage]: ...
    async def run(
        self,
        input: AgentInputLike,
        *,
        limits: RunLimits,
        idempotency_key: Optional[str] = None,
    ) -> Run: ...
    async def archive(self, *, idempotency_key: Optional[str] = None) -> None: ...


class Muzen(Protocol):
    async def capabilities(self) -> Capabilities: ...
    async def put_secret(self, input: PutSecretInput) -> SecretRef: ...
    async def delete_secret(self, secret: SecretRef) -> None: ...
    async def create_session(
        self, spec: SessionSpec, *, idempotency_key: Optional[str] = None
    ) -> AgentSession: ...
    async def get_session(self, session_id: SessionId) -> AgentSession: ...
    async def start_run(self, spec: RunSpec) -> Run: ...
    async def get_run(self, run_id: RunId) -> Run: ...
    async def answer_tool_call(
        self, run_id: RunId, input: AnswerToolCallInput
    ) -> None: ...
    async def close(self) -> None: ...


def define_agent(definition: AgentDefinition) -> AgentDefinition:
    _non_empty(definition.name, "name")
    _non_empty(definition.model, "model")
    if not definition.instructions:
        raise _invalid("instructions", "must contain at least one content block")
    for index, block in enumerate(definition.instructions):
        _validate_block(block, "instructions[%d]" % index)
    for index, grant in enumerate(definition.tools):
        _non_empty(grant.provider, "tools[%d].provider" % index)
        _non_empty(grant.tool, "tools[%d].tool" % index)
        if grant.description is not None:
            _non_empty(grant.description, "tools[%d].description" % index)
        if grant.input_schema is not None and not isinstance(grant.input_schema, dict):
            raise _invalid(
                "tools[%d].inputSchema" % index, "must be a JSON Schema object"
            )
        if grant.max_calls is not None:
            _positive_integer(grant.max_calls, "tools[%d].max_calls" % index)
    if definition.budget is not None:
        _positive_integer(definition.budget.max_turns, "budget.max_turns")
        _non_negative_integer(definition.budget.max_tool_calls, "budget.max_tool_calls")
        _positive_integer(definition.budget.max_prompt_tokens, "budget.max_prompt_tokens")
        _positive_integer(definition.budget.max_output_tokens, "budget.max_output_tokens")
    if definition.output is not None and not isinstance(definition.output.schema, (bool, dict)):
        raise _invalid("output.schema", "must be a JSON Schema boolean or object")
    return definition


def normalize_agent_input(input: AgentInputLike) -> AgentInput:
    if isinstance(input, str):
        _non_empty(input, "content[0].text")
        return AgentInput(content=(TextBlock(input),))
    if not input.content:
        raise _invalid("content", "must contain at least one block")
    for index, block in enumerate(input.content):
        _validate_block(block, "content[%d]" % index)
    return input


def to_wire(value: Any) -> Any:
    if is_dataclass(value):
        result = {}
        for item in fields(value):
            field_value = getattr(value, item.name)
            if field_value is None:
                continue
            wire_name = item.metadata.get("wire", _snake_to_camel(item.name))
            result[wire_name] = to_wire(field_value)
        return result
    if isinstance(value, Mapping):
        return {key: to_wire(item) for key, item in value.items()}
    if isinstance(value, (tuple, list)):
        return [to_wire(item) for item in value]
    return value


def _agent_definition_from_wire(value: Mapping[str, Any]) -> AgentDefinition:
    budget = value.get("budget")
    output = value.get("output")
    return AgentDefinition(
        name=value["name"],
        instructions=tuple(_block_from_wire(item) for item in value["instructions"]),
        model=value["model"],
        tools=tuple(
            ToolGrant(
                provider=item["provider"],
                tool=item["tool"],
                effects=tuple(item["effects"]),
                description=item.get("description"),
                input_schema=item.get("inputSchema"),
                max_calls=item.get("maxCalls"),
            )
            for item in value["tools"]
        ),
        budget=None
        if budget is None
        else AgentBudget(
            max_turns=budget["maxTurns"],
            max_tool_calls=budget["maxToolCalls"],
            max_prompt_tokens=budget["maxPromptTokens"],
            max_output_tokens=budget["maxOutputTokens"],
        ),
        output=None
        if output is None
        else OutputContract(schema=output["schema"], name=output.get("name")),
        metadata=value.get("metadata"),
    )


def _block_from_wire(value: Mapping[str, Any]) -> ContentBlock:
    if value["type"] == "text":
        return TextBlock(value["text"])
    if value["type"] == "artifact":
        return ArtifactBlock(value["artifactId"])
    if value["type"] == "image":
        return ImageBlock(value["mediaType"], value["data"])
    raise _invalid("content.type", "is unknown")


def _validate_block(block: ContentBlock, path: str) -> None:
    if isinstance(block, TextBlock):
        _non_empty(block.text, path + ".text")
    elif isinstance(block, ArtifactBlock):
        _non_empty(block.artifact_id, path + ".artifactId")
    elif isinstance(block, ImageBlock):
        _non_empty(block.media_type, path + ".mediaType")
        _non_empty(block.data, path + ".data")
    else:
        raise _invalid(path, "has an unknown content block")


def _snake_to_camel(value: str) -> str:
    head, *tail = value.split("_")
    return head + "".join(part.capitalize() for part in tail)


def _non_empty(value: str, path: str) -> None:
    if not value.strip():
        raise _invalid(path, "must not be empty")


def _positive_integer(value: int, path: str) -> None:
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise _invalid(path, "must be a positive integer")


def _non_negative_integer(value: int, path: str) -> None:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise _invalid(path, "must be a non-negative integer")


def _invalid(path: str, message: str) -> MuzenError:
    return MuzenError("invalid_input", "%s %s" % (path, message), False, {"path": path})


async def connect(options: Optional[Mapping[str, Any]] = None) -> Muzen:
    """Connect to the local runner by default, or to the configured transport."""
    from .agent_transports import connect as _connect

    return await _connect(options)


async def connect_local_runner(
    *,
    store: Literal["memory", "sqlite"] = "memory",
    sqlite_path: Optional[str] = None,
    allow_loopback_http: bool = False,
    binary_path: Optional[str] = None,
    close_timeout: float = 5.0,
) -> Muzen:
    """Spawn and connect to ``muzen-agent-runner`` over stdio JSON-RPC."""
    from .agent_transports import connect_local_runner as _connect_local_runner

    return await _connect_local_runner(
        store=store,
        sqlite_path=sqlite_path,
        allow_loopback_http=allow_loopback_http,
        binary_path=binary_path,
        close_timeout=close_timeout,
    )


async def connect_http(
    base_url: str,
    bearer_token: Optional[str] = None,
) -> Muzen:
    """Connect to a Muzen agent service over HTTP and SSE."""
    from .agent_transports import connect_http as _connect_http

    return await _connect_http(base_url, bearer_token=bearer_token)


# Imported last so the ergonomic facade can build on the wire contracts above
# while remaining available from ``muzen.agent``.
from .agent_facade import (
    Agent,
    AgentConversation,
    AgentResult,
    discover_local_runner_binary,
)
from .tools import Tool, tool
