from __future__ import annotations

from dataclasses import asdict
from typing import Any, Dict, List, Optional

from .sources import source_key
from .types import (
    ReviewArtifact,
    ReviewCoverage,
    ReviewEvent,
    ReviewEventType,
    ReviewFinding,
    ReviewLimits,
    ReviewOptions,
    ReviewResult,
    ReviewSource,
    ReviewStatus,
)


def _to_runner_start_params(
    review_id: str,
    source: ReviewSource,
    options: ReviewOptions,
) -> Dict[str, Any]:
    changed_files = options.scope_files or (source.changed_files if source.type == "local" else [])
    payload = {
        "protocolVersion": "muzen.runner.v1",
        "runId": review_id,
        "source": _source_to_remote(source),
        "changedFiles": changed_files,
        "sessions": [_session_to_runner(session, options.model) for session in options.sessions],
        "limits": _limits_to_runner(options.limits),
    }
    if source.type == "local":
        payload["repo"] = source.repo
    return payload


def _session_to_runner(session: Any, default_model: Optional[str]) -> Dict[str, Any]:
    payload = {
        "id": session.id,
        "role": session.role,
        "objective": session.objective,
        "cwd": session.cwd,
        "modelProfileId": session.model_profile_id or default_model,
    }
    if session.budget is not None:
        payload["budget"] = _camel_dict(asdict(session.budget))
    return payload


def _limits_to_runner(limits: Optional[ReviewLimits]) -> Optional[Dict[str, Any]]:
    if limits is None:
        return None
    return {
        "maxActiveSessions": limits.max_active_sessions,
        "maxFileBytes": limits.max_file_bytes,
        "maxSearchMatches": limits.max_search_matches,
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


def _map_runner_finding(value: Dict[str, Any]) -> ReviewFinding:
    return ReviewFinding(
        id=value.get("id", ""),
        severity="error" if value.get("publishable") else "info",
        category="other",
        title=value.get("title", ""),
        message=value.get("claim", ""),
    )


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
            "changedFiles": source.changed_files,
        }
    if source.type == "github_pull_request":
        return {
            "type": "github_pull_request",
            "owner": source.owner,
            "repo": source.repo,
            "number": source.number,
        }
    return {
        "type": "gitlab_merge_request",
        "owner": source.owner,
        "repo": source.repo,
        "number": source.number,
    }


def _camel_dict(value: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "maxTurns": value["max_turns"],
        "maxToolCalls": value["max_tool_calls"],
        "maxPromptTokens": value["max_prompt_tokens"],
        "maxOutputTokens": value["max_output_tokens"],
    }
