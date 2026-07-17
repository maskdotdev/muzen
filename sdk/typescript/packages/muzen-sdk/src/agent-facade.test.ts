import assert from "node:assert/strict";
import { existsSync, statSync } from "node:fs";
import { createServer } from "node:http";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  Agent,
  AgentResult,
  MuzenError,
  discoverLocalRunnerBinary,
  tool,
  type AgentInputLike,
  type AgentSession,
  type JsonValue,
  type Muzen,
  type Run,
  type RunLimits,
  type RunResult,
  type SessionSpec,
  type Usage,
} from "./agent.js";

const usage: Usage = { inputTokens: 2, outputTokens: 1, toolCalls: 0 };

class FakeSession {
  readonly id: string;
  readonly runs: Array<{ prompt: AgentInputLike; limits: RunLimits }> = [];
  archived = false;

  constructor(readonly spec: SessionSpec, number: number, private readonly output: JsonValue = "done") {
    this.id = `session-${number}`;
  }

  async run(prompt: AgentInputLike, options: { limits: RunLimits }): Promise<Run> {
    this.runs.push({ prompt, limits: options.limits });
    const result: RunResult = {
      runId: "run-1",
      status: "completed",
      outputs: [{ sessionId: this.id, path: [], status: "completed", output: this.output, usage }],
      usage,
      artifacts: [],
      metadata: {},
    };
    return { id: result.runId, wait: async () => result } as unknown as Run;
  }

  async archive(): Promise<void> {
    this.archived = true;
  }
}

class FakeMuzen {
  readonly secrets: string[] = [];
  readonly sessions: FakeSession[] = [];
  closed = false;

  constructor(private readonly output: JsonValue = "done") {}

  async putSecret(input: { value: string }): Promise<string> {
    this.secrets.push(input.value);
    return `secret-${this.secrets.length}`;
  }

  async createSession(spec: SessionSpec): Promise<AgentSession> {
    const session = new FakeSession(spec, this.sessions.length + 1, this.output);
    this.sessions.push(session);
    return session as unknown as AgentSession;
  }

  async close(): Promise<void> {
    this.closed = true;
  }
}

async function runAndSpec(model: string): Promise<SessionSpec> {
  const fake = new FakeMuzen();
  const agent = new Agent({ client: fake as unknown as Muzen, instructions: "do it", model, apiKey: "test" });
  await agent.run("hello");
  await agent.close();
  return fake.sessions[0]!.spec;
}

test("model strings synthesize provider, protocol, name, and one lazy secret", async () => {
  const cases = [
    ["claude-sonnet-5", "anthropic", "messages", "claude-sonnet-5"],
    ["gpt-4o-mini", "openai_compatible", "chat_completions", "gpt-4o-mini"],
    ["anthropic:not-claude", "anthropic", "messages", "not-claude"],
    ["openai:claude-named", "openai_compatible", "chat_completions", "claude-named"],
  ] as const;
  for (const [model, provider, protocol, name] of cases) {
    const profile = (await runAndSpec(model)).models[0]!;
    assert.deepEqual([profile.provider, profile.protocol, profile.model, profile.credential], [provider, protocol, name, "secret-1"]);
  }
});

test("missing provider environment key is invalid input", () => {
  const previous = process.env.ANTHROPIC_API_KEY;
  delete process.env.ANTHROPIC_API_KEY;
  try {
    assert.throws(
      () => new Agent({ instructions: "do it", model: "claude-test" }),
      (error: unknown) => error instanceof MuzenError && error.code === "invalid_input" && error.message.includes("ANTHROPIC_API_KEY"),
    );
  } finally {
    if (previous === undefined) delete process.env.ANTHROPIC_API_KEY;
    else process.env.ANTHROPIC_API_KEY = previous;
  }
});

test("facade builds instructions, swarm grants, output contract, overrides, and defaults", async () => {
  const fake = new FakeMuzen({ summary: "ok", issues: ["one"] });
  const schema = {
    type: "object",
    properties: { summary: { type: "string" }, issues: { type: "array", items: { type: "string" } } },
    required: ["summary", "issues"],
    additionalProperties: false,
  };
  const agent = new Agent<{ summary: string; issues: string[] }>({
    client: fake as unknown as Muzen,
    instructions: "review carefully",
    model: "gpt-test",
    output: schema,
    apiKey: "test",
    canSpawn: true,
    canMessage: true,
    temperature: 0.25,
    maxOutputTokens: 777,
  });
  assert.deepEqual(fake.secrets, []);
  const result = await agent.run("one");
  await agent.run("two");
  const spec = fake.sessions[0]!.spec;
  assert.deepEqual(spec.agent.instructions, [{ type: "text", text: "review carefully" }]);
  assert.deepEqual(spec.agent.tools, [
    { provider: "builtin", tool: "agent.spawn", effects: ["agent_spawn"] },
    { provider: "builtin", tool: "agent.message", effects: ["agent_message"] },
  ]);
  assert.deepEqual(spec.toolProviders, [{ id: "builtin", kind: "builtin" }]);
  assert.deepEqual(spec.agent.output, { schema });
  assert.equal(spec.models[0]!.temperature, 0.25);
  assert.equal(spec.models[0]!.maxOutputTokens, 777);
  assert.equal(result.output.issues[0], "one");
  assert.equal(result.text, '{"summary":"ok","issues":["one"]}');
  assert.deepEqual(fake.sessions[0]!.runs[0]!.limits, {
    maxActiveAgents: 4,
    maxAgents: 16,
    maxDepth: 3,
    maxInputBytes: 1_048_576,
  });
  assert.equal(fake.secrets.length, 1);
  assert.ok(fake.sessions.every((session) => session.archived));
  await agent.close();
});

test("HTTP transport rejects local tools because the service cannot reach loopback", () => {
  const lookup = tool<{ query: string }>({
    name: "lookup",
    description: "Look up a query.",
    input: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
      additionalProperties: false,
    },
    execute: ({ query }) => query,
  });
  assert.throws(
    () => new Agent({
      instructions: "do it",
      model: "gpt-test",
      tools: [lookup],
      transport: "http",
      baseUrl: "https://muzen.example",
    }),
    (error: unknown) =>
      error instanceof MuzenError &&
      error.code === "invalid_input" &&
      error.message.includes("remote service cannot reach the client's loopback server"),
  );
});

test("facade composes local tool grants and lazily installs one MCP provider", async () => {
  const fake = new FakeMuzen();
  const lookup = tool<{ query: string }>({
    name: "lookup",
    description: "Look up a query.",
    input: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
      additionalProperties: false,
    },
    execute: ({ query }) => query,
  });
  const agent = new Agent({
    client: fake as unknown as Muzen,
    instructions: "use tools",
    model: "gpt-test",
    apiKey: "test",
    tools: [lookup],
    canSpawn: true,
    canMessage: true,
  });
  await agent.run("hello");
  const spec = fake.sessions[0]!.spec;
  assert.deepEqual(spec.agent.tools, [
    { provider: "builtin", tool: "agent.spawn", effects: ["agent_spawn"] },
    { provider: "builtin", tool: "agent.message", effects: ["agent_message"] },
    { provider: "local_tools", tool: "lookup", effects: [] },
  ]);
  assert.deepEqual(spec.toolProviders.map((provider) => [provider.id, provider.kind]), [
    ["builtin", "builtin"],
    ["local_tools", "mcp_http"],
  ]);
  const localProvider = spec.toolProviders[1];
  assert.ok(localProvider?.kind === "mcp_http" && localProvider.url.startsWith("http://127.0.0.1:") && localProvider.url.endsWith("/mcp"));
  const loopbackUrl = localProvider.url;
  await agent.close();
  assert.equal(fake.closed, true);
  await assert.rejects(fetch(loopbackUrl));
});

test("facade rejects duplicate local names and collisions with model-visible built-ins", () => {
  const named = (name: string) => tool({
    name,
    description: "Test tool.",
    input: { type: "object", properties: {}, additionalProperties: false },
    execute: () => undefined,
  });
  assert.throws(
    () => new Agent({ instructions: "use tools", model: "gpt-test", apiKey: "test", tools: [named("same"), named("same")] }),
    (error: unknown) => error instanceof MuzenError && error.code === "invalid_input" && error.message === "tools must have unique function names",
  );
  assert.throws(
    () => new Agent({ instructions: "use tools", model: "gpt-test", apiKey: "test", tools: [named("agent_spawn")], canSpawn: true }),
    (error: unknown) => error instanceof MuzenError && error.code === "invalid_input" && error.message === "tools must have unique function names",
  );
});

test("failed local-runner connections close the tool server and close remains idempotent", async () => {
  const previous = process.env.MUZEN_AGENT_RUNNER_BIN;
  process.env.MUZEN_AGENT_RUNNER_BIN = `/tmp/muzen-missing-runner-${process.pid}`;
  const lookup = tool({
    name: "lookup",
    description: "Look up a query.",
    input: { type: "object", properties: {}, additionalProperties: false },
    execute: () => "done",
  });
  const agent = new Agent({ instructions: "use tools", model: "gpt-test", apiKey: "test", tools: [lookup] });
  const server = (agent as unknown as { toolServer: { readonly url: string } }).toolServer;
  try {
    await assert.rejects(
      agent.run("hello"),
      (error: unknown) => error instanceof MuzenError && error.code === "unavailable",
    );
    assert.throws(() => server.url, /has not been started/);
    await agent.close();
    await agent.close();
  } finally {
    if (previous === undefined) delete process.env.MUZEN_AGENT_RUNNER_BIN;
    else process.env.MUZEN_AGENT_RUNNER_BIN = previous;
    await agent.close();
  }
});

test("spec and client escape hatches bypass model synthesis", async () => {
  const fake = new FakeMuzen();
  const spec: SessionSpec = {
    agent: { name: "custom", instructions: [{ type: "text", text: "custom" }], model: "custom", tools: [] },
    models: [{ id: "custom", provider: "anthropic", protocol: "messages", model: "custom", credential: "already-set", maxInputTokens: 10, maxOutputTokens: 10 }],
    toolProviders: [],
    workspace: { base: { kind: "path", root: "/tmp" } },
  };
  const agent = new Agent({ spec, client: fake as unknown as Muzen });
  await agent.run("hello", { limits: { maxActiveAgents: 1, maxAgents: 1, maxDepth: 0, maxInputBytes: 32 } });
  assert.equal(fake.sessions[0]!.spec, spec);
  assert.equal(fake.secrets.length, 0);
  assert.equal(fake.sessions[0]!.runs[0]!.limits.maxAgents, 1);
  await agent.close();
  assert.equal(fake.closed, true);
});

test("session reuses one session and archives on async disposal", async () => {
  const fake = new FakeMuzen();
  const agent = new Agent({ client: fake as unknown as Muzen, instructions: "do it", model: "gpt-test", apiKey: "test" });
  {
    await using chat = agent.session();
    const first = await chat.run("first");
    const second = await chat.run("follow-up");
    assert.equal(first.text, second.output);
  }
  assert.equal(fake.sessions.length, 1);
  assert.deepEqual(fake.sessions[0]!.runs.map((run) => run.prompt), ["first", "follow-up"]);
  assert.equal(fake.sessions[0]!.archived, true);
  await agent.run("fresh");
  assert.equal(fake.sessions.length, 2);
  await agent.close();
});

test("raiseForStatus converts terminal failures only when requested", () => {
  const raw: RunResult = {
    runId: "run-1",
    status: "failed",
    outputs: [{
      sessionId: "session-1",
      path: [],
      status: "failed",
      usage,
      error: { code: "model_error", message: "provider failed", retryable: true },
    }],
    usage,
    artifacts: [],
    metadata: {},
  };
  const result = new AgentResult("null", "null", usage, "failed", raw.runId, raw);
  assert.throws(
    () => result.raiseForStatus(),
    (error: unknown) => error instanceof MuzenError && error.code === "internal" && error.retryable && error.message === "provider failed",
  );
});

class ModelServer {
  readonly requests: Array<{ messages?: Array<{ role?: string; content?: unknown }> }> = [];
  readonly server = createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      const payload = JSON.parse(Buffer.concat(chunks).toString("utf8")) as { messages?: Array<{ role?: string; content?: unknown }> };
      this.requests.push(payload);
      const structured = JSON.stringify(payload.messages).includes("structured");
      const text = structured ? '{"summary":"ok","issues":["one"]}' : "done";
      const body = JSON.stringify({ content: [{ type: "text", text }], usage: { input_tokens: 1, output_tokens: 1 } });
      response.writeHead(200, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) });
      response.end(body);
    });
  });

  async start(): Promise<string> {
    await new Promise<void>((resolve) => this.server.listen(0, "127.0.0.1", resolve));
    const address = this.server.address();
    if (address === null || typeof address === "string") throw new Error("model server did not bind TCP");
    return `http://127.0.0.1:${address.port}`;
  }

  async close(): Promise<void> {
    await new Promise<void>((resolve, reject) => this.server.close((error) => error === undefined ? resolve() : reject(error)));
  }
}

class ToolModelServer {
  readonly requests: Array<Record<string, unknown>> = [];
  readonly server = createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      const payload = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
      this.requests.push(payload);
      const hasResult = JSON.stringify(payload.messages).includes('"type":"tool_result"');
      const reply = hasResult
        ? {
            content: [{ type: "text", text: "tool completed" }],
            usage: { input_tokens: 2, output_tokens: 1 },
            stop_reason: "end_turn",
          }
        : {
            content: [{
              type: "tool_use",
              id: "search-1",
              name: "search",
              input: { query: "retry policy", limit: 3 },
            }],
            usage: { input_tokens: 2, output_tokens: 1 },
            stop_reason: "tool_use",
          };
      const body = JSON.stringify(reply);
      response.writeHead(200, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) });
      response.end(body);
    });
  });

  async start(): Promise<string> {
    await new Promise<void>((resolve) => this.server.listen(0, "127.0.0.1", resolve));
    const address = this.server.address();
    if (address === null || typeof address === "string") throw new Error("model server did not bind TCP");
    return `http://127.0.0.1:${address.port}`;
  }

  async close(): Promise<void> {
    await new Promise<void>((resolve, reject) => this.server.close((error) => error === undefined ? resolve() : reject(error)));
  }
}

test("facade local runner supports one-shot, continuity, and structured output", async (t) => {
  const binary = discoverLocalRunnerBinary();
  if (binary === undefined || !existsSync(binary)) {
    return t.skip("muzen-agent-runner is missing; set MUZEN_AGENT_RUNNER_BIN or build target/debug/muzen-agent-runner");
  }
  const model = new ModelServer();
  const baseUrl = await model.start();
  const agent = new Agent({ instructions: "Answer the user.", model: "claude-test", baseUrl, apiKey: "test" });
  try {
    assert.equal((await agent.run("hello")).text, "done");
    {
      await using chat = agent.session();
      await chat.run("first");
      await chat.run("follow-up");
      const session = (chat as unknown as { sessionValue: AgentSession }).sessionValue;
      assert.deepEqual((await session.messages()).items.map((message) => message.role), ["user", "assistant", "user", "assistant"]);
    }
    const reviewer = new Agent<{ summary: string; issues: string[] }>({
      instructions: "Return structured findings.",
      model: "claude-test",
      baseUrl,
      apiKey: "test",
      output: {
        type: "object",
        properties: { summary: { type: "string" }, issues: { type: "array", items: { type: "string" } } },
        required: ["summary", "issues"],
        additionalProperties: false,
      },
    });
    try {
      const result = await reviewer.run("structured");
      assert.deepEqual(result.output, { summary: "ok", issues: ["one"] });
      assert.equal(result.output.issues[0], "one");
    } finally {
      await reviewer.close();
    }
  } finally {
    await agent.close();
    await model.close();
  }
});

test("facade local runner executes a TypeScript tool and returns its result to the model transcript", async (t) => {
  const binary = discoverLocalRunnerBinary();
  if (binary === undefined || !existsSync(binary)) {
    return t.skip("muzen-agent-runner is missing; set MUZEN_AGENT_RUNNER_BIN or build target/debug/muzen-agent-runner");
  }
  const mcpSource = fileURLToPath(new URL("../../../../../src/agent_runtime/local/mcp.rs", import.meta.url));
  if (statSync(mcpSource).mtimeMs > statSync(binary).mtimeMs) {
    return t.skip("built muzen-agent-runner predates MCP HTTP tool support");
  }

  const calls: Array<{ query: string; limit: number }> = [];
  const search = tool<{ query: string; limit?: number }>({
    name: "search",
    description: "Search the product docs.",
    input: {
      type: "object",
      properties: { query: { type: "string" }, limit: { type: "integer" } },
      required: ["query"],
      additionalProperties: false,
    },
    execute: ({ query, limit = 5 }) => {
      calls.push({ query, limit });
      return "retry three times";
    },
  });
  const model = new ToolModelServer();
  const baseUrl = await model.start();
  const agent = new Agent({
    instructions: "Answer using tools.",
    model: "claude-test",
    tools: [search],
    baseUrl,
    apiKey: "test",
  });
  try {
    const result = await agent.run("find the retry policy");
    assert.equal(result.text, "tool completed");
    assert.deepEqual(calls, [{ query: "retry policy", limit: 3 }]);
    assert.equal(model.requests.length, 2);
    assert.deepEqual(model.requests[0]!.tools, [{
      name: "search",
      description: "Search the product docs.",
      input_schema: search.input,
    }]);
    assert.ok(
      JSON.stringify(model.requests[1]!.messages).includes('"type":"tool_result"') &&
      JSON.stringify(model.requests[1]!.messages).includes("retry three times"),
    );
  } finally {
    await agent.close();
    await model.close();
  }
});
