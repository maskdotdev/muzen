from __future__ import annotations

import asyncio
import os
import uuid
from datetime import datetime, timezone
from typing import Any, AsyncIterator, Callable, Dict, List, Optional, Union

from .errors import MuzenUnsupportedFeatureError
from .remote import RemoteClient, RemoteTransport
from .runner import RunnerStdioClient
from .runner_mapping import (
    _map_notification,
    _map_runner_artifact,
    _map_runner_result,
    _map_runner_status,
    _to_runner_start_params,
)
from .sources import parse_review_source
from .types import (
    ReviewCancelOptions,
    ReviewEvent,
    ReviewArtifact,
    ReviewArtifactExport,
    ReviewArtifactExportOptions,
    ReviewArtifactReadOptions,
    ReviewOptions,
    ReviewResult,
    ReviewSessionSnapshot,
    ReviewSource,
    ReviewSourceLike,
    ReviewStatus,
)


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


def _timestamp_utc() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
