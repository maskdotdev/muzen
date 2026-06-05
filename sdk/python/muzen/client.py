from __future__ import annotations

import asyncio
import inspect
import json
import os
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import asdict
from datetime import datetime, timezone
from typing import Any, AsyncIterator, Callable, Dict, List, Optional, Union

from .runner import RunnerStdioClient
from .sources import parse_review_source, source_key
from .types import (
    ModelProfile,
    ModelProfileInput,
    ProviderProfile,
    ProviderProfileInput,
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

RemoteTransport = Callable[
    [str, str, Optional[Dict[str, Any]], Dict[str, str]],
    Union[None, Dict[str, Any], List[Any]],
]


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


def create_muzen_client(
    *,
    base_url: str,
    token: Optional[str] = None,
    transport: Optional[RemoteTransport] = None,
) -> "RemoteClient":
    return RemoteClient(base_url=base_url, token=token, transport=transport)


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
                "local resume_review is process-local for Client.create(); use create_muzen_client(base_url=...).resume_review(review_id) for durable review lookup"
            ) from error

    async def close(self) -> None:
        await self._runner.close()

    def workspace(self, workspace_id: str) -> "RunnerBackedWorkspace":
        return RunnerBackedWorkspace(self, workspace_id)


class RemoteClient:
    def __init__(
        self,
        *,
        base_url: str,
        token: Optional[str] = None,
        transport: Optional[RemoteTransport] = None,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._token = token
        self._transport = transport

    async def review(
        self,
        source_like: ReviewSourceLike,
        options: Optional[ReviewOptions] = None,
    ) -> "RemoteReviewSession":
        source = parse_review_source(source_like)
        payload = {
            "source": _source_to_remote(source),
            "options": _review_options_to_remote(options or ReviewOptions()),
        }
        snapshot = _unwrap_review_snapshot(
            await self._request_json("POST", "/v1/reviews", payload)
        )
        return RemoteReviewSession(self, snapshot)

    async def create_review_session(
        self,
        *,
        source: ReviewSourceLike,
        options: Optional[ReviewOptions] = None,
    ) -> "RemoteReviewSession":
        return await self.review(source, options)

    async def resume_review(self, review_id: str) -> "RemoteReviewSession":
        snapshot = _unwrap_review_snapshot(
            await self._request_json("GET", f"/v1/reviews/{_quote(review_id)}")
        )
        return RemoteReviewSession(self, snapshot)

    def workspace(self, workspace_id: str) -> "RemoteWorkspace":
        return RemoteWorkspace(self, workspace_id)

    async def close(self) -> None:
        return None

    async def _request_json(
        self,
        method: str,
        path: str,
        body: Optional[Dict[str, Any]] = None,
    ) -> Any:
        headers = {"Content-Type": "application/json"}
        if self._token:
            headers["Authorization"] = f"Bearer {self._token}"
        if self._transport is not None:
            result = self._transport(method, path, body, headers)
            if inspect.isawaitable(result):
                return await result
            return result
        return await asyncio.to_thread(
            _http_json,
            method,
            f"{self._base_url}{path}",
            body,
            headers,
        )


class RemoteWorkspace:
    def __init__(self, client: RemoteClient, workspace_id: str) -> None:
        self._client = client
        self.id = workspace_id
        self.models = RemoteWorkspaceProfileCollection(
            client,
            workspace_id,
            "models",
            _unwrap_model_profile,
            _unwrap_model_profiles,
            _model_profile_input_to_remote,
        )
        self.providers = RemoteWorkspaceProfileCollection(
            client,
            workspace_id,
            "providers",
            _unwrap_provider_profile,
            _unwrap_provider_profiles,
            _provider_profile_input_to_remote,
        )

    async def review(
        self,
        source_like: ReviewSourceLike,
        options: Optional[ReviewOptions] = None,
    ) -> "RemoteReviewSession":
        source = parse_review_source(source_like)
        snapshot = _unwrap_review_snapshot(
            await self._client._request_json(
                "POST",
                f"/v1/workspaces/{_quote(self.id)}/reviews",
                {
                    "source": _source_to_remote(source),
                    "options": _review_options_to_remote(options or ReviewOptions()),
                },
            )
        )
        return RemoteReviewSession(self._client, snapshot)


class RemoteWorkspaceProfileCollection:
    def __init__(
        self,
        client: RemoteClient,
        workspace_id: str,
        kind: str,
        unwrap_one: Callable[[Any], Any],
        unwrap_many: Callable[[Any], List[Any]],
        encode_input: Callable[[Any], Dict[str, Any]],
    ) -> None:
        self._client = client
        self._workspace_id = workspace_id
        self._kind = kind
        self._unwrap_one = unwrap_one
        self._unwrap_many = unwrap_many
        self._encode_input = encode_input

    async def set(self, name: str, input: Any) -> Any:
        return self._unwrap_one(
            await self._client._request_json(
                "PUT",
                self._profile_path(name),
                self._encode_input(input),
            )
        )

    async def get(self, name: str) -> Any:
        response = await self._client._request_json("GET", self._profile_path(name))
        if response is None:
            return None
        return self._unwrap_one(response)

    async def list(self) -> List[Any]:
        return self._unwrap_many(
            await self._client._request_json("GET", self._collection_path())
        )

    def _collection_path(self) -> str:
        return f"/v1/workspaces/{_quote(self._workspace_id)}/{self._kind}"

    def _profile_path(self, name: str) -> str:
        return f"{self._collection_path()}/{_quote(name)}"


class RunnerBackedWorkspace:
    def __init__(self, client: Client, workspace_id: str) -> None:
        self._client = client
        self.id = workspace_id
        self.models = UnsupportedWorkspaceProfileCollection("model")
        self.providers = UnsupportedWorkspaceProfileCollection("provider")

    async def review(
        self,
        source_like: ReviewSourceLike,
        options: Optional[ReviewOptions] = None,
    ) -> "ReviewSession":
        return await self._client.review(source_like, options)


class UnsupportedWorkspaceProfileCollection:
    def __init__(self, kind: str) -> None:
        self._kind = kind

    async def set(self, name: str, input: Any) -> Any:
        raise self._error()

    async def get(self, name: str) -> Any:
        raise self._error()

    async def list(self) -> List[Any]:
        raise self._error()

    def _error(self) -> MuzenUnsupportedFeatureError:
        return MuzenUnsupportedFeatureError(
            f"workspace {self._kind} profiles require remote workspace storage; Client.create() only supports local runner review execution in this preview"
        )


class RemoteReviewSession:
    def __init__(self, client: RemoteClient, snapshot: ReviewSessionSnapshot) -> None:
        self._client = client
        self.id = snapshot.id
        self.status = snapshot.status
        self.source = snapshot.source
        self._result = snapshot.result

    def subscribe(
        self,
        listener: Callable[[ReviewEvent], None],
        *,
        replay: bool = True,
    ) -> Callable[[], None]:
        cancelled = False

        async def replay_events() -> None:
            async for event in self.events():
                if cancelled:
                    return
                listener(event)

        if replay:
            asyncio.create_task(replay_events())

        def unsubscribe() -> None:
            nonlocal cancelled
            cancelled = True

        return unsubscribe

    async def events(self, *, after: Optional[str] = None) -> AsyncIterator[ReviewEvent]:
        path = f"/v1/reviews/{_quote(self.id)}/events"
        if after:
            path = f"{path}?after={_quote(after)}"
        for event in _unwrap_review_events(
            await self._client._request_json("GET", path)
        ):
            yield event

    async def wait(
        self,
        *,
        timeout: Optional[Union[int, float, str]] = None,
    ) -> ReviewResult:
        timeout_seconds = _parse_timeout_seconds(timeout)
        return await asyncio.wait_for(self._wait_for_result(), timeout=timeout_seconds)

    async def _wait_for_result(self) -> ReviewResult:
        while True:
            result = await self.result()
            if result is not None:
                return result
            await asyncio.sleep(0.25)

    async def result(self) -> Optional[ReviewResult]:
        if self._result is not None:
            return self._result
        self._result = _unwrap_optional_review_result(
            await self._client._request_json("GET", f"/v1/reviews/{_quote(self.id)}/result")
        )
        if self._result is not None:
            self.status = self._result.status
        return self._result

    async def cancel(
        self,
        reason: Optional[Union[str, ReviewCancelOptions]] = None,
    ) -> None:
        cancel_reason = reason.reason if isinstance(reason, ReviewCancelOptions) else reason
        await self._client._request_json(
            "POST",
            f"/v1/reviews/{_quote(self.id)}/cancel",
            {"reason": cancel_reason or "cancelled"},
        )
        self.status = "cancelled"

    async def refresh(self) -> ReviewSessionSnapshot:
        snapshot = _unwrap_review_snapshot(
            await self._client._request_json("GET", f"/v1/reviews/{_quote(self.id)}")
        )
        self.status = snapshot.status
        self._result = snapshot.result
        return snapshot

    async def read_artifact(
        self,
        artifact_id: str,
        options: Optional[ReviewArtifactReadOptions] = None,
    ) -> ReviewArtifact:
        options = options or ReviewArtifactReadOptions()
        return _unwrap_review_artifact(
            await self._client._request_json(
                "GET",
                f"/v1/reviews/{_quote(self.id)}/artifacts/{_quote(artifact_id)}?view={_quote(options.view)}",
            )
        )

    async def export_artifacts(
        self,
        options: Optional[ReviewArtifactExportOptions] = None,
    ) -> ReviewArtifactExport:
        options = options or ReviewArtifactExportOptions()
        return _unwrap_artifact_export(
            await self._client._request_json(
                "POST",
                f"/v1/reviews/{_quote(self.id)}/artifacts/export",
                {
                    "view": options.view,
                    "artifactIds": options.artifact_ids,
                    "maxArtifacts": options.max_artifacts,
                    "maxBytes": options.max_bytes,
                },
            )
        )


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


def _review_options_to_remote(options: ReviewOptions) -> Dict[str, Any]:
    return {
        "dedupe": options.dedupe,
        "cancelSuperseded": options.cancel_superseded,
        "model": options.model,
        "scope": {
            "files": options.scope_files,
            "include": options.scope_include,
            "exclude": options.scope_exclude,
        },
        "metadata": options.metadata,
        "sessions": [_session_to_runner(session, options.model) for session in options.sessions],
        "limits": _limits_to_runner(options.limits),
    }


def _model_profile_input_to_remote(input: ModelProfileInput) -> Dict[str, Any]:
    return {
        "provider": input.provider,
        "model": input.model,
        "secretRef": input.secret_ref,
        "baseUrl": input.base_url,
        "routing": input.routing,
    }


def _provider_profile_input_to_remote(input: ProviderProfileInput) -> Dict[str, Any]:
    return {
        "provider": input.provider,
        "secretRef": input.secret_ref,
        "baseUrl": input.base_url,
        "routing": input.routing,
    }


def _unwrap_review_snapshot(value: Any) -> ReviewSessionSnapshot:
    snapshot = value.get("review") if isinstance(value, dict) and isinstance(value.get("review"), dict) else value
    if not isinstance(snapshot, dict):
        raise RuntimeError("Muzen remote returned an invalid review session snapshot")
    return ReviewSessionSnapshot(
        id=snapshot["id"],
        status=_map_runner_status(snapshot["status"]),
        source=_remote_source(snapshot["source"]),
        result=_unwrap_optional_review_result(snapshot.get("result")),
    )


def _unwrap_optional_review_result(value: Any) -> Optional[ReviewResult]:
    if value is None:
        return None
    result = value.get("result") if isinstance(value, dict) and "result" in value else value
    if result is None:
        return None
    if not isinstance(result, dict):
        raise RuntimeError("Muzen remote returned an invalid review result")
    return _remote_result(result)


def _unwrap_review_events(value: Any) -> List[ReviewEvent]:
    events = value.get("events") if isinstance(value, dict) and isinstance(value.get("events"), list) else value
    if not isinstance(events, list):
        raise RuntimeError("Muzen remote returned invalid review events")
    return [
        ReviewEvent(
            cursor=str(event["cursor"]),
            type=event["type"],
            review_id=event["reviewId"],
            timestamp_utc=event["timestampUtc"],
            payload=event.get("payload"),
        )
        for event in events
    ]


def _unwrap_review_artifact(value: Any) -> ReviewArtifact:
    artifact = value.get("artifact") if isinstance(value, dict) and isinstance(value.get("artifact"), dict) else value
    if not isinstance(artifact, dict):
        raise RuntimeError("Muzen remote returned an invalid review artifact")
    return _map_runner_artifact(artifact)


def _unwrap_artifact_export(value: Any) -> ReviewArtifactExport:
    if not isinstance(value, dict):
        raise RuntimeError("Muzen remote returned an invalid artifact export")
    return ReviewArtifactExport(
        view=value.get("view", "redacted"),
        artifact_count=value.get("artifactCount", 0),
        total_bytes=value.get("totalBytes", 0),
        artifacts=[
            _map_runner_artifact(artifact)
            for artifact in value.get("artifacts", [])
            if isinstance(artifact, dict)
        ],
    )


def _unwrap_model_profile(value: Any) -> ModelProfile:
    profile = value.get("profile") if isinstance(value, dict) and isinstance(value.get("profile"), dict) else value
    if not isinstance(profile, dict):
        raise RuntimeError("Muzen remote returned an invalid model profile")
    return ModelProfile(
        workspace_id=profile["workspaceId"],
        name=profile["name"],
        version=profile["version"],
        provider=profile["provider"],
        model=profile["model"],
        secret_ref=profile.get("secretRef"),
        base_url=profile.get("baseUrl"),
        routing=profile.get("routing") or {},
        updated_at_utc=profile.get("updatedAtUtc", ""),
    )


def _unwrap_model_profiles(value: Any) -> List[ModelProfile]:
    profiles = value.get("profiles") if isinstance(value, dict) and isinstance(value.get("profiles"), list) else value
    if not isinstance(profiles, list):
        raise RuntimeError("Muzen remote returned invalid model profiles")
    return [_unwrap_model_profile(profile) for profile in profiles]


def _unwrap_provider_profile(value: Any) -> ProviderProfile:
    profile = value.get("profile") if isinstance(value, dict) and isinstance(value.get("profile"), dict) else value
    if not isinstance(profile, dict):
        raise RuntimeError("Muzen remote returned an invalid provider profile")
    return ProviderProfile(
        workspace_id=profile["workspaceId"],
        name=profile["name"],
        version=profile["version"],
        provider=profile["provider"],
        secret_ref=profile.get("secretRef"),
        base_url=profile.get("baseUrl"),
        routing=profile.get("routing") or {},
        updated_at_utc=profile.get("updatedAtUtc", ""),
    )


def _unwrap_provider_profiles(value: Any) -> List[ProviderProfile]:
    profiles = value.get("profiles") if isinstance(value, dict) and isinstance(value.get("profiles"), list) else value
    if not isinstance(profiles, list):
        raise RuntimeError("Muzen remote returned invalid provider profiles")
    return [_unwrap_provider_profile(profile) for profile in profiles]


def _remote_source(value: Dict[str, Any]) -> ReviewSource:
    if value["type"] == "local":
        return ReviewSource(
            type="local",
            repo=value["repo"],
            changed_files=value.get("changedFiles", []),
        )
    return ReviewSource(
        type=value["type"],
        owner=value.get("owner"),
        repo=value["repo"],
        number=value.get("number"),
    )


def _remote_result(value: Dict[str, Any]) -> ReviewResult:
    return ReviewResult(
        review_id=value["reviewId"],
        session_id=value["sessionId"],
        status=_map_runner_status(value["status"]),
        conclusion=value["conclusion"],
        summary=value["summary"],
        findings=[
            ReviewFinding(
                id=finding.get("id", ""),
                severity=finding.get("severity", "info"),
                category=finding.get("category", "other"),
                title=finding.get("title", ""),
                message=finding.get("message", ""),
                location=finding.get("location"),
                suggested_fix=finding.get("suggestedFix"),
                confidence=finding.get("confidence"),
            )
            for finding in value.get("findings", [])
        ],
        coverage=ReviewCoverage(
            files_considered=value.get("coverage", {}).get("filesConsidered", 0),
            files_reviewed=value.get("coverage", {}).get("filesReviewed", 0),
            files_skipped=value.get("coverage", {}).get("filesSkipped", 0),
        ),
        metadata=value.get("metadata") or {},
    )


def _http_json(
    method: str,
    url: str,
    body: Optional[Dict[str, Any]],
    headers: Dict[str, str],
) -> Any:
    data = json.dumps(body).encode("utf-8") if body is not None else None
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request) as response:
            if response.status == 204:
                return None
            payload = response.read()
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"Muzen remote request failed: {error.code} {error.reason}") from error
    if not payload:
        return None
    return json.loads(payload.decode("utf-8"))


def _quote(value: str) -> str:
    return urllib.parse.quote(value, safe="")


def _timestamp_utc() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
