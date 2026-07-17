import asyncio
import json
import os
import socket
import subprocess
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import List, Optional, TypedDict

import pytest

from muzen import Agent, tool
from muzen.agent import (
    AgentEvent,
    AgentOutput,
    AgentResult,
    ExecutionError,
    MuzenError,
    Page,
    RunResult,
    TextBlock,
    Usage,
    discover_local_runner_binary,
)


_USAGE = Usage(input_tokens=2, output_tokens=1, tool_calls=0)


class _FakeRun:
    def __init__(self, session_id, output="done", status="completed", error=None):
        self.id = "run-1"
        self._result = RunResult(
            run_id=self.id,
            status="completed" if status == "completed" else "failed",
            outputs=(
                AgentOutput(
                    session_id=session_id,
                    path=(),
                    status=status,
                    usage=_USAGE,
                    output=output,
                    error=error,
                ),
            ),
            usage=_USAGE,
            artifacts=(),
            metadata={},
        )

    async def wait(self):
        return self._result


class _FakeSession:
    def __init__(self, spec, number):
        self.id = "session-%d" % number
        self.spec = spec
        self.runs = []
        self.archived = False

    async def run(self, prompt, *, limits, idempotency_key=None):
        self.runs.append((prompt, limits))
        return _FakeRun(self.id)

    async def archive(self, *, idempotency_key=None):
        self.archived = True

    async def messages(self, *, after=None, limit=None):
        return Page(())


class _FakeMuzen:
    def __init__(self):
        self.secrets = []
        self.sessions = []
        self.closed = False

    async def put_secret(self, value):
        self.secrets.append(value)
        return "secret-%d" % len(self.secrets)

    async def create_session(self, spec, *, idempotency_key=None):
        session = _FakeSession(spec, len(self.sessions) + 1)
        self.sessions.append(session)
        return session

    async def close(self):
        self.closed = True


def _run_and_spec(**kwargs):
    fake = _FakeMuzen()

    async def exercise():
        agent = Agent(client=fake, instructions="do it", api_key="test", **kwargs)
        result = await agent.run("hello")
        await agent.close()
        return result, fake.sessions[0].spec

    return asyncio.run(exercise())


@pytest.mark.parametrize(
    "model,provider,protocol,name",
    [
        ("claude-sonnet-5", "anthropic", "messages", "claude-sonnet-5"),
        ("gpt-4o-mini", "openai_compatible", "chat_completions", "gpt-4o-mini"),
        ("anthropic:not-claude", "anthropic", "messages", "not-claude"),
        ("openai:claude-named", "openai_compatible", "chat_completions", "claude-named"),
    ],
)
def test_model_string_synthesis(model, provider, protocol, name):
    _, spec = _run_and_spec(model=model)
    profile = spec.models[0]
    assert (profile.provider, profile.protocol, profile.model) == (provider, protocol, name)
    assert profile.credential == "secret-1"


def test_missing_provider_environment_key_is_invalid_input(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    with pytest.raises(MuzenError) as caught:
        Agent(instructions="do it", model="claude-test")
    assert caught.value.code == "invalid_input"
    assert "ANTHROPIC_API_KEY" in str(caught.value)


class _Child(TypedDict):
    enabled: bool


class _TypedOutput(TypedDict):
    title: str
    children: List[_Child]
    score: Optional[float]


@dataclass
class _DataclassOutput:
    count: int
    child: _Child
    note: Optional[str] = None


def test_typed_dict_output_schema_supports_nested_list_and_optional():
    _, spec = _run_and_spec(model="gpt-test", output=_TypedOutput)
    assert spec.agent.output.schema == {
        "type": "object",
        "properties": {
            "title": {"type": "string"},
            "children": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"enabled": {"type": "boolean"}},
                    "additionalProperties": False,
                    "required": ["enabled"],
                },
            },
            "score": {"anyOf": [{"type": "number"}, {"type": "null"}]},
        },
        "additionalProperties": False,
        "required": ["title", "children", "score"],
    }


def test_dataclass_output_schema_uses_defaults_for_required_fields():
    _, spec = _run_and_spec(model="gpt-test", output=_DataclassOutput)
    schema = spec.agent.output.schema
    assert schema["required"] == ["count", "child"]
    assert schema["properties"]["count"] == {"type": "integer"}
    assert schema["properties"]["child"]["properties"] == {
        "enabled": {"type": "boolean"}
    }
    assert schema["properties"]["note"] == {
        "anyOf": [{"type": "string"}, {"type": "null"}]
    }


def test_instructions_swarm_grants_defaults_and_lazy_secret():
    fake = _FakeMuzen()

    async def exercise():
        agent = Agent(
            client=fake,
            instructions="review carefully",
            model="gpt-test",
            api_key="test",
            can_spawn=True,
            can_message=True,
        )
        assert fake.secrets == []
        await agent.run("one")
        await agent.run("two")
        spec = fake.sessions[0].spec
        assert spec.agent.instructions == (TextBlock("review carefully"),)
        assert [(grant.tool, grant.effects) for grant in spec.agent.tools] == [
            ("agent.spawn", ("agent_spawn",)),
            ("agent.message", ("agent_message",)),
        ]
        assert [(provider.id, provider.kind) for provider in spec.tool_providers] == [
            ("builtin", "builtin")
        ]
        limits = fake.sessions[0].runs[0][1]
        assert (
            limits.max_active_agents,
            limits.max_agents,
            limits.max_depth,
            limits.max_input_bytes,
            limits.max_total_tokens,
            limits.deadline_ms,
        ) == (4, 16, 3, 1_048_576, None, None)
        assert len(fake.secrets) == 1
        assert all(session.archived for session in fake.sessions)
        await agent.close()

    asyncio.run(exercise())


def test_builtin_grant_rejects_model_visible_tool_name_collision():
    @tool
    def agent_spawn(query: str) -> str:
        return query

    with pytest.raises(MuzenError, match="unique function names") as caught:
        Agent(
            instructions="do it",
            model="gpt-test",
            api_key="test",
            can_spawn=True,
            tools=[agent_spawn],
        )
    assert caught.value.code == "invalid_input"

    Agent(
        instructions="do it",
        model="gpt-test",
        api_key="test",
        tools=[agent_spawn],
    )


def test_http_transport_constructs_client_tools_with_real_signatures():
    @tool
    def lookup(query: str) -> str:
        """Look up an issue."""
        return query

    fake = _FakeMuzen()
    agent = Agent(
        client=fake,
        instructions="do it",
        model="gpt-test",
        api_key="test",
        tools=[lookup],
        transport="http",
    )
    grant = agent._spec_template.agent.tools[0]
    assert grant.provider == "local_tools"
    assert grant.description == "Look up an issue."
    assert grant.input_schema == {
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "additionalProperties": False,
        "required": ["query"],
    }
    assert [
        (provider.id, provider.kind)
        for provider in agent._spec_template.tool_providers
    ] == [("local_tools", "client")]
    assert agent._tool_server is None
    asyncio.run(agent.close())


class _PumpClient:
    def __init__(self, error_code=None):
        self.answers = []
        self.error_code = error_code

    async def answer_tool_call(self, run_id, input):
        self.answers.append((run_id, input))
        if self.error_code is not None:
            raise MuzenError(self.error_code, "answer raced", False)

    async def close(self):
        pass


class _PumpRun:
    def __init__(self, tool_name, arguments):
        self.id = "run-tools"
        self.tool_name = tool_name
        self.arguments = arguments
        self.after = []

    async def events(self, *, after=None):
        self.after.append(after)
        yield AgentEvent(
            run_id=self.id,
            sequence=1,
            type="tool.requested",
            timestamp="2026-07-16T00:00:00Z",
            payload={
                "callId": "call-1",
                "provider": "local_tools",
                "tool": self.tool_name,
                "arguments": self.arguments,
                "timeoutMs": 120000,
            },
        )
        yield AgentEvent(
            run_id=self.id,
            sequence=2,
            type="run.completed",
            timestamp="2026-07-16T00:00:01Z",
            payload={},
        )


def _pump_outcome(handler, tool_name, arguments, error_code=None):
    client = _PumpClient(error_code)
    agent = Agent(
        client=client,
        instructions="use tools",
        model="gpt-test",
        api_key="test",
        tools=[handler],
        transport="http",
    )

    async def exercise():
        run = _PumpRun(tool_name, arguments)
        await agent._pump_client_tools(run)
        await agent.close()
        assert run.after == [None]

    asyncio.run(exercise())
    assert len(client.answers) == 1
    return client.answers[0][1].outcome


def test_http_tool_pump_posts_tool_errors_and_unknown_tools():
    @tool
    def explode(query: str) -> str:
        raise RuntimeError("handler exploded")

    assert _pump_outcome(explode, "explode", {"query": "x"}) == {
        "error": {"message": "handler exploded", "retryable": False}
    }
    unknown = _pump_outcome(explode, "missing_tool", {"query": "x"})
    assert unknown["error"]["retryable"] is False
    assert "missing_tool" in unknown["error"]["message"]


@pytest.mark.parametrize("code", ["conflict", "not_found"])
def test_http_tool_pump_swallows_benign_answer_races(code):
    @tool
    def lookup(query: str) -> str:
        return "found " + query

    assert _pump_outcome(lookup, "lookup", {"query": "x"}, code) == {
        "result": "found x"
    }


def test_http_tool_pump_replays_request_when_answer_transport_drops():
    @tool
    def lookup(query: str) -> str:
        return "found " + query

    class ResumeClient(_PumpClient):
        async def answer_tool_call(self, run_id, input):
            self.answers.append((run_id, input))
            if len(self.answers) == 1:
                raise MuzenError("unavailable", "connection dropped", True)

    client = ResumeClient()
    agent = Agent(
        client=client,
        instructions="use tools",
        model="gpt-test",
        api_key="test",
        tools=[lookup],
        transport="http",
    )

    async def exercise():
        run = _PumpRun("lookup", {"query": "x"})
        await agent._pump_client_tools(run)
        await agent.close()
        assert run.after == [None, None]

    asyncio.run(exercise())
    assert [answer.outcome for _, answer in client.answers] == [
        {"result": "found x"},
        {"result": "found x"},
    ]


def test_session_reuses_one_session_and_one_shot_does_not():
    fake = _FakeMuzen()

    async def exercise():
        agent = Agent(client=fake, instructions="do it", model="gpt-test", api_key="test")
        async with agent.session() as chat:
            first = await chat.run("first")
            second = await chat.run("follow-up")
            assert first.text == second.output == "done"
        assert len(fake.sessions) == 1
        assert [prompt for prompt, _ in fake.sessions[0].runs] == ["first", "follow-up"]
        assert fake.sessions[0].archived
        await agent.run("fresh")
        assert len(fake.sessions) == 2
        await agent.close()
        assert fake.closed

    asyncio.run(exercise())


def test_non_completed_result_is_returned_and_raise_for_status_is_opt_in():
    error = ExecutionError(
        code="model_error", message="provider failed", retryable=True
    )
    raw = _FakeRun("session-1", status="failed", error=error)._result
    result = AgentResult(
        text="null",
        output="null",
        usage=_USAGE,
        status="failed",
        run_id=raw.run_id,
        raw=raw,
    )
    with pytest.raises(MuzenError, match="provider failed") as caught:
        result.raise_for_status()
    assert caught.value.code == "internal"
    assert caught.value.retryable


class _ModelHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        json.loads(self.rfile.read(length).decode("utf-8"))
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


class _ToolModelHandler(BaseHTTPRequestHandler):
    requests = []

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(length).decode("utf-8"))
        self.requests.append(request)
        has_result = any(
            block.get("type") == "tool_result"
            for message in request.get("messages", [])
            for block in message.get("content", [])
            if isinstance(block, dict)
        )
        if has_result:
            response = {
                "content": [{"type": "text", "text": "tool completed"}],
                "usage": {"input_tokens": 2, "output_tokens": 1},
                "stop_reason": "end_turn",
            }
        else:
            response = {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "search-1",
                        "name": "search",
                        "input": {"query": "retry policy", "limit": 3},
                    }
                ],
                "usage": {"input_tokens": 2, "output_tokens": 1},
                "stop_reason": "tool_use",
            }
        body = json.dumps(response).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


def test_facade_local_runner_one_shot_continuity_and_close():
    binary = discover_local_runner_binary()
    if binary is None or not os.path.isfile(binary):
        pytest.skip("muzen-agent-runner is not built")
    server = ThreadingHTTPServer(("127.0.0.1", 0), _ModelHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        async def exercise():
            agent = Agent(
                instructions="Answer the user.",
                model="claude-test",
                base_url="http://127.0.0.1:%d" % server.server_port,
                api_key="test",
            )
            result = await agent.run("hello")
            assert result.text == "done"
            async with agent.session() as chat:
                await chat.run("first")
                await chat.run("follow-up")
                messages = await chat._session.messages()
                assert [message.role for message in messages.items] == [
                    "user", "assistant", "user", "assistant"
                ]
            process = agent._client._transport.process
            await agent.close()
            assert process.returncode is not None

        asyncio.run(exercise())
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


def test_facade_local_runner_executes_python_tool_round_trip():
    binary = discover_local_runner_binary()
    if binary is None or not os.path.isfile(binary):
        pytest.skip("muzen-agent-runner is not built")
    runtime_sources = [
        Path(__file__).resolve().parents[3] / "src/agent_runtime/local/mcp.rs",
        Path(__file__).resolve().parents[3] / "src/agent_runtime/types.rs",
    ]
    if any(source.stat().st_mtime > os.path.getmtime(binary) for source in runtime_sources):
        pytest.skip("built muzen-agent-runner predates MCP HTTP tool support")
    calls = []

    @tool
    def search(query: str, limit: int = 5) -> str:
        """Search the product docs."""
        calls.append((query, limit))
        return "retry three times"

    _ToolModelHandler.requests = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), _ToolModelHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        async def exercise():
            agent = Agent(
                instructions="Answer using tools.",
                model="claude-test",
                tools=[search],
                base_url="http://127.0.0.1:%d" % server.server_port,
                api_key="test",
            )
            result = await agent.run("find the retry policy")
            await agent.close()
            return result

        result = asyncio.run(exercise())
        assert result.text == "tool completed"
        assert calls == [("retry policy", 3)]
        assert len(_ToolModelHandler.requests) == 2
        assert _ToolModelHandler.requests[0]["tools"] == [
            {
                "name": "search",
                "description": "Search the product docs.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "integer"},
                    },
                    "additionalProperties": False,
                    "required": ["query"],
                },
            }
        ]
        second_blocks = [
            block
            for message in _ToolModelHandler.requests[1]["messages"]
            for block in message["content"]
            if isinstance(block, dict)
        ]
        assert any(
            block.get("type") == "tool_result"
            and block.get("tool_use_id") == "search-1"
            and "retry three times" in json.dumps(block)
            for block in second_blocks
        )
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class _HttpToolModelHandler(BaseHTTPRequestHandler):
    requests = []

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(length).decode("utf-8"))
        self.requests.append(request)
        result_blocks = [
            block
            for message in request.get("messages", [])
            for block in message.get("content", [])
            if isinstance(block, dict) and block.get("type") == "tool_result"
        ]
        if result_blocks:
            response = {
                "content": [
                    {
                        "type": "text",
                        "text": "model saw tool result: %s"
                        % result_blocks[-1].get("content"),
                    }
                ],
                "usage": {"input_tokens": 2, "output_tokens": 1},
                "stop_reason": "end_turn",
            }
        else:
            response = {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "http-search-1",
                        "name": "search",
                        "input": {"query": "http retry policy", "limit": 2},
                    }
                ],
                "usage": {"input_tokens": 2, "output_tokens": 1},
                "stop_reason": "tool_use",
            }
        body = json.dumps(response).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


def _agent_service_binary():
    configured = os.environ.get("MUZEN_AGENT_SERVICE_BIN")
    candidates = [Path(configured)] if configured else []
    repo = Path(__file__).resolve().parents[3]
    candidates.extend(
        [
            repo / "target" / "release" / "muzen-agent-service",
            repo / "target" / "debug" / "muzen-agent-service",
        ]
    )
    for candidate in candidates:
        if candidate.is_file():
            return str(candidate)
    pytest.skip(
        "muzen-agent-service is missing; set MUZEN_AGENT_SERVICE_BIN or build "
        "target/release or target/debug"
    )


def _free_port():
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
    listener.close()
    return port


def _wait_for_service(port, process):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(
                "muzen-agent-service exited with %s" % process.returncode
            )
        try:
            connection = socket.create_connection(("127.0.0.1", port), timeout=0.1)
            connection.close()
            return
        except OSError:
            time.sleep(0.02)
    raise AssertionError("muzen-agent-service did not listen before timeout")


def test_facade_http_executes_python_tool_through_real_service():
    service = _agent_service_binary()
    port = _free_port()
    process = subprocess.Popen(
        [
            service,
            "--listen",
            "127.0.0.1:%d" % port,
            "--store",
            "memory",
            "--allow-loopback-http",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    server = ThreadingHTTPServer(("127.0.0.1", 0), _HttpToolModelHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    calls = []

    @tool
    def search(query: str, limit: int = 5) -> dict:
        """Search the product docs."""
        calls.append((query, limit))
        return {"policy": "retry exactly twice over HTTP"}

    try:
        _wait_for_service(port, process)
        _HttpToolModelHandler.requests = []

        async def exercise():
            agent = Agent(
                instructions="Answer using tools.",
                model="claude-test",
                tools=[search],
                transport="http",
                base_url="http://127.0.0.1:%d" % port,
                model_base_url="http://127.0.0.1:%d" % server.server_port,
                api_key="test",
            )
            try:
                return await agent.run("find the HTTP retry policy")
            finally:
                await agent.close()

        result = asyncio.run(exercise())
        assert calls == [("http retry policy", 2)]
        assert "retry exactly twice over HTTP" in result.text
        assert len(_HttpToolModelHandler.requests) == 2
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)
        if process.stderr is not None:
            process.stderr.close()
