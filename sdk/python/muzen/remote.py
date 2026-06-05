from __future__ import annotations

import asyncio
import inspect
import json
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, AsyncIterator, Callable, Dict, List, Optional, Union

from .runner_mapping import (
    _limits_to_runner,
    _session_to_runner,
    _source_to_remote,
)
from .sources import parse_review_source
from .types import (
    ModelProfileInput,
    ProviderProfileInput,
    ReviewArtifact,
    ReviewArtifactExport,
    ReviewArtifactExportOptions,
    ReviewArtifactReadOptions,
    ReviewCancelOptions,
    ReviewEvent,
    ReviewOptions,
    ReviewResult,
    ReviewSessionSnapshot,
    ReviewSourceLike,
)
from .wire_validation import (
    _unwrap_artifact_export,
    _unwrap_model_profile,
    _unwrap_model_profiles,
    _unwrap_optional_review_result,
    _unwrap_provider_profile,
    _unwrap_provider_profiles,
    _unwrap_review_artifact,
    _unwrap_review_events,
    _unwrap_review_snapshot,
)

RemoteTransport = Callable[
    [str, str, Optional[Dict[str, Any]], Dict[str, str]],
    Union[None, Dict[str, Any], List[Any]],
]


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
            await self._client._request_json(
                "GET", f"/v1/reviews/{_quote(self.id)}/result"
            )
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
