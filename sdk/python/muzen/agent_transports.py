from __future__ import annotations

import asyncio
import base64
import binascii
import codecs
import http.client
import json
import os
import sys
import uuid
from typing import Any, AsyncIterator, Dict, List, Mapping, Optional, Tuple
from urllib.parse import quote, urlencode, urlsplit

from .agent import (
    AgentEvent,
    AgentInputLike,
    AgentMessage,
    AgentSession,
    Artifact,
    ArtifactId,
    ArtifactRef,
    Capabilities,
    CommandReceipt,
    Muzen,
    MuzenError,
    Page,
    PutSecretInput,
    Run,
    RunLimits,
    RunResult,
    RunSnapshot,
    RunSpec,
    SendCommand,
    SessionSnapshot,
    SessionSpec,
    SpawnCommand,
    normalize_agent_input,
    to_wire,
)
from .agent_wire import (
    artifact_ref_from_wire as _artifact_ref,
    capabilities_from_wire as _capabilities,
    event_from_wire as _event,
    message_from_wire as _message,
    run_result_from_wire as _run_result,
    run_snapshot_from_wire as _run_snapshot,
    session_snapshot_from_wire as _session_snapshot,
)

_TERMINAL_EVENTS = {
    "run.completed",
    "run.partial",
    "run.failed",
    "run.cancelled",
}
_ARTIFACT_CHUNK_BYTES = 64 * 1024
_END = object()


def _unavailable(message: str) -> MuzenError:
    return MuzenError("unavailable", message, True)


def _error_from_wire(value: Any, fallback: str = "Muzen request failed") -> MuzenError:
    if isinstance(value, Mapping):
        code = value.get("code")
        message = value.get("message")
        retryable = value.get("retryable")
        if isinstance(code, str) and isinstance(message, str) and isinstance(retryable, bool):
            details = value.get("details")
            return MuzenError(code, message, retryable, details)
    return MuzenError("internal", fallback, False)


class _JsonRpcDemultiplexer:
    """Routes JSON-RPC responses and run.event notifications."""

    def __init__(self) -> None:
        self.pending: Dict[Any, asyncio.Future[Any]] = {}
        self.subscriptions: Dict[str, asyncio.Queue[Any]] = {}

    def response_future(self, request_id: Any) -> asyncio.Future[Any]:
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        return future

    def add_subscription(self, subscription_id: str) -> asyncio.Queue[Any]:
        queue: asyncio.Queue[Any] = asyncio.Queue()
        self.subscriptions[subscription_id] = queue
        return queue

    def remove_subscription(self, subscription_id: str) -> None:
        self.subscriptions.pop(subscription_id, None)

    def feed(self, message: Mapping[str, Any]) -> None:
        if "id" in message:
            future = self.pending.pop(message["id"], None)
            if future is None or future.done():
                return
            if "error" in message:
                error = message["error"]
                if isinstance(error, Mapping) and error.get("code") == -32000:
                    future.set_exception(_error_from_wire(error.get("data"), error.get("message", "request failed")))
                else:
                    text = error.get("message", "JSON-RPC request failed") if isinstance(error, Mapping) else "JSON-RPC request failed"
                    future.set_exception(MuzenError("internal", text, False))
            else:
                future.set_result(message.get("result"))
            return
        if message.get("method") == "run.event":
            params = message.get("params")
            if isinstance(params, Mapping):
                queue = self.subscriptions.get(params.get("subscriptionId"))
                if queue is not None:
                    queue.put_nowait(params.get("event"))

    def fail(self, error: MuzenError) -> None:
        pending = list(self.pending.values())
        self.pending.clear()
        for future in pending:
            if not future.done():
                future.set_exception(error)
        for queue in list(self.subscriptions.values()):
            queue.put_nowait(error)


class _RunnerTransport:
    def __init__(
        self,
        process: asyncio.subprocess.Process,
        close_timeout: float,
    ) -> None:
        self.process = process
        self.close_timeout = close_timeout
        self.demux = _JsonRpcDemultiplexer()
        self._next_id = 1
        self._write_lock = asyncio.Lock()
        self._closed = False
        self._failure: Optional[MuzenError] = None
        self._reader_task = asyncio.create_task(self._read_loop())

    async def _read_loop(self) -> None:
        error = _unavailable("local runner transport closed")
        try:
            assert self.process.stdout is not None
            while True:
                line = await self.process.stdout.readline()
                if not line:
                    break
                try:
                    message = json.loads(line.decode("utf-8"))
                    if isinstance(message, Mapping):
                        self.demux.feed(message)
                except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                    error = _unavailable("invalid JSON from local runner: %s" % exc)
                    break
        except (OSError, asyncio.CancelledError) as exc:
            if isinstance(exc, asyncio.CancelledError):
                raise
            error = _unavailable("local runner read failed: %s" % exc)
        finally:
            self._failure = error
            self.demux.fail(error)

    async def request(self, method: str, params: Optional[Mapping[str, Any]] = None) -> Any:
        if (
            self._closed
            or self._failure is not None
            or self.process.returncode is not None
            or self._reader_task.done()
        ):
            raise self._failure or _unavailable("local runner is not available")
        request_id = self._next_id
        self._next_id += 1
        future = self.demux.response_future(request_id)
        message = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": dict(params or {}),
        }
        try:
            encoded = (json.dumps(message, separators=(",", ":")) + "\n").encode("utf-8")
            async with self._write_lock:
                if self.process.stdin is None or self.process.stdin.is_closing():
                    raise _unavailable("local runner stdin is closed")
                self.process.stdin.write(encoded)
                await self.process.stdin.drain()
        except (BrokenPipeError, ConnectionError, OSError) as exc:
            self.demux.pending.pop(request_id, None)
            if not future.done():
                future.cancel()
            raise _unavailable("local runner write failed: %s" % exc)
        return await future

    async def events(self, run_id: str, after: Optional[int]) -> AsyncIterator[AgentEvent]:
        subscription_id = uuid.uuid4().hex
        queue = self.demux.add_subscription(subscription_id)
        subscribed = False
        cursor = after
        try:
            while True:
                params: Dict[str, Any] = {
                    "runId": run_id,
                    "subscriptionId": subscription_id,
                }
                if cursor is not None:
                    params["after"] = cursor
                response = await self.request("run.events", params)
                events = response["events"]
                subscribed = bool(response["subscribed"])
                for item in events:
                    event = _event(item)
                    cursor = event.sequence
                    yield event
                    if event.type in _TERMINAL_EVENTS:
                        return
                if subscribed:
                    break
                if not events:
                    return
            while True:
                item = await queue.get()
                if isinstance(item, BaseException):
                    raise item
                event = _event(item)
                yield event
                if event.type in _TERMINAL_EVENTS:
                    return
        finally:
            self.demux.remove_subscription(subscription_id)
            if subscribed and not self._closed:
                try:
                    await self.request("run.unsubscribe", {"subscriptionId": subscription_id})
                except (MuzenError, asyncio.CancelledError):
                    pass

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self.process.stdin is not None and not self.process.stdin.is_closing():
            self.process.stdin.close()
            try:
                await self.process.stdin.wait_closed()
            except (AttributeError, BrokenPipeError, ConnectionError):
                pass
        try:
            await asyncio.wait_for(self.process.wait(), timeout=self.close_timeout)
        except asyncio.TimeoutError:
            self.process.kill()
            await self.process.wait()
        if not self._reader_task.done():
            await self._reader_task


class _SSEParser:
    """Incremental UTF-8 SSE parser returning completed data payloads."""

    def __init__(self) -> None:
        self._decoder = codecs.getincrementaldecoder("utf-8")()
        self._buffer = ""
        self._data: List[str] = []

    def feed(self, chunk: bytes, final: bool = False) -> List[str]:
        self._buffer += self._decoder.decode(chunk, final=final)
        output: List[str] = []
        while True:
            newline = self._buffer.find("\n")
            if newline < 0:
                break
            line = self._buffer[:newline]
            self._buffer = self._buffer[newline + 1 :]
            if line.endswith("\r"):
                line = line[:-1]
            self._line(line, output)
        if final:
            if self._buffer:
                line = self._buffer[:-1] if self._buffer.endswith("\r") else self._buffer
                self._line(line, output)
                self._buffer = ""
            self._line("", output)
        return output

    def _line(self, line: str, output: List[str]) -> None:
        if line == "":
            if self._data:
                output.append("\n".join(self._data))
                self._data = []
            return
        if line.startswith(":"):
            return
        if ":" in line:
            field, value = line.split(":", 1)
            if value.startswith(" "):
                value = value[1:]
        else:
            field, value = line, ""
        if field == "data":
            self._data.append(value)


_STATUS_CODES = {
    400: "invalid_input",
    401: "unauthenticated",
    403: "permission_denied",
    404: "not_found",
    409: "conflict",
    429: "resource_exhausted",
    500: "internal",
    501: "unsupported",
    503: "unavailable",
    504: "deadline_exceeded",
}


class _HttpTransport:
    def __init__(self, base_url: str, bearer_token: Optional[str]) -> None:
        parsed = urlsplit(base_url)
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            raise MuzenError("invalid_input", "base_url must be an HTTP(S) URL", False)
        if parsed.query or parsed.fragment:
            raise MuzenError("invalid_input", "base_url must not contain a query or fragment", False)
        self.scheme = parsed.scheme
        self.host = parsed.hostname
        self.port = parsed.port
        self.prefix = parsed.path.rstrip("/")
        self.bearer_token = bearer_token
        self._closed = False

    def _connection(self) -> http.client.HTTPConnection:
        connection_type = http.client.HTTPSConnection if self.scheme == "https" else http.client.HTTPConnection
        return connection_type(self.host, self.port, timeout=30)

    def _headers(self, idempotency_key: Optional[str] = None) -> Dict[str, str]:
        headers = {"Accept": "application/json"}
        if self.bearer_token is not None:
            headers["Authorization"] = "Bearer " + self.bearer_token
        if idempotency_key is not None:
            headers["Idempotency-Key"] = idempotency_key
        return headers

    async def request(
        self,
        method: str,
        path: str,
        body: Any = _END,
        idempotency_key: Optional[str] = None,
        extra_headers: Optional[Mapping[str, str]] = None,
        raw: bool = False,
    ) -> Any:
        if self._closed:
            raise _unavailable("HTTP transport is closed")

        def perform() -> Any:
            connection = self._connection()
            try:
                headers = self._headers(idempotency_key)
                if extra_headers:
                    headers.update(extra_headers)
                encoded = None
                if body is not _END:
                    encoded = json.dumps(body, separators=(",", ":")).encode("utf-8")
                    headers["Content-Type"] = "application/json"
                connection.request(method, self.prefix + path, body=encoded, headers=headers)
                response = connection.getresponse()
                data = response.read()
                if response.status < 200 or response.status >= 300:
                    self._raise_http(response.status, data)
                if raw:
                    return data
                if not data:
                    return None
                return json.loads(data.decode("utf-8"))
            except MuzenError:
                raise
            except (OSError, http.client.HTTPException, UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise _unavailable("HTTP request failed: %s" % exc)
            finally:
                connection.close()

        return await asyncio.to_thread(perform)

    def _raise_http(self, status: int, data: bytes) -> None:
        try:
            value = json.loads(data.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            value = None
        if (
            isinstance(value, Mapping)
            and isinstance(value.get("code"), str)
            and isinstance(value.get("message"), str)
            and isinstance(value.get("retryable"), bool)
        ):
            raise _error_from_wire(value, "HTTP %d" % status)
        code = _STATUS_CODES.get(status, "internal")
        raise MuzenError(code, "HTTP request failed with status %d" % status, status >= 500)

    async def events(self, run_id: str, after: Optional[int]) -> AsyncIterator[AgentEvent]:
        queue: asyncio.Queue[Any] = asyncio.Queue()
        loop = asyncio.get_running_loop()
        connection_holder: List[http.client.HTTPConnection] = []
        stopped = False

        def publish(item: Any) -> None:
            if not stopped:
                asyncio.run_coroutine_threadsafe(queue.put(item), loop).result()

        def read_stream() -> None:
            connection = self._connection()
            connection_holder.append(connection)
            parser = _SSEParser()
            try:
                query = ""
                if after is not None:
                    query = "?" + urlencode({"after": after})
                headers = self._headers()
                headers["Accept"] = "text/event-stream"
                connection.request("GET", self.prefix + "/v1/runs/%s/events%s" % (quote(run_id, safe=""), query), headers=headers)
                response = connection.getresponse()
                if response.status < 200 or response.status >= 300:
                    self._raise_http(response.status, response.read())
                while True:
                    chunk = response.read1(4096)
                    if not chunk:
                        for payload in parser.feed(b"", final=True):
                            publish(json.loads(payload))
                        publish(_END)
                        return
                    for payload in parser.feed(chunk):
                        publish(json.loads(payload))
            except MuzenError as exc:
                publish(exc)
            except (OSError, http.client.HTTPException, UnicodeDecodeError, json.JSONDecodeError) as exc:
                publish(_unavailable("SSE transport failed: %s" % exc))
            finally:
                connection.close()

        task = asyncio.create_task(asyncio.to_thread(read_stream))
        try:
            while True:
                item = await queue.get()
                if item is _END:
                    raise _unavailable("SSE stream ended before a terminal run event")
                if isinstance(item, BaseException):
                    raise item
                event = _event(item)
                yield event
                if event.type in _TERMINAL_EVENTS:
                    return
        finally:
            stopped = True
            if connection_holder:
                connection_holder[0].close()
            if not task.done():
                task.cancel()
            try:
                await task
            except (asyncio.CancelledError, OSError):
                pass

    async def close(self) -> None:
        self._closed = True


class _MuzenImpl:
    def __init__(self, transport: Any) -> None:
        self._transport = transport

    async def capabilities(self) -> Capabilities:
        if isinstance(self._transport, _RunnerTransport):
            value = await self._transport.request("muzen.capabilities")
        else:
            value = await self._transport.request("GET", "/v1/capabilities")
        return _capabilities(value)

    async def put_secret(self, input: PutSecretInput) -> str:
        if isinstance(self._transport, _RunnerTransport):
            return await self._transport.request("secret.put", to_wire(input))
        return await self._transport.request(
            "POST",
            "/v1/secrets",
            to_wire(input),
            idempotency_key=input.idempotency_key,
        )

    async def delete_secret(self, secret: str) -> None:
        if isinstance(self._transport, _RunnerTransport):
            await self._transport.request("secret.delete", {"secret": secret})
        else:
            await self._transport.request("DELETE", "/v1/secrets/" + quote(secret, safe=""))

    async def create_session(
        self, spec: SessionSpec, *, idempotency_key: Optional[str] = None
    ) -> AgentSession:
        if isinstance(self._transport, _RunnerTransport):
            params: Dict[str, Any] = {"spec": to_wire(spec)}
            if idempotency_key is not None:
                params["options"] = {"idempotencyKey": idempotency_key}
            session_id = await self._transport.request("session.create", params)
        else:
            session_id = await self._transport.request(
                "POST", "/v1/sessions", to_wire(spec), idempotency_key=idempotency_key
            )
        return _AgentSessionImpl(session_id, self._transport)

    async def get_session(self, session_id: str) -> AgentSession:
        session = _AgentSessionImpl(session_id, self._transport)
        await session.snapshot()
        return session

    async def start_run(self, spec: RunSpec) -> Run:
        if isinstance(self._transport, _RunnerTransport):
            run_id = await self._transport.request("run.start", {"spec": to_wire(spec)})
        else:
            run_id = await self._transport.request(
                "POST", "/v1/runs", to_wire(spec), idempotency_key=spec.idempotency_key
            )
        return _RunImpl(run_id, self._transport)

    async def get_run(self, run_id: str) -> Run:
        run = _RunImpl(run_id, self._transport)
        await run.snapshot()
        return run

    async def close(self) -> None:
        await self._transport.close()


class _AgentSessionImpl:
    def __init__(self, session_id: str, transport: Any) -> None:
        self.id = session_id
        self._transport = transport

    async def snapshot(self) -> SessionSnapshot:
        if isinstance(self._transport, _RunnerTransport):
            value = await self._transport.request("session.get", {"sessionId": self.id})
        else:
            value = await self._transport.request("GET", "/v1/sessions/" + quote(self.id, safe=""))
        return _session_snapshot(value)

    async def messages(
        self, *, after: Optional[str] = None, limit: Optional[int] = None
    ) -> Page[AgentMessage]:
        page = {key: value for key, value in (("after", after), ("limit", limit)) if value is not None}
        if isinstance(self._transport, _RunnerTransport):
            params: Dict[str, Any] = {"sessionId": self.id}
            if page:
                params["page"] = page
            value = await self._transport.request("session.messages", params)
        else:
            suffix = "?" + urlencode(page) if page else ""
            value = await self._transport.request(
                "GET", "/v1/sessions/%s/messages%s" % (quote(self.id, safe=""), suffix)
            )
        return Page(tuple(_message(item) for item in value["items"]), value.get("next"))

    async def run(
        self,
        input: AgentInputLike,
        *,
        limits: RunLimits,
        idempotency_key: Optional[str] = None,
    ) -> Run:
        wire_input = to_wire(normalize_agent_input(input))
        if isinstance(self._transport, _RunnerTransport):
            spec = {
                "roots": [{"sessionId": self.id, "input": wire_input}],
                "limits": to_wire(limits),
            }
            if idempotency_key is not None:
                spec["idempotencyKey"] = idempotency_key
            run_id = await self._transport.request("run.start", {"spec": spec})
        else:
            options: Dict[str, Any] = {"limits": to_wire(limits)}
            if idempotency_key is not None:
                options["idempotencyKey"] = idempotency_key
            run_id = await self._transport.request(
                "POST",
                "/v1/sessions/%s/runs" % quote(self.id, safe=""),
                {"input": wire_input, "options": options},
                idempotency_key=idempotency_key,
            )
        return _RunImpl(run_id, self._transport)

    async def archive(self, *, idempotency_key: Optional[str] = None) -> None:
        options = {} if idempotency_key is None else {"idempotencyKey": idempotency_key}
        if isinstance(self._transport, _RunnerTransport):
            params: Dict[str, Any] = {"sessionId": self.id}
            if options:
                params["options"] = options
            await self._transport.request("session.archive", params)
        else:
            await self._transport.request(
                "POST",
                "/v1/sessions/%s/archive" % quote(self.id, safe=""),
                options,
                idempotency_key=idempotency_key,
            )


class _RunImpl:
    def __init__(self, run_id: str, transport: Any) -> None:
        self.id = run_id
        self._transport = transport

    async def snapshot(self) -> RunSnapshot:
        if isinstance(self._transport, _RunnerTransport):
            value = await self._transport.request("run.get", {"runId": self.id})
        else:
            value = await self._transport.request("GET", "/v1/runs/" + quote(self.id, safe=""))
        return _run_snapshot(value)

    def events(self, *, after: Optional[int] = None) -> AsyncIterator[AgentEvent]:
        return self._transport.events(self.id, after)

    async def wait(self) -> RunResult:
        result = await self.result()
        if result is not None:
            return result
        async for event in self.events():
            if event.type in _TERMINAL_EVENTS:
                break
        result = await self.result()
        if result is None:
            raise MuzenError("internal", "run event stream ended without a durable result", False)
        return result

    async def result(self) -> Optional[RunResult]:
        if isinstance(self._transport, _RunnerTransport):
            value = await self._transport.request("run.result", {"runId": self.id})
        else:
            value = await self._transport.request(
                "GET", "/v1/runs/%s/result" % quote(self.id, safe="")
            )
        return None if value is None else _run_result(value)

    async def send(self, command: SendCommand) -> CommandReceipt:
        if isinstance(self._transport, _RunnerTransport):
            value = await self._transport.request(
                "run.send", {"runId": self.id, "command": to_wire(command)}
            )
        else:
            value = await self._transport.request(
                "POST",
                "/v1/runs/%s/send" % quote(self.id, safe=""),
                to_wire(command),
                idempotency_key=command.idempotency_key,
            )
        return CommandReceipt(value["sequence"])

    async def spawn(self, command: SpawnCommand) -> AgentSession:
        if isinstance(self._transport, _RunnerTransport):
            session_id = await self._transport.request(
                "run.spawn", {"runId": self.id, "command": to_wire(command)}
            )
        else:
            session_id = await self._transport.request(
                "POST",
                "/v1/runs/%s/spawn" % quote(self.id, safe=""),
                to_wire(command),
                idempotency_key=command.idempotency_key,
            )
        return _AgentSessionImpl(session_id, self._transport)

    async def cancel(
        self,
        *,
        reason: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> CommandReceipt:
        options = {key: value for key, value in (("reason", reason), ("idempotencyKey", idempotency_key)) if value is not None}
        if isinstance(self._transport, _RunnerTransport):
            params: Dict[str, Any] = {"runId": self.id}
            if options:
                params["options"] = options
            value = await self._transport.request("run.cancel", params)
        else:
            value = await self._transport.request(
                "POST",
                "/v1/runs/%s/cancel" % quote(self.id, safe=""),
                options,
                idempotency_key=idempotency_key,
            )
        return CommandReceipt(value["sequence"])

    async def artifact(self, artifact_id: ArtifactId) -> Artifact:
        result = await self.result()
        reference = None if result is None else next(
            (item for item in result.artifacts if item.id == artifact_id), None
        )
        if reference is None:
            raise MuzenError("not_found", "artifact not found: %s" % artifact_id, False)
        return _ArtifactImpl(reference, self.id, self._transport)


class _ArtifactImpl:
    def __init__(self, reference: ArtifactRef, run_id: str, transport: Any) -> None:
        self.ref = reference
        self._run_id = run_id
        self._transport = transport

    async def data(self) -> AsyncIterator[bytes]:
        offset = 0
        while True:
            if isinstance(self._transport, _RunnerTransport):
                value = await self._transport.request(
                    "artifact.read",
                    {
                        "artifactId": self.ref.id,
                        "offset": offset,
                        "maxBytes": _ARTIFACT_CHUNK_BYTES,
                    },
                )
                try:
                    chunk = base64.b64decode(value["data"], validate=True)
                except (binascii.Error, ValueError) as exc:
                    raise MuzenError(
                        "internal", "artifact chunk contains invalid base64: %s" % exc, False
                    )
                eof = value["eof"]
            else:
                end = offset + _ARTIFACT_CHUNK_BYTES - 1
                chunk = await self._transport.request(
                    "GET",
                    "/v1/runs/%s/artifacts/%s"
                    % (quote(self._run_id, safe=""), quote(self.ref.id, safe="")),
                    extra_headers={"Range": "bytes=%d-%d" % (offset, end)},
                    raw=True,
                )
                eof = offset + len(chunk) >= self.ref.bytes
            if not chunk and not eof:
                raise MuzenError("internal", "artifact transport returned an empty non-terminal chunk", False)
            if chunk:
                yield chunk
                offset += len(chunk)
            if eof:
                return


async def connect_local_runner(
    *,
    store: str = "memory",
    sqlite_path: Optional[str] = None,
    allow_loopback_http: bool = False,
    binary_path: Optional[str] = None,
    close_timeout: float = 5.0,
) -> Muzen:
    if store not in ("memory", "sqlite"):
        raise MuzenError("invalid_input", "store must be 'memory' or 'sqlite'", False)
    if store == "sqlite" and not sqlite_path:
        raise MuzenError("invalid_input", "sqlite_path is required for sqlite store", False)
    if store == "memory" and sqlite_path is not None:
        raise MuzenError("invalid_input", "sqlite_path requires sqlite store", False)
    if close_timeout <= 0:
        raise MuzenError("invalid_input", "close_timeout must be positive", False)
    executable = binary_path or os.environ.get("MUZEN_AGENT_RUNNER_BIN") or "muzen-agent-runner"
    args = [executable, "--store", store]
    if sqlite_path is not None:
        args.extend(["--db", sqlite_path])
    if allow_loopback_http:
        args.append("--allow-loopback-http")
    try:
        process = await asyncio.create_subprocess_exec(
            *args,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=sys.stderr,
        )
    except OSError as exc:
        raise _unavailable("failed to start local runner: %s" % exc)
    return _MuzenImpl(_RunnerTransport(process, close_timeout))


async def connect_http(base_url: str, bearer_token: Optional[str] = None) -> Muzen:
    return _MuzenImpl(_HttpTransport(base_url, bearer_token))


async def connect(options: Optional[Mapping[str, Any]] = None) -> Muzen:
    values = dict(options or {})
    transport = values.pop("transport", "local_runner")
    if transport == "local_runner":
        return await connect_local_runner(**values)
    if transport == "http":
        base_url = values.pop("base_url", None)
        if base_url is None:
            raise MuzenError("invalid_input", "base_url is required for HTTP transport", False)
        return await connect_http(base_url, **values)
    raise MuzenError("invalid_input", "transport must be 'local_runner' or 'http'", False)
