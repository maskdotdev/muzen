from __future__ import annotations

import asyncio
import os
import uuid
from dataclasses import asdict
from datetime import datetime, timezone
from typing import Any, AsyncIterator, Callable, Dict, List, Optional, Union

from .runner import RunnerStdioClient
from .sources import parse_review_source, source_key
from .types import (
    ReviewCancelOptions,
    ReviewCoverage,
    ReviewEvent,
    ReviewEventType,
    ReviewArtifact,
    ReviewArtifactExport,
    ReviewArtifactExportOptions,
    ReviewArtifactReadOptions,
    ReviewFinding,
    ReviewLimits,
    ReviewOptions,
    ReviewResult,
    ReviewSessionSnapshot,
    ReviewSource,
    ReviewSourceLike,
    ReviewStatus,
)


class MuzenUnsupportedFeatureError(RuntimeError):
    pass


async def create_muzen(
    *,
    runner_path: Optional[str] = None,
    runner_args: Optional[List[str]] = None,
    client_name: str = "muzen-py",
    client_version: Optional[str] = None,
) -> "Client":
    return await Client.create(
        runner_path=runner_path,
        runner_args=runner_args,
        client_name=client_name,
        client_version=client_version,
    )


class Client:
    def __init__(self, runner: RunnerStdioClient) -> None:
        self._runner = runner
        self._sessions: Dict[str, ReviewSession] = {}

    @classmethod
    async def create(
        cls,
        *,
        runner_path: Optional[str] = None,
        runner_args: Optional[List[str]] = None,
        client_name: str = "muzen-py",
        client_version: Optional[str] = None,
    ) -> "Client":
        runner = await RunnerStdioClient.start(
            runner_path or os.environ.get("MUZEN_RUNNER_PATH", "muzen-runner"),
            runner_args or ["stdio"],
        )
        await runner.handshake(client_name=client_name, client_version=client_version)
        return cls(runner)

    async def review(
        self,
        source_like: ReviewSourceLike,
        options: Optional[ReviewOptions] = None,
    ) -> "ReviewSession":
        source = parse_review_source(source_like)
        review_id = f"review-{uuid.uuid4()}"
        events: List[ReviewEvent] = []

        def on_notification(notification: Dict[str, Any]) -> None:
            event = _map_notification(notification)
            if event and event.review_id == review_id:
                events.append(event)

        unsubscribe = self._runner.on_notification(on_notification)
        try:
            runner_result = await self._runner.request(
                "run.start",
                _to_runner_start_params(review_id, source, options or ReviewOptions()),
            )
        finally:
            unsubscribe()
        result = _map_runner_result(review_id, source, runner_result)
        session = ReviewSession(
            runner=self._runner,
            id=review_id,
            status=result.status,
            source=source,
            events=events,
            result=result,
        )
        self._sessions[review_id] = session
        return session

    async def create_review_session(
        self,
        *,
        source: ReviewSourceLike,
        options: Optional[ReviewOptions] = None,
    ) -> "ReviewSession":
        return await self.review(source, options)

    async def resume_review(self, review_id: str) -> "ReviewSession":
        try:
            return self._sessions[review_id]
        except KeyError as error:
            raise MuzenUnsupportedFeatureError(
                "resume_review currently supports sessions created by this SDK process; durable session lookup is not implemented yet"
            ) from error

    async def close(self) -> None:
        await self._runner.close()


class ReviewSession:
    def __init__(
        self,
        *,
        runner: RunnerStdioClient,
        id: str,
        status: ReviewStatus,
        source: ReviewSource,
        events: List[ReviewEvent],
        result: Optional[ReviewResult],
    ) -> None:
        self._runner = runner
        self.id = id
        self.status = status
        self.source = source
        self._events = events
        self._result = result
        self._listeners: List[Callable[[ReviewEvent], None]] = []

    def subscribe(
        self,
        listener: Callable[[ReviewEvent], None],
        *,
        replay: bool = True,
    ) -> Callable[[], None]:
        if replay:
            for event in self._events:
                listener(event)
        self._listeners.append(listener)

        def unsubscribe() -> None:
            if listener in self._listeners:
                self._listeners.remove(listener)

        return unsubscribe

    async def events(self, *, after: Optional[str] = None) -> AsyncIterator[ReviewEvent]:
        start = _after_cursor_index(self._events, after)
        for event in self._events[start:]:
            yield event

    async def wait(
        self,
        *,
        timeout: Optional[Union[int, float, str]] = None,
    ) -> ReviewResult:
        if self._result is not None:
            return self._result
        timeout_seconds = _parse_timeout_seconds(timeout)
        result_task = asyncio.create_task(self.result())
        result = await asyncio.wait_for(result_task, timeout=timeout_seconds)
        if result is None:
            raise RuntimeError(f"review {self.id} has no final result yet")
        return result

    async def result(self) -> Optional[ReviewResult]:
        if self._result is not None:
            return self._result
        runner_result = await self._runner.request("run.result", {"runId": self.id})
        self._result = _map_runner_result(self.id, self.source, runner_result)
        self.status = self._result.status
        return self._result

    async def read_artifact(
        self,
        artifact_id: str,
        options: Optional[ReviewArtifactReadOptions] = None,
    ) -> ReviewArtifact:
        options = options or ReviewArtifactReadOptions()
        result = await self._runner.request(
            "artifact.read",
            {
                "runId": self.id,
                "artifactId": artifact_id,
                "view": options.view,
            },
        )
        if not isinstance(result, dict) or not isinstance(result.get("artifact"), dict):
            raise RuntimeError("muzen-runner returned an invalid artifact read result")
        return _map_runner_artifact(result["artifact"])

    async def export_artifacts(
        self,
        options: Optional[ReviewArtifactExportOptions] = None,
    ) -> ReviewArtifactExport:
        options = options or ReviewArtifactExportOptions()
        result = await self._runner.request(
            "artifact.export",
            {
                "runId": self.id,
                "artifactIds": options.artifact_ids,
                "view": options.view,
                "maxArtifacts": options.max_artifacts,
                "maxBytes": options.max_bytes,
            },
        )
        if not isinstance(result, dict):
            raise RuntimeError("muzen-runner returned an invalid artifact export result")
        return ReviewArtifactExport(
            view=result.get("view", options.view),
            artifact_count=result.get("artifactCount", 0),
            total_bytes=result.get("totalBytes", 0),
            artifacts=[
                _map_runner_artifact(artifact)
                for artifact in result.get("artifacts", [])
                if isinstance(artifact, dict)
            ],
        )

    async def cancel(
        self,
        reason: Optional[Union[str, ReviewCancelOptions]] = None,
    ) -> None:
        cancel_reason = reason.reason if isinstance(reason, ReviewCancelOptions) else reason
        await self._runner.request(
            "run.cancel",
            {"runId": self.id, "reason": cancel_reason or "cancelled"},
        )
        if self.status not in ("completed", "failed", "cancelled"):
            self.status = "cancelled"
            self._record(
                ReviewEvent(
                    cursor=str(len(self._events) + 1),
                    type="session.cancelled",
                    review_id=self.id,
                    timestamp_utc=_timestamp_utc(),
                    payload={"reason": cancel_reason},
                )
            )

    async def refresh(self) -> ReviewSessionSnapshot:
        status = await self._runner.request("run.status", {"runId": self.id})
        if isinstance(status, dict) and isinstance(status.get("status"), str):
            self.status = _map_runner_status(status["status"])
        return ReviewSessionSnapshot(
            id=self.id,
            status=self.status,
            source=self.source,
            result=self._result,
        )

    def _record(self, event: ReviewEvent) -> None:
        self._events.append(event)
        for listener in list(self._listeners):
            listener(event)


def _to_runner_start_params(
    review_id: str,
    source: ReviewSource,
    options: ReviewOptions,
) -> Dict[str, Any]:
    if source.type != "local":
        raise MuzenUnsupportedFeatureError(
            f"review source {source_key(source)} requires provider materialization, which is not implemented in this preview"
        )
    changed_files = options.scope_files or source.changed_files
    return {
        "protocolVersion": "muzen.runner.v1",
        "runId": review_id,
        "repo": source.repo,
        "changedFiles": changed_files,
        "sessions": [_session_to_runner(session, options.model) for session in options.sessions],
        "limits": _limits_to_runner(options.limits),
    }


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


def _after_cursor_index(events: List[ReviewEvent], after: Optional[str]) -> int:
    if not after:
        return 0
    for index, event in enumerate(events):
        if event.cursor == after:
            return index + 1
    return 0


def _parse_timeout_seconds(timeout: Optional[Union[int, float, str]]) -> Optional[float]:
    if timeout is None:
        return None
    if isinstance(timeout, (int, float)):
        return float(timeout) / 1000
    text = timeout.strip()
    if text.endswith("ms"):
        return float(text[:-2]) / 1000
    if text.endswith("s"):
        return float(text[:-1])
    if text.endswith("m"):
        return float(text[:-1]) * 60
    return float(text) / 1000


def _camel_dict(value: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "maxTurns": value["max_turns"],
        "maxToolCalls": value["max_tool_calls"],
        "maxPromptTokens": value["max_prompt_tokens"],
        "maxOutputTokens": value["max_output_tokens"],
    }


def _timestamp_utc() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
