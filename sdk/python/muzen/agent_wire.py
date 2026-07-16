from __future__ import annotations

from typing import Any, Mapping, Optional

from .agent import (
    AgentEvent,
    AgentMessage,
    AgentOutput,
    AgentSnapshot,
    ArtifactRef,
    Capabilities,
    ExecutionError,
    RunResult,
    RunSnapshot,
    SessionSnapshot,
    Usage,
    _block_from_wire,
)


def usage_from_wire(value: Mapping[str, Any]) -> Usage:
    return Usage(value["inputTokens"], value["outputTokens"], value["toolCalls"])


def session_snapshot_from_wire(value: Mapping[str, Any]) -> SessionSnapshot:
    return SessionSnapshot(
        id=value["id"],
        status=value["status"],
        created_at=value["createdAt"],
        updated_at=value["updatedAt"],
        metadata=value["metadata"],
        active_run_id=value.get("activeRunId"),
    )


def agent_snapshot_from_wire(value: Mapping[str, Any]) -> AgentSnapshot:
    return AgentSnapshot(
        session_id=value["sessionId"],
        path=tuple(value["path"]),
        status=value["status"],
        model=value["model"],
        usage=usage_from_wire(value["usage"]),
        parent_session_id=value.get("parentSessionId"),
    )


def run_snapshot_from_wire(value: Mapping[str, Any]) -> RunSnapshot:
    return RunSnapshot(
        id=value["id"],
        status=value["status"],
        roots=tuple(value["roots"]),
        agents=tuple(agent_snapshot_from_wire(item) for item in value["agents"]),
        last_sequence=value["lastSequence"],
        created_at=value["createdAt"],
        updated_at=value["updatedAt"],
    )


def execution_error_from_wire(
    value: Optional[Mapping[str, Any]],
) -> Optional[ExecutionError]:
    if value is None:
        return None
    return ExecutionError(
        code=value["code"],
        message=value["message"],
        retryable=value["retryable"],
        details=value.get("details"),
    )


def artifact_ref_from_wire(value: Mapping[str, Any]) -> ArtifactRef:
    return ArtifactRef(value["id"], value["mediaType"], value["bytes"])


def run_result_from_wire(value: Mapping[str, Any]) -> RunResult:
    outputs = tuple(
        AgentOutput(
            session_id=item["sessionId"],
            path=tuple(item["path"]),
            status=item["status"],
            usage=usage_from_wire(item["usage"]),
            output=item.get("output"),
            error=execution_error_from_wire(item.get("error")),
        )
        for item in value["outputs"]
    )
    return RunResult(
        run_id=value["runId"],
        status=value["status"],
        outputs=outputs,
        usage=usage_from_wire(value["usage"]),
        artifacts=tuple(artifact_ref_from_wire(item) for item in value["artifacts"]),
        metadata=value["metadata"],
    )


def message_from_wire(value: Mapping[str, Any]) -> AgentMessage:
    return AgentMessage(
        id=value["id"],
        session_id=value["sessionId"],
        role=value["role"],
        content=tuple(_block_from_wire(item) for item in value["content"]),
        created_at=value["createdAt"],
    )


def event_from_wire(value: Mapping[str, Any]) -> AgentEvent:
    return AgentEvent(
        run_id=value["runId"],
        sequence=value["sequence"],
        type=value["type"],
        timestamp=value["timestamp"],
        payload=value["payload"],
        session_id=value.get("sessionId"),
    )


def capabilities_from_wire(value: Mapping[str, Any]) -> Capabilities:
    return Capabilities(
        protocol_version=value["protocolVersion"],
        workspace_bases=tuple(value["workspaceBases"]),
        tool_provider_kinds=tuple(value["toolProviderKinds"]),
        model_protocols=tuple(value["modelProtocols"]),
        max_replay_batch=value["maxReplayBatch"],
    )
