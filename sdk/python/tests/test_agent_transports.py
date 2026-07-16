import asyncio
import json
import os
import socket
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from muzen.agent import (
    AgentDefinition,
    ModelProfile,
    MuzenError,
    PathWorkspaceBase,
    PutSecretInput,
    RunLimits,
    SendCommand,
    SessionSpec,
    TextBlock,
    WorkspaceSpec,
    connect_http,
    connect_local_runner,
    normalize_agent_input,
)
from muzen.agent_transports import _JsonRpcDemultiplexer, _SSEParser


def _binary(env_name, name):
    configured = os.environ.get(env_name)
    path = Path(configured) if configured else Path(__file__).resolve().parents[3] / "target" / "debug" / name
    if not path.is_file():
        pytest.skip("%s is missing; set %s or build target/debug/%s" % (name, env_name, name))
    return str(path)


class _ModelHandler(BaseHTTPRequestHandler):
    gate = None
    requested = None

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(length).decode("utf-8"))
        if self.requested is not None:
            self.requested.set()
        messages = request.get("messages", [])
        should_wait = any("block" in str(item.get("content", "")) for item in messages)
        if should_wait and self.gate is not None:
            self.gate.wait(timeout=10)
        body = json.dumps(
            {
                "content": [{"type": "text", "text": "done"}],
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


class _ModelServer:
    def __init__(self):
        handler = type("ModelHandler", (_ModelHandler,), {})
        handler.gate = threading.Event()
        handler.requested = threading.Event()
        self.handler = handler
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def base_url(self):
        return "http://127.0.0.1:%d" % self.server.server_port

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, exc_type, exc, traceback):
        self.handler.gate.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def _session_spec(secret, model_url):
    return SessionSpec(
        agent=AgentDefinition(
            name="test-agent",
            instructions=(TextBlock("Answer the user."),),
            model="primary",
            tools=(),
        ),
        models=(
            ModelProfile(
                id="primary",
                provider="anthropic",
                protocol="messages",
                model="test-model",
                credential=secret,
                max_input_tokens=4096,
                max_output_tokens=256,
                base_url=model_url,
            ),
        ),
        tool_providers=(),
        workspace=WorkspaceSpec(PathWorkspaceBase("/tmp")),
    )


def _limits():
    return RunLimits(
        max_active_agents=1,
        max_agents=1,
        max_depth=0,
        max_input_bytes=64 * 1024,
        deadline_ms=10_000,
    )


async def _wait_until(predicate, timeout=5.0):
    deadline = asyncio.get_running_loop().time() + timeout
    while not await predicate():
        if asyncio.get_running_loop().time() >= deadline:
            raise AssertionError("condition was not reached before timeout")
        await asyncio.sleep(0.01)


def test_sse_parser_handles_split_utf8_crlf_comments_and_multiline_data():
    wire = (
        b": keepalive\r\n"
        b"id: 7\r\n"
        b"event: run.event\r\n"
        b"data: {\"text\":\"caf\xc3\xa9\",\r\n"
        b"data: \"sequence\":7}\r\n\r\n"
    )
    parser = _SSEParser()
    output = []
    for split in (1, 13, 43, 61, len(wire)):
        start = 0 if not output and split == 1 else previous
        output.extend(parser.feed(wire[start:split]))
        previous = split
    output.extend(parser.feed(b"", final=True))
    assert output == ['{"text":"caf\u00e9",\n"sequence":7}']


def test_json_rpc_demultiplexer_handles_interleaved_notifications():
    async def exercise():
        demux = _JsonRpcDemultiplexer()
        first = demux.response_future(1)
        second = demux.response_future(2)
        queue = demux.add_subscription("sub")
        demux.feed(
            {
                "jsonrpc": "2.0",
                "method": "run.event",
                "params": {"subscriptionId": "sub", "event": {"sequence": 1}},
            }
        )
        demux.feed({"jsonrpc": "2.0", "id": 2, "result": "second"})
        demux.feed({"jsonrpc": "2.0", "id": 1, "result": "first"})
        assert await first == "first"
        assert await second == "second"
        assert await queue.get() == {"sequence": 1}

    asyncio.run(exercise())


def test_local_runner_full_lifecycle_replay_unsubscribe_and_errors():
    runner = _binary("MUZEN_AGENT_RUNNER_BIN", "muzen-agent-runner")

    async def exercise(model):
        muzen = await connect_local_runner(
            binary_path=runner,
            store="memory",
            allow_loopback_http=True,
        )
        try:
            secret_input = PutSecretInput("dGVzdC1rZXk=", idempotency_key="secret-replay")
            secret = await muzen.put_secret(secret_input)
            assert await muzen.put_secret(secret_input) == secret
            session = await muzen.create_session(_session_spec(secret, model.base_url))

            run = await session.run("hello", limits=_limits())
            events = [event async for event in run.events()]
            assert [event.sequence for event in events] == list(range(1, len(events) + 1))
            assert events[-1].type == "run.completed"
            result = await run.wait()
            assert result.status == "completed"
            assert result.outputs[0].output == "done"
            messages = await session.messages()
            assert [message.role for message in messages.items] == ["user", "assistant"]
            assert messages.items[1].content == (TextBlock("done"),)

            with pytest.raises(MuzenError) as conflict:
                await run.send(
                    SendCommand(
                        session_id=session.id,
                        input=normalize_agent_input("too late"),
                        delivery="follow_up",
                    )
                )
            assert conflict.value.code == "conflict"
            with pytest.raises(MuzenError) as missing:
                await muzen.get_run("run_does_not_exist")
            assert missing.value.code == "not_found"

            blocked = await session.run("block", limits=_limits())
            assert await asyncio.to_thread(model.handler.requested.wait, 5)

            async def has_started():
                return (await blocked.snapshot()).last_sequence >= 3

            await _wait_until(has_started)
            iterator = blocked.events(after=2)
            first = await iterator.__anext__()
            assert first.sequence == 3
            await iterator.aclose()
            model.handler.gate.set()
            full_replay = [event async for event in blocked.events()]
            assert [event.sequence for event in full_replay] == list(
                range(1, len(full_replay) + 1)
            )
            assert full_replay[-1].type == "run.completed"
        finally:
            model.handler.gate.set()
            await muzen.close()

    with _ModelServer() as model:
        asyncio.run(exercise(model))


def _unused_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def _wait_for_port(port, process):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError("muzen-agent-service exited with %s" % process.returncode)
        try:
            connection = socket.create_connection(("127.0.0.1", port), timeout=0.1)
            connection.close()
            return
        except OSError:
            time.sleep(0.02)
    raise AssertionError("muzen-agent-service did not listen before timeout")


def test_http_service_auth_sse_wait_and_idempotency():
    service = _binary("MUZEN_AGENT_SERVICE_BIN", "muzen-agent-service")
    port = _unused_port()
    process = subprocess.Popen(
        [
            service,
            "--listen",
            "127.0.0.1:%d" % port,
            "--store",
            "memory",
            "--allow-loopback-http",
            "--bearer-token",
            "test-token",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        _wait_for_port(port, process)

        async def exercise(model):
            base_url = "http://127.0.0.1:%d" % port
            unauthenticated = await connect_http(base_url)
            try:
                with pytest.raises(MuzenError) as denied:
                    await unauthenticated.capabilities()
                assert denied.value.code == "unauthenticated"
            finally:
                await unauthenticated.close()

            muzen = await connect_http(base_url, bearer_token="test-token")
            try:
                secret = await muzen.put_secret(PutSecretInput("dGVzdC1rZXk="))
                spec = _session_spec(secret, model.base_url)
                first = await muzen.create_session(spec, idempotency_key="session-replay")
                replay = await muzen.create_session(spec, idempotency_key="session-replay")
                assert replay.id == first.id
                run = await first.run("hello", limits=_limits())
                result = await run.wait()
                assert result.status == "completed"
                assert result.outputs[0].output == "done"
            finally:
                await muzen.close()

        with _ModelServer() as model:
            asyncio.run(exercise(model))
    finally:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)
        if process.stderr is not None:
            process.stderr.close()
