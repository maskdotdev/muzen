from __future__ import annotations

import asyncio
import base64
import json
import os
import shutil
import tempfile
import types
from contextlib import AbstractAsyncContextManager
from dataclasses import MISSING, dataclass, fields, is_dataclass, replace
from pathlib import Path
from typing import (
    Any,
    Dict,
    List,
    Mapping,
    Optional,
    Sequence,
    Set,
    Tuple,
    Type,
    Union,
    get_args,
    get_origin,
    get_type_hints,
)
from urllib.parse import urlparse

from .agent import (
    AgentBudget,
    AgentDefinition,
    AgentInputLike,
    AgentOutput,
    ArtifactBlock,
    BuiltinToolProvider,
    ContentBlock,
    ImageBlock,
    ModelProfile,
    Muzen,
    MuzenError,
    OutputContract,
    PathWorkspaceBase,
    PutSecretInput,
    RunLimits,
    RunResult,
    SessionSpec,
    TextBlock,
    ToolGrant,
    Usage,
    WorkspaceSpec,
    connect_http,
    connect_local_runner,
)


_MODEL_ID = "default"
_BUILTIN_PROVIDER_ID = "builtin"
_DEFAULT_MAX_INPUT_TOKENS = 128_000
_DEFAULT_MAX_OUTPUT_TOKENS = 4_096


def discover_local_runner_binary() -> Optional[str]:
    """Return the configured, installed, or repo-local agent runner binary."""
    configured = os.environ.get("MUZEN_AGENT_RUNNER_BIN")
    if configured:
        return configured
    installed = shutil.which("muzen-agent-runner")
    if installed:
        return installed
    repo_binary = (
        Path(__file__).resolve().parents[3]
        / "target"
        / "debug"
        / "muzen-agent-runner"
    )
    if repo_binary.is_file():
        return str(repo_binary)
    return None


@dataclass(frozen=True)
class AgentResult:
    """The terminal output of one root agent run.

    Terminal failures are returned for inspection. Call ``raise_for_status``
    to turn failed, cancelled, or budget-exhausted outcomes into ``MuzenError``.
    """

    text: str
    output: Any
    usage: Usage
    status: str
    run_id: str
    raw: RunResult

    def raise_for_status(self) -> "AgentResult":
        if self.status == "completed":
            return self
        failed_output = next(
            (output for output in self.raw.outputs if output.status == self.status),
            None,
        )
        error = failed_output.error if failed_output is not None else None
        message = (
            error.message
            if error is not None
            else "agent ended with status %s" % self.status
        )
        code = {
            "budget_exhausted": "resource_exhausted",
            "cancelled": "conflict",
        }.get(self.status, "internal")
        details = {"status": self.status}
        if error is not None:
            details["executionCode"] = error.code
        retryable = error.retryable if error is not None else False
        raise MuzenError(code, message, retryable, details)


class Agent(AbstractAsyncContextManager):
    """Ergonomic authoring facade over Muzen's agent wire contracts."""

    def __init__(
        self,
        *,
        instructions: Optional[Union[str, ContentBlock, Sequence[ContentBlock]]] = None,
        model: Optional[str] = None,
        output: Optional[Any] = None,
        can_spawn: bool = False,
        can_message: bool = False,
        tools: Optional[Sequence[Any]] = None,
        spec: Optional[SessionSpec] = None,
        client: Optional[Muzen] = None,
        transport: str = "local_runner",
        api_key: Optional[str] = None,
        base_url: Optional[str] = None,
        temperature: Optional[float] = None,
        max_output_tokens: Optional[int] = None,
        max_total_tokens: Optional[int] = None,
        deadline_ms: Optional[int] = None,
        budget: Optional[AgentBudget] = None,
    ) -> None:
        if tools is not None:
            raise NotImplementedError("tools= is reserved; MCP support is coming")
        if transport not in ("local_runner", "http"):
            raise _invalid("transport", "must be 'local_runner' or 'http'")
        if transport == "http" and client is None and not base_url:
            raise _invalid("base_url", "is required for HTTP transport")

        self._client = client
        self._transport = transport
        self._service_base_url = base_url if transport == "http" else None
        self._connect_lock = asyncio.Lock()
        self._secret_ref: Optional[str] = None
        self._closed = False
        self._temp_dir: Optional[tempfile.TemporaryDirectory[str]] = None
        self._api_key: Optional[str] = None

        if spec is not None:
            conflicting = (
                instructions is not None
                or model is not None
                or output is not None
                or can_spawn
                or can_message
                or api_key is not None
                or temperature is not None
                or max_output_tokens is not None
                or budget is not None
            )
            if conflicting:
                raise _invalid("spec", "cannot be combined with facade authoring options")
            self._spec_template = spec
            self._needs_secret = False
            self._has_output = spec.agent.output is not None
        else:
            if instructions is None:
                raise _invalid("instructions", "is required")
            if model is None:
                raise _invalid("model", "is required")
            provider, protocol, model_name, environment = _model_settings(model)
            key = api_key or os.environ.get(environment)
            if not key:
                raise _invalid(
                    "api_key",
                    "is required when %s is not set" % environment,
                )
            if max_output_tokens is not None and max_output_tokens <= 0:
                raise _invalid("max_output_tokens", "must be positive")

            self._api_key = key
            self._needs_secret = True
            self._has_output = output is not None
            self._temp_dir = tempfile.TemporaryDirectory(prefix="muzen-agent-")
            grants = []
            if can_spawn:
                grants.append(
                    ToolGrant(
                        provider=_BUILTIN_PROVIDER_ID,
                        tool="agent.spawn",
                        effects=("agent_spawn",),
                    )
                )
            if can_message:
                grants.append(
                    ToolGrant(
                        provider=_BUILTIN_PROVIDER_ID,
                        tool="agent.message",
                        effects=("agent_message",),
                    )
                )
            providers = (
                (BuiltinToolProvider(_BUILTIN_PROVIDER_ID),)
                if can_spawn or can_message
                else ()
            )
            contract = None if output is None else OutputContract(_output_schema(output))
            definition = AgentDefinition(
                name="agent",
                instructions=_coerce_instructions(instructions),
                model=_MODEL_ID,
                tools=tuple(grants),
                budget=budget,
                output=contract,
            )
            profile = ModelProfile(
                id=_MODEL_ID,
                provider=provider,
                protocol=protocol,
                model=model_name,
                credential="pending",
                max_input_tokens=_DEFAULT_MAX_INPUT_TOKENS,
                max_output_tokens=max_output_tokens or _DEFAULT_MAX_OUTPUT_TOKENS,
                base_url=base_url if transport == "local_runner" else None,
                temperature=temperature,
            )
            self._spec_template = SessionSpec(
                agent=definition,
                models=(profile,),
                tool_providers=providers,
                workspace=WorkspaceSpec(PathWorkspaceBase(self._temp_dir.name)),
            )

        self._default_limits = RunLimits(
            max_active_agents=4,
            max_agents=16,
            max_depth=3,
            max_input_bytes=1_048_576,
            max_total_tokens=max_total_tokens,
            deadline_ms=deadline_ms,
        )

    async def __aenter__(self) -> "Agent":
        await self._connection()
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        await self.close()

    async def run(
        self, prompt: AgentInputLike, *, limits: Optional[RunLimits] = None
    ) -> AgentResult:
        client, spec = await self._ready()
        session = await client.create_session(spec)
        try:
            run = await session.run(prompt, limits=limits or self._default_limits)
            return self._result(await run.wait(), session.id)
        finally:
            await _archive_best_effort(session)

    def session(self) -> "AgentConversation":
        return AgentConversation(self)

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            if self._client is not None:
                await self._client.close()
        finally:
            if self._temp_dir is not None:
                self._temp_dir.cleanup()

    async def _connection(self) -> Muzen:
        if self._closed:
            raise MuzenError("unavailable", "Agent is closed", False)
        if self._client is not None:
            return self._client
        async with self._connect_lock:
            if self._client is None:
                if self._transport == "http":
                    self._client = await connect_http(self._service_base_url or "")
                else:
                    self._client = await connect_local_runner(
                        binary_path=discover_local_runner_binary(),
                        allow_loopback_http=_is_loopback_url(
                            self._spec_template.models[0].base_url
                            if self._spec_template.models
                            else None
                        ),
                    )
        return self._client

    async def _ready(self) -> Tuple[Muzen, SessionSpec]:
        client = await self._connection()
        if not self._needs_secret:
            return client, self._spec_template
        if self._secret_ref is None:
            async with self._connect_lock:
                if self._secret_ref is None:
                    encoded = base64.b64encode(
                        (self._api_key or "").encode("utf-8")
                    ).decode("ascii")
                    self._secret_ref = await client.put_secret(PutSecretInput(encoded))
        profiles = tuple(
            replace(profile, credential=self._secret_ref)
            if profile.id == _MODEL_ID
            else profile
            for profile in self._spec_template.models
        )
        return client, replace(self._spec_template, models=profiles)

    def _result(self, raw: RunResult, session_id: str) -> AgentResult:
        root = _root_output(raw, session_id)
        value = root.output
        text = value if isinstance(value, str) else json.dumps(value)
        exposed = value if self._has_output else text
        return AgentResult(
            text=text,
            output=exposed,
            usage=root.usage,
            status=root.status,
            run_id=raw.run_id,
            raw=raw,
        )


class AgentConversation(AbstractAsyncContextManager):
    def __init__(self, agent: Agent) -> None:
        self._agent = agent
        self._session: Any = None

    async def __aenter__(self) -> "AgentConversation":
        client, spec = await self._agent._ready()
        self._session = await client.create_session(spec)
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        if self._session is not None:
            await _archive_best_effort(self._session)

    async def run(
        self, prompt: AgentInputLike, *, limits: Optional[RunLimits] = None
    ) -> AgentResult:
        if self._session is None:
            raise MuzenError(
                "conflict",
                "Agent.session() must be entered with 'async with' before run()",
                False,
            )
        run = await self._session.run(
            prompt, limits=limits or self._agent._default_limits
        )
        return self._agent._result(await run.wait(), self._session.id)


def _model_settings(model: str) -> Tuple[str, str, str, str]:
    if not isinstance(model, str) or not model.strip():
        raise _invalid("model", "must not be empty")
    name = model.strip()
    override = None
    if ":" in name:
        prefix, candidate = name.split(":", 1)
        if prefix in ("anthropic", "openai"):
            override, name = prefix, candidate
            if not name:
                raise _invalid("model", "must include a name after the provider prefix")
    anthropic = override == "anthropic" or (
        override is None and name.lower().startswith("claude")
    )
    if anthropic:
        return "anthropic", "messages", name, "ANTHROPIC_API_KEY"
    return "openai_compatible", "chat_completions", name, "OPENAI_API_KEY"


def _coerce_instructions(
    instructions: Union[str, ContentBlock, Sequence[ContentBlock]],
) -> Tuple[ContentBlock, ...]:
    if isinstance(instructions, str):
        blocks: Tuple[ContentBlock, ...] = (TextBlock(instructions),)
    elif isinstance(instructions, (TextBlock, ArtifactBlock, ImageBlock)):
        blocks = (instructions,)
    else:
        try:
            blocks = tuple(instructions)
        except TypeError:
            raise _invalid("instructions", "must be text or content blocks")
    if not blocks:
        raise _invalid("instructions", "must contain at least one content block")
    if not all(isinstance(item, (TextBlock, ArtifactBlock, ImageBlock)) for item in blocks):
        raise _invalid("instructions", "must contain only content blocks")
    if any(isinstance(item, TextBlock) and not item.text.strip() for item in blocks):
        raise _invalid("instructions", "text blocks must not be empty")
    return blocks


def _output_schema(output: Any) -> Dict[str, Any]:
    if isinstance(output, dict):
        return output
    if not isinstance(output, type) or not (
        _is_typed_dict(output) or is_dataclass(output)
    ):
        raise _invalid("output", "must be a TypedDict, dataclass, or JSON Schema dict")
    return _object_schema(output, set())


def _object_schema(value: Type[Any], active: Set[Type[Any]]) -> Dict[str, Any]:
    if value in active:
        raise _invalid("output", "recursive output annotations are unsupported")
    active.add(value)
    try:
        hints = get_type_hints(value)
    except (NameError, TypeError) as exc:
        raise _invalid("output", "contains an unresolved annotation: %s" % exc)
    properties = {
        name: _annotation_schema(annotation, active)
        for name, annotation in hints.items()
    }
    if _is_typed_dict(value):
        required_keys = getattr(value, "__required_keys__", None)
        required = (
            list(hints)
            if required_keys is None
            else [name for name in hints if name in required_keys]
        )
    else:
        field_by_name = {item.name: item for item in fields(value)}
        required = [
            name
            for name in hints
            if field_by_name[name].default is MISSING
            and field_by_name[name].default_factory is MISSING
        ]
    schema: Dict[str, Any] = {
        "type": "object",
        "properties": properties,
        "additionalProperties": False,
    }
    if required:
        schema["required"] = required
    active.remove(value)
    return schema


def _annotation_schema(annotation: Any, active: Set[Type[Any]]) -> Dict[str, Any]:
    primitive = {str: "string", int: "integer", float: "number", bool: "boolean"}
    if annotation in primitive:
        return {"type": primitive[annotation]}
    if isinstance(annotation, type) and (
        _is_typed_dict(annotation) or is_dataclass(annotation)
    ):
        return _object_schema(annotation, active)
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in (list, List):
        if len(args) != 1:
            raise _invalid("output", "list annotations must have one item type")
        return {"type": "array", "items": _annotation_schema(args[0], active)}
    union_origins = [Union]
    union_type = getattr(types, "UnionType", None)
    if union_type is not None:
        union_origins.append(union_type)
    if origin in tuple(union_origins) and len(args) == 2 and type(None) in args:
        item = args[0] if args[1] is type(None) else args[1]
        return {"anyOf": [_annotation_schema(item, active), {"type": "null"}]}
    raise _invalid("output", "contains unsupported annotation %r" % (annotation,))


def _is_typed_dict(value: Any) -> bool:
    return (
        isinstance(value, type)
        and issubclass(value, dict)
        and hasattr(value, "__annotations__")
        and hasattr(value, "__total__")
    )


def _root_output(raw: RunResult, session_id: str) -> AgentOutput:
    for output in raw.outputs:
        if output.session_id == session_id and not output.path:
            return output
    for output in raw.outputs:
        if output.session_id == session_id:
            return output
    if raw.outputs:
        return raw.outputs[0]
    raise MuzenError("internal", "run completed without an agent output", False)


async def _archive_best_effort(session: Any) -> None:
    try:
        await session.archive()
    except Exception:
        pass


def _is_loopback_url(value: Optional[str]) -> bool:
    if not value:
        return False
    return (urlparse(value).hostname or "").lower() in (
        "localhost",
        "127.0.0.1",
        "::1",
    )


def _invalid(path: str, message: str) -> MuzenError:
    return MuzenError("invalid_input", "%s %s" % (path, message), False, {"path": path})
