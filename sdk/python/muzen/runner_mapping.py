from __future__ import annotations

from dataclasses import asdict
from typing import Any, Dict, List, Optional

from .sources import source_key
from .types import (
    AnthropicReviewModelSpec,
    OpenAIReviewModelSpec,
    ReviewArtifact,
    ReviewCoverage,
    ReviewEvent,
    ReviewEventType,
    ReviewFinding,
    ReviewFindingEvidence,
    ReviewLimits,
    ReviewOptions,
    ReviewResult,
    ReviewSource,
    ReviewStatus,
    SwarmAgentOutput,
    SwarmOptions,
    SwarmResult,
    SwarmUsage,
)


def _to_runner_start_params(
    review_id: str,
    source: ReviewSource,
    options: ReviewOptions,
) -> Dict[str, Any]:
    changed_files = options.scope_files
    model_plan = _model_plan(options.model, [])
    payload = {
        "protocolVersion": "muzen.runner.v1",
        "runId": review_id,
        "source": _source_to_remote(source),
        "changedFiles": changed_files,
        "metadata": options.metadata,
        "change": _change_to_runner(options),
        "instructions": [_instruction_to_runner(item) for item in options.instructions],
        "sessions": [],
        "limits": _limits_to_runner(options.limits),
        "model": model_plan["runnerModel"],
        "tools": [],
    }
    if source.type == "local":
        payload["repo"] = source.repo
    return payload


def _to_swarm_start_params(run_id: str, options: SwarmOptions) -> Dict[str, Any]:
    if not options.agents:
        raise ValueError("run_swarm requires at least one agent")
    model_plan = _model_plan(options.model, options.agents)
    return {
        "protocolVersion": "muzen.runner.v1",
        "runId": run_id,
        "repo": options.repo,
        "source": {"type": "local", "repo": options.repo},
        "changedFiles": options.files,
        "metadata": options.metadata,
        "instructions": [],
        "sessions": [
            _swarm_agent_to_runner(agent, model_plan["sessionProfileIds"])
            for agent in options.agents
        ],
        "limits": _limits_to_runner(options.limits),
        "model": model_plan["runnerModel"],
        "tools": [_tool_to_runner(tool) for tool in options.tools],
    }


def _swarm_agent_to_runner(agent: Any, session_profiles: Dict[str, str]) -> Dict[str, Any]:
    payload = {
        "id": agent.id,
        "role": "generalist",
        "objective": agent.objective,
        "cwd": None,
        "modelProfileId": session_profiles.get(agent.id),
        "instructions": [_instruction_to_runner(item) for item in agent.instructions],
        "toolGrants": agent.tool_grants,
    }
    if agent.budget is not None:
        payload["budget"] = _camel_dict(asdict(agent.budget))
    return payload


_HOSTED_MODEL_SPECS = (OpenAIReviewModelSpec, AnthropicReviewModelSpec)


def _model_plan(model: Any, sessions: List[Any]) -> Dict[str, Any]:
    profiles: Dict[str, Dict[str, Any]] = {}
    session_profiles: Dict[str, str] = {}
    default_profile_id: Optional[str] = None
    if isinstance(model, _HOSTED_MODEL_SPECS):
        default_profile_id = _add_hosted_profile(profiles, "default", model)
    for session in sessions:
        if isinstance(session.model, _HOSTED_MODEL_SPECS):
            profile_id = _add_hosted_profile(
                profiles, f"session:{session.id}", session.model
            )
            session_profiles[session.id] = profile_id
            default_profile_id = default_profile_id or profile_id
    if not profiles:
        return {"runnerModel": None, "sessionProfileIds": session_profiles}
    if not isinstance(model, _HOSTED_MODEL_SPECS) and any(
        not isinstance(session.model, _HOSTED_MODEL_SPECS)
        for session in sessions
    ):
        raise ValueError(
            "session model overrides require a run-level model when any session omits its own model"
        )
    return {
        "runnerModel": {
            "callback": False,
            "defaultModelProfileId": default_profile_id,
            "modelProfiles": list(profiles.values()),
        },
        "sessionProfileIds": session_profiles,
    }


def _add_hosted_profile(
    profiles: Dict[str, Dict[str, Any]], profile_id: str, model: Any
) -> str:
    key = repr(
        (
            model.provider,
            model.model,
            model.credential,
            model.base_url,
            model.api_protocol,
            model.max_input_tokens,
            model.max_output_tokens,
            model.temperature,
            model.top_p,
        )
    )
    if key in profiles:
        return profiles[key]["id"]
    profiles[key] = {
        "id": profile_id,
        "provider": _runner_provider_for_hosted_model(model.provider),
        "model": model.model,
        "credential": _credential_to_runner(model.credential),
        "baseUrl": model.base_url,
        "apiProtocol": model.api_protocol,
        "maxInputTokens": model.max_input_tokens,
        "maxOutputTokens": model.max_output_tokens,
        "temperature": model.temperature,
        "topP": model.top_p,
    }
    return profile_id


def _runner_provider_for_hosted_model(provider: str) -> str:
    if provider == "openai":
        return "openai_compatible"
    return provider


def _credential_to_runner(credential: Any) -> Dict[str, str]:
    if credential.env:
        return {"env": credential.env}
    return {"secretRef": credential.secret_ref}


def _limits_to_runner(limits: Optional[ReviewLimits]) -> Optional[Dict[str, Any]]:
    if limits is None:
        return None
    return {
        "maxActiveSessions": limits.max_active_sessions,
        "maxFileBytes": limits.max_file_bytes,
        "maxSearchMatches": limits.max_search_matches,
    }


def _semantic_config_to_runner(config: Any) -> Dict[str, Any]:
    payload = {
        "mode": config.mode,
        "allowRestrictedHostedInputs": config.allow_restricted_hosted_inputs,
        "maxEmbeddingInputs": config.max_embedding_inputs,
    }
    if config.provider is not None:
        payload["provider"] = config.provider
    if getattr(config, "hosted_base_url", None) is not None:
        payload["hostedBaseUrl"] = config.hosted_base_url
    if getattr(config, "hosted_model", None) is not None:
        payload["hostedModel"] = config.hosted_model
    if getattr(config, "hosted_credential_ref", None) is not None:
        payload["hostedCredentialRef"] = config.hosted_credential_ref
    return payload


def _change_to_runner(options: ReviewOptions) -> Optional[Dict[str, Any]]:
    change = options.change
    if change is None:
        return None
    return {
        "kind": change.kind,
        "baseRevision": change.base_revision,
        "startRevision": change.start_revision,
        "headRevision": change.head_revision,
        "changedFiles": [
            {"path": file.path, "status": file.status}
            for file in change.changed_files
        ],
        "diff": change.diff,
        "reviewTarget": change.review_target,
    }


def _instruction_to_runner(instruction: Any) -> Dict[str, Any]:
    return {
        "kind": instruction.kind,
        "text": instruction.text,
        "trusted": instruction.trusted,
    }


def _tool_to_runner(tool: Any) -> Dict[str, Any]:
    return {
        "id": tool.id,
        "description": tool.description,
        "parameters": tool.parameters,
        "effects": tool.effects,
        "cacheable": tool.cacheable,
        "providerResources": tool.provider_resources,
    }


def _map_notification(notification: Dict[str, Any]) -> Optional[ReviewEvent]:
    if notification.get("method") != "event.review":
        return None
    record = notification.get("params")
    if not isinstance(record, dict):
        return None
    return ReviewEvent(
        cursor=str(record.get("seq")),
        type=_map_runner_event_type(record.get("event")),
        review_id=record.get("runId") or "unknown",
        timestamp_utc=record.get("timestampUtc"),
        payload=record.get("event"),
    )


def _map_runner_event_type(event: Any) -> ReviewEventType:
    kind = next(iter(event.keys()), None) if isinstance(event, dict) else None
    if kind == "runStarted":
        return "session.started"
    if kind == "repoManifestCompleted":
        return "scope.inferred"
    if kind == "sessionStarted":
        return "agent.started"
    if kind == "sessionFinished":
        return "agent.completed"
    if kind == "toolBatchStarted":
        return "tool.started"
    if kind in ("toolCallCompleted", "toolCallDenied"):
        return "tool.completed"
    if kind == "findingRecorded":
        return "finding.created"
    if kind == "snapshotFinished":
        return "repo.materialized"
    if kind == "runFinished":
        value = event.get("runFinished") if isinstance(event, dict) else {}
        status = value.get("status") if isinstance(value, dict) else None
        if status == "completed":
            return "session.completed"
        if status == "cancelled":
            return "session.cancelled"
        return "session.failed"
    return "runner.event"


def _map_runner_result(review_id: str, source: ReviewSource, value: Any) -> ReviewResult:
    if not isinstance(value, dict):
        raise RuntimeError("muzen-runner returned an invalid run result")
    summary = value.get("summary") or {}
    findings = [_map_runner_finding(finding) for finding in value.get("findings", [])]
    status = _map_runner_status(value.get("status"))
    return ReviewResult(
        review_id=review_id,
        session_id=review_id,
        status=status,
        conclusion=_conclusion_from_findings(findings),
        summary=(
            f"Review completed {summary.get('completedSessions', 0)}/{summary.get('sessions', 0)} "
            f"session(s), produced {len(findings)} finding(s), used {summary.get('modelCalls', 0)} "
            f"model call(s), {summary.get('toolCalls', 0)} tool call(s), and "
            f"{summary.get('totalTokens', 0)} total token(s)."
        ),
        findings=findings,
        coverage=_coverage_from_snapshots(value.get("snapshots", [])),
        metadata={
            "runnerRunId": value.get("runId"),
            "runnerStatus": value.get("status"),
            "source": source_key(source),
        },
    )


def _map_swarm_result(run_id: str, value: Any) -> SwarmResult:
    if not isinstance(value, dict):
        raise RuntimeError("muzen-runner returned an invalid run result")
    summary = value.get("summary") or {}
    return SwarmResult(
        run_id=run_id,
        status=_map_swarm_run_status(value.get("status")),
        outputs=[
            _map_swarm_agent_output(output)
            for output in value.get("sessionOutputs", [])
            if isinstance(output, dict)
        ],
        usage=SwarmUsage(
            agents=summary.get("sessions", 0),
            completed_agents=summary.get("completedSessions", 0),
            model_calls=summary.get("modelCalls", 0),
            tool_calls=summary.get("toolCalls", 0),
            input_tokens=summary.get("inputTokens", 0),
            output_tokens=summary.get("outputTokens", 0),
            total_tokens=summary.get("totalTokens", 0),
        ),
        metadata=value.get("metadata") if isinstance(value.get("metadata"), dict) else {},
    )


def _map_swarm_run_status(status: Any) -> str:
    if status in ("completed", "partial", "failed", "cancelled"):
        return status
    return "failed"


def _map_swarm_agent_output(value: Dict[str, Any]) -> SwarmAgentOutput:
    return SwarmAgentOutput(
        agent_id=value.get("sessionId", ""),
        status=_map_swarm_agent_status(value.get("status")),
        completed=bool(value.get("completed")),
        output=value.get("output") if isinstance(value.get("output"), str) else None,
    )


def _map_swarm_agent_status(status: Any) -> str:
    if status in ("done", "failed", "cancelled", "partial"):
        return status
    return "failed"


def _map_runner_finding(value: Dict[str, Any]) -> ReviewFinding:
    return ReviewFinding(
        id=value.get("id", ""),
        severity=_map_finding_severity(value),
        category="other",
        title=value.get("title", ""),
        message=value.get("claim", ""),
        location=value.get("location"),
        confidence=value.get("confidence"),
        validation_status=value.get("validationStatus"),
        evidence=[
            ReviewFindingEvidence(
                evidence_id=evidence.get("evidenceId", ""),
                artifact_id=evidence.get("artifactId", ""),
                kind=evidence.get("kind", ""),
                content_hash=evidence.get("contentHash", ""),
                producing_tool_call_id=evidence.get("producingToolCallId", ""),
            )
            for evidence in value.get("evidence", [])
            if isinstance(evidence, dict)
        ],
        discovered_by=list(value.get("discoveredBy", [])),
        validated_by=list(value.get("validatedBy", [])),
        challenged_by=list(value.get("challengedBy", [])),
    )


def _map_finding_severity(value: Dict[str, Any]) -> str:
    severity = value.get("severity")
    if severity in ("blocker", "high"):
        return "error"
    if severity in ("medium", "low"):
        return "warning"
    if severity == "nit":
        return "info"
    return "error" if value.get("publishable") else "info"


def _map_runner_artifact(value: Dict[str, Any]) -> ReviewArtifact:
    return ReviewArtifact(
        artifact_id=value.get("artifactId", ""),
        bytes=value.get("bytes", 0),
        content_hash=value.get("contentHash", ""),
        content=value.get("content", ""),
    )


def _conclusion_from_findings(findings: List[ReviewFinding]) -> str:
    if any(finding.severity == "error" for finding in findings):
        return "changes_requested"
    return "approved" if not findings else "commented"


def _coverage_from_snapshots(snapshots: List[Dict[str, Any]]) -> ReviewCoverage:
    files_considered = sum(snapshot.get("files", 0) for snapshot in snapshots)
    files_reviewed = sum(snapshot.get("capturedFiles", 0) for snapshot in snapshots)
    return ReviewCoverage(
        files_considered=files_considered,
        files_reviewed=files_reviewed,
        files_skipped=max(0, files_considered - files_reviewed),
    )


def _map_runner_status(status: Any) -> ReviewStatus:
    if status in ("created", "queued", "running", "completed", "failed", "cancelled"):
        return status
    return "failed"


def _source_to_remote(source: ReviewSource) -> Dict[str, Any]:
    if source.type == "local":
        return {
            "type": "local",
            "repo": source.repo,
        }
    if source.type == "github_pull_request":
        return {
            "type": "github_pull_request",
            "owner": source.owner,
            "repo": source.repo,
            "number": source.number,
        }
    if source.type == "gitlab_merge_request":
        return {
            "type": "gitlab_merge_request",
            "owner": source.owner,
            "repo": source.repo,
            "number": source.number,
        }
    if source.type == "raw_snapshot":
        return {
            "type": "raw_snapshot",
            "root": source.root,
        }
    if source.type == "perforce_changelist":
        return {
            "type": "perforce_changelist",
            "server": source.server,
            "changelist": source.changelist,
            "client": source.client,
            "depotPaths": source.depot_paths,
        }
    return {
        "type": "custom",
        "provider": source.provider,
        "id": source.id,
    }


def _camel_dict(value: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "maxTurns": value["max_turns"],
        "maxToolCalls": value["max_tool_calls"],
        "maxPromptTokens": value["max_prompt_tokens"],
        "maxOutputTokens": value["max_output_tokens"],
    }
