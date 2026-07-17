import assert from "node:assert/strict";
import test from "node:test";

import { MuzenError, tool } from "./agent.js";
import { LoopbackToolServer } from "./tools.js";

const details = tool<{ query: string }>({
  name: "details",
  description: "Look up details.",
  input: {
    type: "object",
    properties: { query: { type: "string" } },
    required: ["query"],
    additionalProperties: false,
  },
  execute: ({ query }) => ({ query, count: 1 }),
});

test("tool object and function forms preserve typed execution metadata", async () => {
  async function search({ query, limit }: { query: string; limit?: number }): Promise<string> {
    return query.repeat(limit ?? 5);
  }
  const wrapped = tool<{ query: string; limit?: number }>(search, {
    description: "Search the product docs.",
    input: {
      type: "object",
      properties: { query: { type: "string" }, limit: { type: "integer" } },
      required: ["query"],
      additionalProperties: false,
    },
  });

  assert.equal(wrapped.name, "search");
  assert.equal(wrapped.description, "Search the product docs.");
  assert.deepEqual(wrapped.input, {
    type: "object",
    properties: { query: { type: "string" }, limit: { type: "integer" } },
    required: ["query"],
    additionalProperties: false,
  });
  assert.equal(await wrapped.execute({ query: "x", limit: 2 }), "xx");
});

test("tool rejects invalid names and malformed input schemas at authoring time", () => {
  assert.throws(
    () => tool({ ...details, name: "not valid" }),
    (error: unknown) =>
      error instanceof MuzenError &&
      error.code === "invalid_input" &&
      error.message === "tool.name must match [a-zA-Z0-9_-]{1,64}",
  );
  assert.throws(
    () => tool({
      ...details,
      input: { type: "object", properties: {}, additionalProperties: true } as never,
    }),
    (error: unknown) => error instanceof MuzenError && error.code === "invalid_input",
  );
});

test("loopback server mirrors the Python MCP wire behavior", async () => {
  const fail = tool<{ reason: string }>({
    name: "fail",
    description: "Fail deliberately.",
    input: {
      type: "object",
      properties: { reason: { type: "string" } },
      required: ["reason"],
      additionalProperties: false,
    },
    execute: ({ reason }) => {
      throw new Error(`failed: ${reason}`);
    },
  });
  const echo = tool<{ value: string }>({
    name: "echo",
    description: "Echo text.",
    input: {
      type: "object",
      properties: { value: { type: "string" } },
      required: ["value"],
      additionalProperties: false,
    },
    execute: ({ value }) => value,
  });
  const server = new LoopbackToolServer([details, fail, echo]);
  await server.start();
  try {
    const initialized = await post(server.url, {
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {},
    });
    assert.equal(initialized.response.status, 200);
    assert.equal(initialized.response.headers.get("mcp-session-id"), "muzen-python-tools");
    assert.deepEqual(initialized.body, {
      jsonrpc: "2.0",
      id: 1,
      result: {
        protocolVersion: "2025-03-26",
        capabilities: { tools: {} },
        serverInfo: { name: "muzen-python-tools", version: "1" },
      },
    });

    const notification = await post(server.url, {
      jsonrpc: "2.0",
      method: "notifications/initialized",
      params: {},
    });
    assert.equal(notification.response.status, 202);
    assert.equal(notification.text, "");

    const listed = await post(server.url, { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
    const listedResult = (listed.body as { result: { tools: Array<{ name: string }> } }).result;
    assert.deepEqual(listedResult.tools.map((item) => item.name), ["details", "fail", "echo"]);
    assert.deepEqual(listedResult.tools[0], {
      name: "details",
      description: "Look up details.",
      inputSchema: details.input,
    });

    const called = await post(server.url, {
      jsonrpc: "2.0",
      id: 3,
      method: "tools/call",
      params: { name: "details", arguments: { query: "retry" } },
    });
    assert.deepEqual((called.body as { result: unknown }).result, {
      content: [{ type: "text", text: '{"query": "retry", "count": 1}' }],
      isError: false,
      structuredContent: { query: "retry", count: 1 },
    });

    const echoed = await post(server.url, {
      jsonrpc: "2.0",
      id: 4,
      method: "tools/call",
      params: { name: "echo", arguments: { value: "plain text" } },
    });
    assert.deepEqual((echoed.body as { result: unknown }).result, {
      content: [{ type: "text", text: "plain text" }],
      isError: false,
    });

    const failed = await post(server.url, {
      jsonrpc: "2.0",
      id: 5,
      method: "tools/call",
      params: { name: "fail", arguments: { reason: "boom" } },
    });
    assert.deepEqual((failed.body as { result: unknown }).result, {
      content: [{ type: "text", text: "failed: boom" }],
      isError: true,
    });

    assert.deepEqual(
      (await post(server.url, { jsonrpc: "2.0", id: 6, method: "tools/call", params: { name: "missing" } })).body,
      { jsonrpc: "2.0", id: 6, error: { code: -32602, message: "unknown tool" } },
    );
    assert.deepEqual(
      (await post(server.url, { jsonrpc: "2.0", id: 7, method: "tools/call", params: { name: "echo", arguments: [] } })).body,
      { jsonrpc: "2.0", id: 7, error: { code: -32602, message: "arguments must be an object" } },
    );
    assert.deepEqual(
      (await post(server.url, { jsonrpc: "2.0", id: 8, method: "missing" })).body,
      { jsonrpc: "2.0", id: 8, error: { code: -32601, message: "method not found" } },
    );

    const invalidJson = await fetch(server.url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "{",
    });
    assert.deepEqual(await invalidJson.json(), {
      jsonrpc: "2.0",
      id: null,
      error: { code: -32700, message: "invalid JSON" },
    });
  } finally {
    await server.close();
  }
  assert.throws(() => server.url, /has not been started/);
});

async function post(url: string, payload: unknown): Promise<{
  response: Response;
  text: string;
  body: unknown;
}> {
  const response = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  const text = await response.text();
  return { response, text, body: text.length === 0 ? undefined : JSON.parse(text) as unknown };
}
