import asyncio
import json
from urllib.request import Request, urlopen

import pytest

from muzen import Agent, tool
from muzen.agent import MuzenError
from muzen.tools import LoopbackToolServer, Tool


def _post(url, payload):
    request = Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
    )
    with urlopen(request) as response:
        body = response.read()
        return response.status, response.headers, json.loads(body) if body else None


def test_tool_decorator_builds_schema_description_and_async_wrapper():
    @tool
    async def search(query: str, limit: int = 5) -> str:
        """Search the product docs.

        Additional detail is not part of the MCP description.
        """
        return query * limit

    assert isinstance(search, Tool)
    assert search.name == "search"
    assert search.description == "Search the product docs."
    assert search.input_schema == {
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "limit": {"type": "integer"},
        },
        "additionalProperties": False,
        "required": ["query"],
    }
    assert search.invoke({"query": "x", "limit": 2}) == "xx"


def test_tool_rejects_invalid_name_and_unsupported_signatures_at_decoration_time():
    def invalid_name(value: str) -> str:
        return value

    invalid_name.__name__ = "not valid"
    with pytest.raises(MuzenError, match="tool.name") as caught:
        tool(invalid_name)
    assert caught.value.code == "invalid_input"

    with pytest.raises(MuzenError, match="unsupported annotation"):
        @tool
        def unsupported(values: set) -> str:
            return str(values)

    with pytest.raises(MuzenError, match="variadic parameters"):
        @tool
        def variadic(*values: str) -> str:
            return "".join(values)


def test_agent_auto_wraps_bare_function_and_composes_grants():
    def lookup(query: str) -> str:
        return query

    class FakeClient:
        closed = False

        async def close(self):
            self.closed = True

    fake = FakeClient()
    agent = Agent(
        client=fake,
        instructions="use tools",
        model="gpt-test",
        api_key="test",
        tools=[lookup],
        can_spawn=True,
        can_message=True,
    )

    async def exercise():
        await agent._connection()
        assert [(grant.provider, grant.tool) for grant in agent._spec_template.agent.tools] == [
            ("builtin", "agent.spawn"),
            ("builtin", "agent.message"),
            ("local_tools", "lookup"),
        ]
        assert [provider.kind for provider in agent._spec_template.tool_providers] == [
            "builtin",
            "mcp_http",
        ]
        await agent.close()

    asyncio.run(exercise())
    assert fake.closed


def test_function_tools_enable_runner_loopback_http(monkeypatch):
    def lookup(query: str) -> str:
        return query

    captured = {}

    class FakeClient:
        async def close(self):
            pass

    async def fake_connect_local_runner(**kwargs):
        captured.update(kwargs)
        return FakeClient()

    monkeypatch.setattr(
        "muzen.agent_facade.connect_local_runner", fake_connect_local_runner
    )
    agent = Agent(
        instructions="use tools",
        model="gpt-test",
        api_key="test",
        tools=[lookup],
    )

    async def exercise():
        await agent._connection()
        await agent.close()

    asyncio.run(exercise())
    assert captured["allow_loopback_http"] is True


def test_loopback_server_lists_calls_and_reports_structured_and_error_results():
    @tool
    def details(query: str) -> dict:
        return {"query": query, "count": 1}

    @tool
    def fail(reason: str) -> str:
        raise RuntimeError("failed: %s" % reason)

    server = LoopbackToolServer((details, fail))
    server.start()
    try:
        status, headers, initialized = _post(
            server.url,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {},
            },
        )
        assert status == 200
        assert headers["Mcp-Session-Id"] == "muzen-python-tools"
        assert initialized["result"]["protocolVersion"] == "2025-03-26"

        status, _, empty = _post(
            server.url,
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            },
        )
        assert status == 202
        assert empty is None

        _, _, listed = _post(
            server.url,
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
        )
        assert [item["name"] for item in listed["result"]["tools"]] == [
            "details",
            "fail",
        ]

        _, _, called = _post(
            server.url,
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "details", "arguments": {"query": "retry"}},
            },
        )
        assert called["result"] == {
            "content": [
                {"type": "text", "text": '{"query": "retry", "count": 1}'}
            ],
            "structuredContent": {"query": "retry", "count": 1},
            "isError": False,
        }

        _, _, failed = _post(
            server.url,
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "fail", "arguments": {"reason": "boom"}},
            },
        )
        assert failed["result"]["isError"] is True
        assert failed["result"]["content"] == [
            {"type": "text", "text": "failed: boom"}
        ]
    finally:
        server.close()
