import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { createServer, type ServerResponse } from "node:http";
import { createServer as createNetServer } from "node:net";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  JsonRpcDemultiplexer,
  SseParser,
} from "./agent-transports.js";
import {
  MuzenError,
  connectHttp,
  connectLocalRunner,
  normalizeAgentInput,
  type RunLimits,
  type SessionSpec,
} from "./agent.js";

function binary(envName: string, name: string): string | undefined {
  const configured = process.env[envName];
  const path = configured ?? fileURLToPath(new URL(`../../../../../target/debug/${name}`, import.meta.url));
  return existsSync(path) ? path : undefined;
}

class ModelServer {
  readonly server = createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as { messages?: Array<{ content?: unknown }> };
      const blocked = body.messages?.some((message) => JSON.stringify(message.content).includes("block")) === true;
      this.markRequested();
      if (blocked && !this.open) this.waiting.push(response);
      else this.respond(response);
    });
  });
  private waiting: ServerResponse[] = [];
  private open = false;
  private requestedResolve!: () => void;
  requested = new Promise<void>((resolve) => { this.requestedResolve = resolve; });

  private markRequested(): void { this.requestedResolve(); }
  private respond(response: ServerResponse): void {
    const body = JSON.stringify({ content: [{ type: "text", text: "done" }], usage: { input_tokens: 1, output_tokens: 1 } });
    response.writeHead(200, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) });
    response.end(body);
  }
  release(): void { this.open = true; for (const response of this.waiting.splice(0)) this.respond(response); }
  async start(): Promise<string> {
    await new Promise<void>((resolve) => this.server.listen(0, "127.0.0.1", resolve));
    const address = this.server.address();
    if (address === null || typeof address === "string") throw new Error("model server did not bind TCP");
    return `http://127.0.0.1:${address.port}`;
  }
  async close(): Promise<void> { this.release(); await new Promise<void>((resolve, reject) => this.server.close((error) => error === undefined ? resolve() : reject(error))); }
}

function sessionSpec(secret: string, baseUrl: string): SessionSpec {
  return {
    agent: { name: "test-agent", instructions: [{ type: "text", text: "Answer the user." }], model: "primary", tools: [] },
    models: [{ id: "primary", provider: "anthropic", protocol: "messages", model: "test-model", baseUrl, credential: secret, maxInputTokens: 4096, maxOutputTokens: 256 }],
    toolProviders: [],
    workspace: { base: { kind: "path", root: "/tmp" } },
  };
}

const limits: RunLimits = { maxActiveAgents: 1, maxAgents: 1, maxDepth: 0, maxInputBytes: 64 * 1024, deadlineMs: 10_000 };

async function waitUntil(predicate: () => Promise<boolean>, timeoutMs = 5_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!(await predicate())) {
    if (Date.now() >= deadline) throw new Error("condition was not reached before timeout");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

test("SSE parser handles split UTF-8, CRLF, comments, and multiline data", () => {
  const wire = Buffer.from(": keepalive\r\nid: 7\r\nevent: run.event\r\ndata: {\"text\":\"café\",\r\ndata: \"sequence\":7}\r\n\r\n");
  const parser = new SseParser();
  const output: string[] = [];
  let previous = 0;
  for (const split of [1, 13, 43, 61, wire.length]) {
    output.push(...parser.feed(wire.subarray(previous, split)));
    previous = split;
  }
  output.push(...parser.feed(new Uint8Array(), true));
  assert.deepEqual(output, ['{"text":"café",\n"sequence":7}']);
});

test("JSON-RPC demultiplexer handles interleaved notifications", async () => {
  const demux = new JsonRpcDemultiplexer();
  const first = demux.response(1);
  const second = demux.response(2);
  const queue = demux.subscribe("sub");
  demux.feed({ jsonrpc: "2.0", method: "run.event", params: { subscriptionId: "sub", event: { sequence: 1 } } });
  demux.feed({ jsonrpc: "2.0", id: 2, result: "second" });
  demux.feed({ jsonrpc: "2.0", id: 1, result: "first" });
  assert.equal(await first, "first");
  assert.equal(await second, "second");
  assert.deepEqual(await queue.next(), { sequence: 1 });
});

function sseEvent(sequence: number, type: string): string {
  return `id: ${sequence}\nevent: run.event\ndata: ${JSON.stringify({ runId: "run-idle", sequence, type, timestamp: "now", payload: {} })}\n\n`;
}

test("HTTP events reconnect after idle with cursor and no duplicates", async () => {
  const requests: Array<{ after: string | null; lastEventId: string | undefined }> = [];
  let firstResponse: ServerResponse | undefined;
  const server = createServer((request, response) => {
    if (request.url === "/v1/runs/run-idle") {
      const body = JSON.stringify({ id: "run-idle", status: "running", roots: [], agents: [], lastSequence: 0, createdAt: "now", updatedAt: "now" });
      response.writeHead(200, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) });
      response.end(body);
      return;
    }
    const url = new URL(request.url ?? "", "http://127.0.0.1");
    const lastEventId = request.headers["last-event-id"];
    requests.push({
      after: url.searchParams.get("after"),
      lastEventId: Array.isArray(lastEventId) ? lastEventId[0] : lastEventId,
    });
    response.writeHead(200, { "Content-Type": "text/event-stream" });
    if (requests.length === 1) {
      firstResponse = response;
      response.write(sseEvent(1, "run.started"));
      return;
    }
    response.end(sseEvent(2, "agent.completed") + sseEvent(3, "run.completed"));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("HTTP test server did not bind TCP");
  const muzen = await connectHttp(`http://127.0.0.1:${address.port}`, { sseIdleTimeoutMs: 50 });
  try {
    const run = await muzen.getRun("run-idle");
    const events = [];
    for await (const event of run.events()) events.push(event);
    assert.deepEqual(events.map((event) => event.sequence), [1, 2, 3]);
    assert.deepEqual(requests, [
      { after: null, lastEventId: undefined },
      { after: "1", lastEventId: "1" },
    ]);
  } finally {
    firstResponse?.destroy();
    await muzen.close();
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) => server.close((error) => error === undefined ? resolve() : reject(error)));
  }
});

test("local runner full lifecycle, replay, unsubscribe, and errors", async (t) => {
  const runner = binary("MUZEN_AGENT_RUNNER_BIN", "muzen-agent-runner");
  if (runner === undefined) return t.skip("muzen-agent-runner is missing; set MUZEN_AGENT_RUNNER_BIN or build target/debug/muzen-agent-runner");
  const model = new ModelServer();
  const baseUrl = await model.start();
  const muzen = await connectLocalRunner({ store: "memory", binaryPath: runner, allowLoopbackHttp: true });
  try {
    const secretInput = { value: "dGVzdC1rZXk=", idempotencyKey: "secret-replay" };
    const secret = await muzen.putSecret(secretInput);
    assert.equal(await muzen.putSecret(secretInput), secret);
    const session = await muzen.createSession(sessionSpec(secret, baseUrl));
    const run = await session.run("hello", { limits });
    const events = [];
    for await (const event of run.events()) events.push(event);
    assert.deepEqual(events.map((event) => event.sequence), Array.from({ length: events.length }, (_, index) => index + 1));
    assert.equal(events.at(-1)?.type, "run.completed");
    const result = await run.wait();
    assert.equal(result.status, "completed");
    assert.equal(result.outputs[0]?.output, "done");
    const messages = await session.messages();
    assert.deepEqual(messages.items.map((message) => message.role), ["user", "assistant"]);
    assert.deepEqual(messages.items[1]?.content, [{ type: "text", text: "done" }]);
    await assert.rejects(run.send({ sessionId: session.id, input: normalizeAgentInput("too late"), delivery: "follow_up" }), (error: unknown) => error instanceof MuzenError && error.code === "conflict");
    await assert.rejects(muzen.getRun("run_does_not_exist"), (error: unknown) => error instanceof MuzenError && error.code === "not_found");

    const blocked = await session.run("block", { limits });
    await model.requested;
    await waitUntil(async () => (await blocked.snapshot()).lastSequence >= 3);
    const iterator = blocked.events({ after: 2 })[Symbol.asyncIterator]();
    assert.equal((await iterator.next()).value?.sequence, 3);
    await iterator.return?.();
    model.release();
    const replay = [];
    for await (const event of blocked.events()) replay.push(event);
    assert.deepEqual(replay.map((event) => event.sequence), Array.from({ length: replay.length }, (_, index) => index + 1));
    assert.equal(replay.at(-1)?.type, "run.completed");
  } finally {
    model.release();
    await muzen.close();
    await model.close();
  }
});

async function unusedPort(): Promise<number> {
  const server = createNetServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("port server did not bind TCP");
  const port = address.port;
  await new Promise<void>((resolve, reject) => server.close((error) => error === undefined ? resolve() : reject(error)));
  return port;
}

async function waitForService(port: number, process: ChildProcess): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    if (process.exitCode !== null) throw new Error(`muzen-agent-service exited with ${process.exitCode}`);
    try { await fetch(`http://127.0.0.1:${port}/v1/capabilities`); return; } catch { await new Promise((resolve) => setTimeout(resolve, 20)); }
  }
  throw new Error("muzen-agent-service did not listen before timeout");
}

async function stopProcess(process: ChildProcess): Promise<void> {
  if (process.exitCode !== null) return;
  const exited = new Promise<void>((resolve) => process.once("exit", () => resolve()));
  process.kill("SIGTERM");
  const timer = setTimeout(() => process.kill("SIGKILL"), 3_000);
  await exited;
  clearTimeout(timer);
}

test("HTTP service auth, SSE wait, and idempotency", async (t) => {
  const service = binary("MUZEN_AGENT_SERVICE_BIN", "muzen-agent-service");
  if (service === undefined) return t.skip("muzen-agent-service is missing; set MUZEN_AGENT_SERVICE_BIN or build target/debug/muzen-agent-service");
  const port = await unusedPort();
  const process = spawn(service, ["--listen", `127.0.0.1:${port}`, "--store", "memory", "--allow-loopback-http", "--bearer-token", "test-token"], { stdio: ["ignore", "ignore", "pipe"] });
  const model = new ModelServer();
  try {
    await waitForService(port, process);
    const baseUrl = `http://127.0.0.1:${port}`;
    const unauthenticated = await connectHttp(baseUrl);
    try { await assert.rejects(unauthenticated.capabilities(), (error: unknown) => error instanceof MuzenError && error.code === "unauthenticated"); }
    finally { await unauthenticated.close(); }

    const modelUrl = await model.start();
    const muzen = await connectHttp(baseUrl, { bearerToken: "test-token" });
    try {
      const secret = await muzen.putSecret({ value: "dGVzdC1rZXk=" });
      const spec = sessionSpec(secret, modelUrl);
      const first = await muzen.createSession(spec, { idempotencyKey: "session-replay" });
      const replay = await muzen.createSession(spec, { idempotencyKey: "session-replay" });
      assert.equal(replay.id, first.id);
      const result = await (await first.run("hello", { limits })).wait();
      assert.equal(result.status, "completed");
      assert.equal(result.outputs[0]?.output, "done");
    } finally { await muzen.close(); }
  } finally {
    await model.close().catch(() => undefined);
    await stopProcess(process);
  }
});

test("HTTP client posts raw tool outcomes with auth and encoded identifiers", async () => {
  let observed: { method?: string; url?: string; authorization?: string | undefined; body?: unknown } = {};
  const server = createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      observed = {
        method: request.method,
        url: request.url,
        authorization: request.headers.authorization,
        body: JSON.parse(Buffer.concat(chunks).toString("utf8")) as unknown,
      };
      response.writeHead(204);
      response.end();
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("HTTP test server did not bind TCP");
  const muzen = await connectHttp(`http://127.0.0.1:${address.port}`, { bearerToken: "test-token" });
  try {
    await muzen.answerToolCall("run/1", {
      callId: "call/1",
      outcome: { result: { source: "client", count: 2 } },
    });
    assert.deepEqual(observed, {
      method: "POST",
      url: "/v1/runs/run%2F1/tools/call%2F1/result",
      authorization: "Bearer test-token",
      body: { result: { source: "client", count: 2 } },
    });
  } finally {
    await muzen.close();
    await new Promise<void>((resolve, reject) => server.close((error) => error === undefined ? resolve() : reject(error)));
  }
});
