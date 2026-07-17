import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";

import {
  MuzenError,
  normalizeAgentInput,
  type AgentEvent,
  type AgentInputLike,
  type AgentMessage,
  type AgentSession,
  type AnswerToolCallInput,
  type Artifact,
  type ArtifactId,
  type ArtifactRef,
  type CancelOptions,
  type Capabilities,
  type CommandOptions,
  type CommandReceipt,
  type CreateOptions,
  type ErrorCode,
  type EventOptions,
  type JsonObject,
  type Muzen,
  type Page,
  type PutSecretInput,
  type Run,
  type RunResult,
  type RunSnapshot,
  type RunSpec,
  type SecretRef,
  type SendCommand,
  type SessionSnapshot,
  type SessionSpec,
  type SingleRunOptions,
  type SpawnCommand,
} from "./agent.js";

const TERMINAL_EVENTS = new Set(["run.completed", "run.partial", "run.failed", "run.cancelled"]);
const ARTIFACT_CHUNK_BYTES = 64 * 1024;

type WireObject = Record<string, unknown>;

function unavailable(message: string): MuzenError {
  return new MuzenError("unavailable", message, true);
}

function isObject(value: unknown): value is WireObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function wireError(value: unknown, fallback = "Muzen request failed"): MuzenError {
  if (
    isObject(value) &&
    typeof value.code === "string" &&
    typeof value.message === "string" &&
    typeof value.retryable === "boolean"
  ) {
    return new MuzenError(
      value.code as ErrorCode,
      value.message,
      value.retryable,
      isObject(value.details) ? value.details : undefined,
    );
  }
  return new MuzenError("internal", fallback, false);
}

class AsyncQueue<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<(value: T) => void> = [];

  push(value: T): void {
    const waiter = this.waiters.shift();
    if (waiter === undefined) this.values.push(value);
    else waiter(value);
  }

  next(): Promise<T> {
    const value = this.values.shift();
    if (value !== undefined) return Promise.resolve(value);
    return new Promise((resolve) => this.waiters.push(resolve));
  }
}

type Pending = { resolve(value: unknown): void; reject(error: unknown): void };

/** Exported only to make the protocol demultiplexer independently testable. */
export class JsonRpcDemultiplexer {
  private readonly pending = new Map<number, Pending>();
  private readonly subscriptions = new Map<string, AsyncQueue<unknown>>();

  response(requestId: number): Promise<unknown> {
    return new Promise((resolve, reject) => this.pending.set(requestId, { resolve, reject }));
  }

  removeResponse(requestId: number): void {
    this.pending.delete(requestId);
  }

  subscribe(subscriptionId: string): AsyncQueue<unknown> {
    const queue = new AsyncQueue<unknown>();
    this.subscriptions.set(subscriptionId, queue);
    return queue;
  }

  unsubscribe(subscriptionId: string): void {
    this.subscriptions.delete(subscriptionId);
  }

  feed(message: WireObject): void {
    if (typeof message.id === "number") {
      const pending = this.pending.get(message.id);
      if (pending === undefined) return;
      this.pending.delete(message.id);
      if (isObject(message.error)) {
        if (message.error.code === -32000) {
          pending.reject(wireError(message.error.data, typeof message.error.message === "string" ? message.error.message : "request failed"));
        } else {
          pending.reject(new MuzenError("internal", typeof message.error.message === "string" ? message.error.message : "JSON-RPC request failed", false));
        }
      } else {
        pending.resolve(message.result);
      }
      return;
    }
    if (message.method === "run.event" && isObject(message.params)) {
      const subscriptionId = message.params.subscriptionId;
      if (typeof subscriptionId === "string") this.subscriptions.get(subscriptionId)?.push(message.params.event);
    }
  }

  fail(error: MuzenError): void {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
    for (const queue of this.subscriptions.values()) queue.push(error);
  }
}

/** Incremental UTF-8 SSE data-field parser. */
export class SseParser {
  private readonly decoder = new TextDecoder();
  private buffer = "";
  private data: string[] = [];

  feed(chunk: Uint8Array, final = false): string[] {
    this.buffer += this.decoder.decode(chunk, { stream: !final });
    const output: string[] = [];
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) break;
      let line = this.buffer.slice(0, newline);
      this.buffer = this.buffer.slice(newline + 1);
      if (line.endsWith("\r")) line = line.slice(0, -1);
      this.line(line, output);
    }
    if (final) {
      if (this.buffer.length > 0) {
        const line = this.buffer.endsWith("\r") ? this.buffer.slice(0, -1) : this.buffer;
        this.line(line, output);
        this.buffer = "";
      }
      this.line("", output);
    }
    return output;
  }

  private line(line: string, output: string[]): void {
    if (line === "") {
      if (this.data.length > 0) output.push(this.data.join("\n"));
      this.data = [];
      return;
    }
    if (line.startsWith(":")) return;
    const colon = line.indexOf(":");
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "data") this.data.push(value);
  }
}

interface Transport {
  readonly kind: "runner" | "http";
  request(method: string, pathOrParams?: string | WireObject, body?: unknown, idempotencyKey?: string, headers?: HeadersInit, raw?: boolean): Promise<unknown>;
  events(runId: string, after: number | undefined, signal: AbortSignal | undefined): AsyncIterable<AgentEvent>;
  close(): Promise<void>;
}

class RunnerTransport implements Transport {
  readonly kind = "runner" as const;
  private readonly demux = new JsonRpcDemultiplexer();
  private nextId = 1;
  private closed = false;
  private failure: MuzenError | undefined;
  private readonly exited: Promise<void>;

  constructor(private readonly process: ChildProcessWithoutNullStreams, private readonly closeTimeoutMs: number) {
    const lines = createInterface({ input: process.stdout, crlfDelay: Infinity });
    lines.on("line", (line) => {
      try {
        const message: unknown = JSON.parse(line);
        if (!isObject(message)) throw new Error("message is not an object");
        this.demux.feed(message);
      } catch (error) {
        this.fail(unavailable(`invalid JSON from local runner: ${String(error)}`));
      }
    });
    process.on("error", (error) => this.fail(unavailable(`local runner failed: ${error.message}`)));
    this.exited = new Promise((resolve) => process.once("exit", () => {
      this.fail(unavailable("local runner transport closed"));
      resolve();
    }));
  }

  private fail(error: MuzenError): void {
    if (this.failure !== undefined) return;
    this.failure = error;
    this.demux.fail(error);
  }

  async request(method: string, params: string | WireObject = {}): Promise<unknown> {
    if (this.closed || this.failure !== undefined || this.process.exitCode !== null) {
      throw this.failure ?? unavailable("local runner is not available");
    }
    if (typeof params === "string") throw new MuzenError("internal", "invalid runner request parameters", false);
    const id = this.nextId++;
    const response = this.demux.response(id);
    const wire = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    try {
      await new Promise<void>((resolve, reject) => {
        this.process.stdin.write(wire, (error) => error === null || error === undefined ? resolve() : reject(error));
      });
    } catch (error) {
      this.demux.removeResponse(id);
      throw unavailable(`local runner write failed: ${String(error)}`);
    }
    return response;
  }

  async *events(runId: string, after: number | undefined, signal: AbortSignal | undefined): AsyncIterable<AgentEvent> {
    const subscriptionId = crypto.randomUUID();
    const queue = this.demux.subscribe(subscriptionId);
    let subscribed = false;
    let cursor = after;
    const abort = () => queue.push(unavailable("event stream aborted"));
    signal?.addEventListener("abort", abort, { once: true });
    try {
      for (;;) {
        if (signal?.aborted) throw unavailable("event stream aborted");
        const params: WireObject = { runId, subscriptionId };
        if (cursor !== undefined) params.after = cursor;
        const response = await this.request("run.events", params);
        if (!isObject(response) || !Array.isArray(response.events)) throw new MuzenError("internal", "invalid run.events response", false);
        subscribed = response.subscribed === true;
        for (const value of response.events) {
          const event = value as AgentEvent;
          cursor = event.sequence;
          yield event;
          if (TERMINAL_EVENTS.has(event.type)) return;
        }
        if (subscribed) break;
        if (response.events.length === 0) return;
      }
      for (;;) {
        const value = await queue.next();
        if (value instanceof MuzenError) throw value;
        const event = value as AgentEvent;
        yield event;
        if (TERMINAL_EVENTS.has(event.type)) return;
      }
    } finally {
      signal?.removeEventListener("abort", abort);
      this.demux.unsubscribe(subscriptionId);
      if (subscribed && !this.closed) void this.request("run.unsubscribe", { subscriptionId }).catch(() => undefined);
    }
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.process.stdin.end();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timedOut = new Promise<"timeout">((resolve) => { timer = setTimeout(() => resolve("timeout"), this.closeTimeoutMs); });
    if (await Promise.race([this.exited.then(() => "exit" as const), timedOut]) === "timeout") {
      this.process.kill();
      await this.exited;
    }
    if (timer !== undefined) clearTimeout(timer);
  }
}

const STATUS_CODES: Readonly<Record<number, ErrorCode>> = {
  400: "invalid_input", 401: "unauthenticated", 403: "permission_denied", 404: "not_found",
  409: "conflict", 429: "resource_exhausted", 500: "internal", 501: "unsupported",
  503: "unavailable", 504: "deadline_exceeded",
};

class HttpTransport implements Transport {
  readonly kind = "http" as const;
  private readonly baseUrl: URL;
  private closed = false;

  constructor(
    baseUrl: string,
    private readonly bearerToken?: string,
    private readonly sseIdleTimeoutMs: number | null = 30_000,
  ) {
    let parsed: URL;
    try { parsed = new URL(baseUrl); } catch { throw new MuzenError("invalid_input", "baseUrl must be an HTTP(S) URL", false); }
    if ((parsed.protocol !== "http:" && parsed.protocol !== "https:") || parsed.hostname === "") throw new MuzenError("invalid_input", "baseUrl must be an HTTP(S) URL", false);
    if (parsed.search !== "" || parsed.hash !== "") throw new MuzenError("invalid_input", "baseUrl must not contain a query or fragment", false);
    if (sseIdleTimeoutMs !== null && (!Number.isFinite(sseIdleTimeoutMs) || sseIdleTimeoutMs <= 0)) throw new MuzenError("invalid_input", "sseIdleTimeoutMs must be positive", false);
    parsed.pathname = parsed.pathname.replace(/\/$/, "");
    this.baseUrl = parsed;
  }

  private makeHeaders(idempotencyKey?: string, extra?: HeadersInit): Headers {
    const headers = new Headers(extra);
    headers.set("Accept", "application/json");
    if (this.bearerToken !== undefined) headers.set("Authorization", `Bearer ${this.bearerToken}`);
    if (idempotencyKey !== undefined) headers.set("Idempotency-Key", idempotencyKey);
    return headers;
  }

  private endpoint(path: string): URL {
    const prefix = this.baseUrl.pathname === "/" ? "" : this.baseUrl.pathname;
    return new URL(`${this.baseUrl.origin}${prefix}${path}`);
  }

  async request(method: string, pathOrParams: string | WireObject = "", body?: unknown, idempotencyKey?: string, extra?: HeadersInit, raw = false): Promise<unknown> {
    if (this.closed) throw unavailable("HTTP transport is closed");
    if (typeof pathOrParams !== "string") throw new MuzenError("internal", "invalid HTTP request path", false);
    const headers = this.makeHeaders(idempotencyKey, extra);
    let encoded: string | undefined;
    if (body !== undefined) {
      encoded = JSON.stringify(body);
      headers.set("Content-Type", "application/json");
    }
    let response: Response;
    try {
      response = await fetch(this.endpoint(pathOrParams), { method, headers, body: encoded });
    } catch (error) {
      throw unavailable(`HTTP request failed: ${String(error)}`);
    }
    if (!response.ok) throw await this.httpError(response);
    if (raw) return new Uint8Array(await response.arrayBuffer());
    if (response.status === 204) return undefined;
    const text = await response.text();
    if (text === "") return undefined;
    try { return JSON.parse(text) as unknown; } catch (error) { throw unavailable(`HTTP response contained invalid JSON: ${String(error)}`); }
  }

  private async httpError(response: Response): Promise<MuzenError> {
    let value: unknown;
    try { value = await response.json(); } catch { value = undefined; }
    if (isObject(value) && typeof value.code === "string" && typeof value.message === "string" && typeof value.retryable === "boolean") return wireError(value, `HTTP ${response.status}`);
    const code = STATUS_CODES[response.status] ?? "internal";
    return new MuzenError(code, `HTTP request failed with status ${response.status}`, response.status >= 500);
  }

  async *events(runId: string, after: number | undefined, signal: AbortSignal | undefined): AsyncIterable<AgentEvent> {
    if (this.closed) throw unavailable("HTTP transport is closed");
    let cursor = after;
    for (;;) {
      const path = `/v1/runs/${encodeURIComponent(runId)}/events${cursor === undefined ? "" : `?after=${cursor}`}`;
      const headers = this.makeHeaders();
      headers.set("Accept", "text/event-stream");
      if (cursor !== undefined) headers.set("Last-Event-ID", String(cursor));
      const controller = new AbortController();
      const abort = () => controller.abort(signal?.reason);
      if (signal?.aborted) abort();
      else signal?.addEventListener("abort", abort, { once: true });
      let connectTimedOut = false;
      let connectTimer: ReturnType<typeof setTimeout> | undefined;
      if (this.sseIdleTimeoutMs !== null) {
        connectTimer = setTimeout(() => {
          connectTimedOut = true;
          controller.abort();
        }, this.sseIdleTimeoutMs);
      }
      let response: Response;
      try {
        response = await fetch(this.endpoint(path), { headers, signal: controller.signal });
      } catch (error) {
        signal?.removeEventListener("abort", abort);
        if (connectTimedOut) throw unavailable("SSE connection exceeded the idle timeout");
        throw unavailable(`SSE transport failed: ${String(error)}`);
      } finally {
        if (connectTimer !== undefined) clearTimeout(connectTimer);
      }
      if (!response.ok) {
        signal?.removeEventListener("abort", abort);
        throw await this.httpError(response);
      }
      if (response.body === null) {
        signal?.removeEventListener("abort", abort);
        throw unavailable("SSE response has no body");
      }
      const reader = response.body.getReader();
      const parser = new SseParser();
      let reconnect = false;
      try {
        for (;;) {
          let read: ReadableStreamReadResult<Uint8Array>;
          let readTimedOut = false;
          let readTimer: ReturnType<typeof setTimeout> | undefined;
          if (this.sseIdleTimeoutMs !== null) {
            readTimer = setTimeout(() => {
              readTimedOut = true;
              controller.abort();
            }, this.sseIdleTimeoutMs);
          }
          try {
            read = await reader.read();
          } catch (error) {
            if (readTimedOut) {
              reconnect = true;
              break;
            }
            throw unavailable(`SSE transport failed: ${String(error)}`);
          } finally {
            if (readTimer !== undefined) clearTimeout(readTimer);
          }
          if (readTimedOut) {
            reconnect = true;
            break;
          }
          const payloads = parser.feed(read.value ?? new Uint8Array(), read.done);
          for (const payload of payloads) {
            let event: AgentEvent;
            try { event = JSON.parse(payload) as AgentEvent; } catch (error) { throw unavailable(`SSE event contained invalid JSON: ${String(error)}`); }
            if (cursor !== undefined && event.sequence <= cursor) continue;
            cursor = event.sequence;
            yield event;
            if (TERMINAL_EVENTS.has(event.type)) return;
          }
          if (read.done) throw unavailable("SSE stream ended before a terminal run event");
        }
      } finally {
        signal?.removeEventListener("abort", abort);
        await reader.cancel().catch(() => undefined);
      }
      if (!reconnect) return;
    }
  }

  async close(): Promise<void> { this.closed = true; }
}

class MuzenImpl implements Muzen {
  constructor(private readonly transport: Transport) {}
  async capabilities(): Promise<Capabilities> { return await this.call("muzen.capabilities", "GET", "/v1/capabilities") as Capabilities; }
  async putSecret(input: PutSecretInput): Promise<SecretRef> { return await this.call("secret.put", "POST", "/v1/secrets", { ...input }, input, input.idempotencyKey) as string; }
  async deleteSecret(secret: SecretRef): Promise<void> { await this.call("secret.delete", "DELETE", `/v1/secrets/${encodeURIComponent(secret)}`, { secret }); }
  async createSession(spec: SessionSpec, options?: CreateOptions): Promise<AgentSession> {
    const id = await this.call("session.create", "POST", "/v1/sessions", { spec, ...(options === undefined ? {} : { options }) }, spec, options?.idempotencyKey) as string;
    return new AgentSessionImpl(id, this.transport);
  }
  async getSession(id: string): Promise<AgentSession> { const session = new AgentSessionImpl(id, this.transport); await session.snapshot(); return session; }
  async startRun(spec: RunSpec): Promise<Run> { const id = await this.call("run.start", "POST", "/v1/runs", { spec }, spec, spec.idempotencyKey) as string; return new RunImpl(id, this.transport); }
  async getRun(id: string): Promise<Run> { const run = new RunImpl(id, this.transport); await run.snapshot(); return run; }
  async answerToolCall(runId: string, input: AnswerToolCallInput): Promise<void> {
    await this.call(
      "run.answer_tool_call",
      "POST",
      `/v1/runs/${encodeURIComponent(runId)}/tools/${encodeURIComponent(input.callId)}/result`,
      { runId, input },
      input.outcome,
    );
  }
  async close(): Promise<void> { await this.transport.close(); }
  async [Symbol.asyncDispose](): Promise<void> { await this.close(); }

  private call(rpc: string, method: string, path: string, rpcParams: WireObject = {}, httpBody?: unknown, key?: string): Promise<unknown> {
    return this.transport.kind === "runner" ? this.transport.request(rpc, rpcParams) : this.transport.request(method, path, httpBody, key);
  }
}

class AgentSessionImpl implements AgentSession {
  constructor(readonly id: string, private readonly transport: Transport) {}
  async snapshot(): Promise<SessionSnapshot> { return await this.call("session.get", "GET", `/v1/sessions/${encodeURIComponent(this.id)}`, { sessionId: this.id }) as SessionSnapshot; }
  async messages(options?: { after?: string; limit?: number }): Promise<Page<AgentMessage>> {
    const page = options ?? {};
    const query = new URLSearchParams();
    if (page.after !== undefined) query.set("after", page.after);
    if (page.limit !== undefined) query.set("limit", String(page.limit));
    const suffix = query.size === 0 ? "" : `?${query}`;
    const params: WireObject = { sessionId: this.id };
    if (options !== undefined) params.page = page;
    return await this.call("session.messages", "GET", `/v1/sessions/${encodeURIComponent(this.id)}/messages${suffix}`, params) as Page<AgentMessage>;
  }
  async run(input: AgentInputLike, options: SingleRunOptions): Promise<Run> {
    const normalized = normalizeAgentInput(input);
    const spec: RunSpec = { roots: [{ sessionId: this.id, input: normalized }], limits: options.limits, ...(options.idempotencyKey === undefined ? {} : { idempotencyKey: options.idempotencyKey }), ...(options.metadata === undefined ? {} : { metadata: options.metadata }) };
    const httpBody = { input: normalized, options };
    const id = await this.call("run.start", "POST", `/v1/sessions/${encodeURIComponent(this.id)}/runs`, { spec }, httpBody, options.idempotencyKey) as string;
    return new RunImpl(id, this.transport);
  }
  async archive(options?: CommandOptions): Promise<void> {
    const params: WireObject = { sessionId: this.id };
    if (options !== undefined) params.options = options;
    await this.call("session.archive", "POST", `/v1/sessions/${encodeURIComponent(this.id)}/archive`, params, options ?? {}, options?.idempotencyKey);
  }
  private call(rpc: string, method: string, path: string, rpcParams: WireObject, httpBody?: unknown, key?: string): Promise<unknown> {
    return this.transport.kind === "runner" ? this.transport.request(rpc, rpcParams) : this.transport.request(method, path, httpBody, key);
  }
}

class RunImpl implements Run {
  constructor(readonly id: string, private readonly transport: Transport) {}
  async snapshot(): Promise<RunSnapshot> { return await this.call("run.get", "GET", `/v1/runs/${encodeURIComponent(this.id)}`, { runId: this.id }) as RunSnapshot; }
  events(options?: EventOptions): AsyncIterable<AgentEvent> { return this.transport.events(this.id, options?.after, options?.signal); }
  async wait(): Promise<RunResult> {
    const durable = await this.result();
    if (durable !== undefined) return durable;
    for await (const event of this.events()) if (TERMINAL_EVENTS.has(event.type)) break;
    const result = await this.result();
    if (result === undefined) throw new MuzenError("internal", "run event stream ended without a durable result", false);
    return result;
  }
  async result(): Promise<RunResult | undefined> {
    const value = await this.call("run.result", "GET", `/v1/runs/${encodeURIComponent(this.id)}/result`, { runId: this.id });
    return value === null || value === undefined ? undefined : value as RunResult;
  }
  async send(command: SendCommand): Promise<CommandReceipt> { return await this.call("run.send", "POST", `/v1/runs/${encodeURIComponent(this.id)}/send`, { runId: this.id, command }, command, command.idempotencyKey) as CommandReceipt; }
  async spawn(command: SpawnCommand): Promise<AgentSession> { const id = await this.call("run.spawn", "POST", `/v1/runs/${encodeURIComponent(this.id)}/spawn`, { runId: this.id, command }, command, command.idempotencyKey) as string; return new AgentSessionImpl(id, this.transport); }
  async cancel(options?: CancelOptions): Promise<CommandReceipt> { const params: WireObject = { runId: this.id }; if (options !== undefined) params.options = options; return await this.call("run.cancel", "POST", `/v1/runs/${encodeURIComponent(this.id)}/cancel`, params, options ?? {}, options?.idempotencyKey) as CommandReceipt; }
  async artifact(id: ArtifactId): Promise<Artifact> {
    const result = await this.result();
    const ref = result?.artifacts.find((item) => item.id === id);
    if (ref === undefined) throw new MuzenError("not_found", `artifact not found: ${id}`, false);
    return new ArtifactImpl(ref, this.id, this.transport);
  }
  private call(rpc: string, method: string, path: string, rpcParams: WireObject, httpBody?: unknown, key?: string): Promise<unknown> {
    return this.transport.kind === "runner" ? this.transport.request(rpc, rpcParams) : this.transport.request(method, path, httpBody, key);
  }
}

class ArtifactImpl implements Artifact {
  readonly data: AsyncIterable<Uint8Array>;
  constructor(readonly ref: ArtifactRef, runId: string, transport: Transport) { this.data = this.read(runId, transport); }
  private async *read(runId: string, transport: Transport): AsyncIterable<Uint8Array> {
    let offset = 0;
    for (;;) {
      let chunk: Uint8Array;
      let eof: boolean;
      if (transport.kind === "runner") {
        const value = await transport.request("artifact.read", { artifactId: this.ref.id, offset, maxBytes: ARTIFACT_CHUNK_BYTES });
        if (!isObject(value) || typeof value.data !== "string" || typeof value.eof !== "boolean") throw new MuzenError("internal", "invalid artifact.read response", false);
        try { chunk = Uint8Array.from(Buffer.from(value.data, "base64")); } catch (error) { throw new MuzenError("internal", `artifact chunk contains invalid base64: ${String(error)}`, false); }
        eof = value.eof;
      } else {
        chunk = await transport.request("GET", `/v1/runs/${encodeURIComponent(runId)}/artifacts/${encodeURIComponent(this.ref.id)}`, undefined, undefined, { Range: `bytes=${offset}-${offset + ARTIFACT_CHUNK_BYTES - 1}` }, true) as Uint8Array;
        eof = offset + chunk.byteLength >= this.ref.bytes;
      }
      if (chunk.byteLength === 0 && !eof) throw new MuzenError("internal", "artifact transport returned an empty non-terminal chunk", false);
      if (chunk.byteLength > 0) { yield chunk; offset += chunk.byteLength; }
      if (eof) return;
    }
  }
}

export interface LocalRunnerOptions {
  store: "memory" | "sqlite";
  sqlitePath?: string;
  allowLoopbackHttp?: boolean;
  binaryPath?: string;
  closeTimeoutMs?: number;
}

export async function connectLocalRunner(options: LocalRunnerOptions): Promise<Muzen> {
  if (options.store === "sqlite" && !options.sqlitePath) throw new MuzenError("invalid_input", "sqlitePath is required for sqlite store", false);
  if (options.store === "memory" && options.sqlitePath !== undefined) throw new MuzenError("invalid_input", "sqlitePath requires sqlite store", false);
  const closeTimeoutMs = options.closeTimeoutMs ?? 5_000;
  if (!Number.isFinite(closeTimeoutMs) || closeTimeoutMs <= 0) throw new MuzenError("invalid_input", "closeTimeoutMs must be positive", false);
  const executable = options.binaryPath ?? process.env.MUZEN_AGENT_RUNNER_BIN ?? "muzen-agent-runner";
  const args = ["--store", options.store];
  if (options.sqlitePath !== undefined) args.push("--db", options.sqlitePath);
  if (options.allowLoopbackHttp === true) args.push("--allow-loopback-http");
  const child = spawn(executable, args, { stdio: ["pipe", "pipe", "pipe"] });
  child.stderr.pipe(process.stderr);
  await new Promise<void>((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", (error) => reject(unavailable(`failed to start local runner: ${error.message}`)));
  });
  return new MuzenImpl(new RunnerTransport(child, closeTimeoutMs));
}

export interface HttpOptions {
  bearerToken?: string;
  sseIdleTimeoutMs?: number | null;
}

export async function connectHttp(baseUrl: string, options: HttpOptions = {}): Promise<Muzen> {
  return new MuzenImpl(new HttpTransport(
    baseUrl,
    options.bearerToken,
    options.sseIdleTimeoutMs === undefined ? 30_000 : options.sseIdleTimeoutMs,
  ));
}
