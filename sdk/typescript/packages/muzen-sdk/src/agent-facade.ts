import { accessSync, constants, mkdtempSync } from "node:fs";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  MuzenError,
  connectHttp,
  connectLocalRunner,
  type AgentBudget,
  type AgentInputLike,
  type AgentOutput,
  type AgentSession,
  type AnswerToolCallOutcome,
  type ContentBlock,
  type JsonObject,
  type JsonValue,
  type Muzen,
  type Run,
  type RunLimits,
  type RunResult,
  type SessionSpec,
  type Usage,
} from "./agent.js";
import { LoopbackToolServer, tool, type Tool } from "./tools.js";

const MODEL_ID = "default";
const BUILTIN_PROVIDER_ID = "builtin";
const LOCAL_TOOLS_PROVIDER_ID = "local_tools";
const DEFAULT_MAX_INPUT_TOKENS = 128_000;
const DEFAULT_MAX_OUTPUT_TOKENS = 4_096;
const MAX_EVENT_RESUME_ATTEMPTS = 3;

export interface AgentOptions {
  instructions?: string | ContentBlock | readonly ContentBlock[];
  model?: string;
  output?: JsonObject;
  canSpawn?: boolean;
  canMessage?: boolean;
  tools?: readonly Tool<any>[];
  spec?: SessionSpec;
  client?: Muzen;
  transport?: "local_runner" | "http";
  apiKey?: string;
  baseUrl?: string;
  modelBaseUrl?: string;
  bearerToken?: string;
  temperature?: number;
  maxOutputTokens?: number;
  maxTotalTokens?: number;
  deadlineMs?: number;
  budget?: AgentBudget;
}

export interface AgentRunOptions {
  limits?: RunLimits;
}

export class AgentResult<TOutput = string> {
  constructor(
    readonly text: string,
    readonly output: TOutput,
    readonly usage: Usage,
    readonly status: AgentOutput["status"],
    readonly runId: string,
    readonly raw: RunResult,
  ) {}

  raiseForStatus(): this {
    if (this.status === "completed") return this;
    const failedOutput = this.raw.outputs.find((output) => output.status === this.status);
    const error = failedOutput?.error;
    const message = error?.message ?? `agent ended with status ${this.status}`;
    const code = this.status === "budget_exhausted"
      ? "resource_exhausted"
      : this.status === "cancelled"
        ? "conflict"
        : "internal";
    throw new MuzenError(code, message, error?.retryable ?? false, {
      status: this.status,
      ...(error === undefined ? {} : { executionCode: error.code }),
    });
  }
}

export class Agent<TOutput = string> implements AsyncDisposable {
  private client: Muzen | undefined;
  private connectionPromise: Promise<Muzen> | undefined;
  private secretPromise: Promise<string> | undefined;
  private readonly transport: "local_runner" | "http";
  private readonly serviceBaseUrl: string | undefined;
  private readonly bearerToken: string | undefined;
  private readonly apiKey: string | undefined;
  private readonly tools: readonly Tool<any>[];
  private readonly toolServer: LoopbackToolServer | undefined;
  private readonly needsSecret: boolean;
  private readonly hasOutput: boolean;
  private specTemplate: SessionSpec;
  private readonly defaultLimits: RunLimits;
  private readonly tempDirectory: string | undefined;
  private closed = false;

  constructor(options: AgentOptions) {
    this.transport = options.transport ?? "local_runner";
    if (this.transport !== "local_runner" && this.transport !== "http") {
      throw invalid("transport", "must be 'local_runner' or 'http'");
    }
    if (this.transport === "http" && options.client === undefined && !options.baseUrl) {
      throw invalid("baseUrl", "is required for HTTP transport");
    }

    this.client = options.client;
    this.serviceBaseUrl = this.transport === "http" ? options.baseUrl : undefined;
    this.bearerToken = options.bearerToken;
    this.tools = (options.tools ?? []).map((item) => tool(item));
    const names = new Set(this.tools.map((item) => item.name));
    if (
      names.size !== this.tools.length ||
      (options.canSpawn === true && names.has("agent_spawn")) ||
      (options.canMessage === true && names.has("agent_message"))
    ) {
      throw invalid("tools", "must have unique function names");
    }
    this.toolServer = this.tools.length === 0 || this.transport === "http"
      ? undefined
      : new LoopbackToolServer(this.tools);

    if (options.spec !== undefined) {
      if (
        options.instructions !== undefined ||
        options.model !== undefined ||
        options.output !== undefined ||
        options.canSpawn === true ||
        options.canMessage === true ||
          options.apiKey !== undefined ||
          options.modelBaseUrl !== undefined ||
          options.temperature !== undefined ||
        options.maxOutputTokens !== undefined ||
        options.budget !== undefined ||
        options.tools !== undefined
      ) {
        throw invalid("spec", "cannot be combined with facade authoring options");
      }
      this.specTemplate = options.spec;
      this.needsSecret = false;
      this.hasOutput = options.spec.agent.output !== undefined;
    } else {
      if (options.instructions === undefined) throw invalid("instructions", "is required");
      if (options.model === undefined) throw invalid("model", "is required");
      const settings = modelSettings(options.model);
      const key = options.apiKey || process.env[settings.environment];
      if (!key) {
        throw invalid("apiKey", `is required when ${settings.environment} is not set`);
      }
      if (options.maxOutputTokens !== undefined && options.maxOutputTokens <= 0) {
        throw invalid("maxOutputTokens", "must be positive");
      }
      if (options.output !== undefined && !isJsonObject(options.output)) {
        throw invalid("output", "must be a JSON Schema object");
      }
      const instructions = coerceInstructions(options.instructions);

      this.apiKey = key;
      this.needsSecret = true;
      this.hasOutput = options.output !== undefined;
      this.tempDirectory = mkdtempSync(join(tmpdir(), "muzen-agent-"));
      const tools = [];
      if (options.canSpawn === true) {
        tools.push({ provider: BUILTIN_PROVIDER_ID, tool: "agent.spawn", effects: ["agent_spawn" as const] });
      }
      if (options.canMessage === true) {
        tools.push({ provider: BUILTIN_PROVIDER_ID, tool: "agent.message", effects: ["agent_message" as const] });
      }
      tools.push(...this.tools.map((item) => ({
        provider: LOCAL_TOOLS_PROVIDER_ID,
        tool: item.name,
        description: item.description,
        inputSchema: item.input,
        effects: [],
      })));
      this.specTemplate = {
        agent: {
          name: "agent",
          instructions,
          model: MODEL_ID,
          tools,
          ...(options.budget === undefined ? {} : { budget: options.budget }),
          ...(options.output === undefined ? {} : { output: { schema: options.output } }),
        },
        models: [{
          id: MODEL_ID,
          provider: settings.provider,
          protocol: settings.protocol,
          model: settings.model,
          credential: "pending",
          maxInputTokens: DEFAULT_MAX_INPUT_TOKENS,
          maxOutputTokens: options.maxOutputTokens ?? DEFAULT_MAX_OUTPUT_TOKENS,
          ...((this.transport === "local_runner" ? options.baseUrl : options.modelBaseUrl) !== undefined
            ? { baseUrl: this.transport === "local_runner" ? options.baseUrl : options.modelBaseUrl }
            : {}),
          ...(options.temperature === undefined ? {} : { temperature: options.temperature }),
        }],
        toolProviders: [
          ...(options.canSpawn === true || options.canMessage === true
            ? [{ id: BUILTIN_PROVIDER_ID, kind: "builtin" as const }]
            : []),
          ...(this.transport === "http" && this.tools.length > 0
            ? [{ id: LOCAL_TOOLS_PROVIDER_ID, kind: "client" as const }]
            : []),
        ],
        workspace: { base: { kind: "path", root: this.tempDirectory } },
      };
    }

    this.defaultLimits = {
      maxActiveAgents: 4,
      maxAgents: 16,
      maxDepth: 3,
      maxInputBytes: 1_048_576,
      ...(options.maxTotalTokens === undefined ? {} : { maxTotalTokens: options.maxTotalTokens }),
      ...(options.deadlineMs === undefined ? {} : { deadlineMs: options.deadlineMs }),
    };
  }

  async run(prompt: AgentInputLike, options: AgentRunOptions = {}): Promise<AgentResult<TOutput>> {
    const { client, spec } = await this.ready();
    const session = await client.createSession(spec);
    try {
      const run = await session.run(prompt, { limits: options.limits ?? this.defaultLimits });
      return this.result(await this.waitForRun(client, run), session.id);
    } finally {
      await archiveBestEffort(session);
    }
  }

  session(): AgentConversation<TOutput> {
    return new AgentConversation(
      () => this.ready().then(({ client, spec }) => client.createSession(spec)),
      (raw, sessionId) => this.result(raw, sessionId),
      (run) => this.connection().then((client) => this.waitForRun(client, run)),
      this.defaultLimits,
    );
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    try {
      let client = this.client;
      if (client === undefined && this.connectionPromise !== undefined) {
        try {
          client = await this.connectionPromise;
        } catch {
          // A failed connection still needs the local resources below cleaned up.
        }
      }
      await client?.close();
    } finally {
      try {
        await this.toolServer?.close();
      } finally {
        if (this.tempDirectory !== undefined) {
          await rm(this.tempDirectory, { recursive: true, force: true });
        }
      }
    }
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }

  private async ready(): Promise<{ client: Muzen; spec: SessionSpec }> {
    const client = await this.connection();
    if (!this.needsSecret) return { client, spec: this.specTemplate };
    this.secretPromise ??= client.putSecret({ value: Buffer.from(this.apiKey ?? "", "utf8").toString("base64") });
    const credential = await this.secretPromise;
    return {
      client,
      spec: {
        ...this.specTemplate,
        models: this.specTemplate.models.map((profile) =>
          profile.id === MODEL_ID ? { ...profile, credential } : profile),
      },
    };
  }

  private result(raw: RunResult, sessionId: string): AgentResult<TOutput> {
    const root = rootOutput(raw, sessionId);
    const value = root.output ?? null;
    const text = typeof value === "string" ? value : JSON.stringify(value);
    return new AgentResult(
      text,
      (this.hasOutput ? value : text) as TOutput,
      root.usage,
      root.status,
      raw.runId,
      raw,
    );
  }

  private async connection(): Promise<Muzen> {
    if (this.closed) throw new MuzenError("unavailable", "Agent is closed", false);
    this.connectionPromise ??= this.connect();
    try {
      this.client = await this.connectionPromise;
      return this.client;
    } catch (error) {
      await this.toolServer?.close();
      throw error;
    }
  }

  private async connect(): Promise<Muzen> {
    await this.startToolServer();
    if (this.closed) throw new MuzenError("unavailable", "Agent is closed", false);
    if (this.client !== undefined) return this.client;
    if (this.transport === "http") {
      return await connectHttp(this.serviceBaseUrl ?? "", { bearerToken: this.bearerToken });
    }
    return await connectLocalRunner({
      store: "memory",
      binaryPath: discoverLocalRunnerBinary(),
      allowLoopbackHttp: isLoopbackUrl(this.specTemplate.models[0]?.baseUrl) || this.tools.length > 0,
    });
  }

  private async startToolServer(): Promise<void> {
    if (this.toolServer === undefined) return;
    await this.toolServer.start();
    const provider = { id: LOCAL_TOOLS_PROVIDER_ID, kind: "mcp_http" as const, url: this.toolServer.url };
    this.specTemplate = {
      ...this.specTemplate,
      toolProviders: [
        ...this.specTemplate.toolProviders.filter((item) => item.id !== LOCAL_TOOLS_PROVIDER_ID),
        provider,
      ],
    };
  }

  private async waitForRun(client: Muzen, run: Run): Promise<RunResult> {
    if (this.transport !== "http" || this.tools.length === 0) return await run.wait();
    const controller = new AbortController();
    const wait = run.wait();
    const pump = pumpClientToolRun(client, run, this.tools, controller.signal);
    try {
      await Promise.race([wait.then(() => undefined), pump]);
      return await wait;
    } finally {
      controller.abort();
      await pump.catch(() => undefined);
    }
  }
}

export class AgentConversation<TOutput = string> implements AsyncDisposable {
  private readonly sessionPromise: Promise<AgentSession>;
  private sessionValue: AgentSession | undefined;
  private closed = false;

  constructor(
    createSession: () => Promise<AgentSession>,
    private readonly makeResult: (raw: RunResult, sessionId: string) => AgentResult<TOutput>,
    private readonly waitForRun: (run: Run) => Promise<RunResult>,
    private readonly defaultLimits: RunLimits,
  ) {
    this.sessionPromise = createSession();
  }

  async run(prompt: AgentInputLike, options: AgentRunOptions = {}): Promise<AgentResult<TOutput>> {
    if (this.closed) throw new MuzenError("conflict", "Agent session is closed", false);
    const session = await this.sharedSession();
    const run = await session.run(prompt, { limits: options.limits ?? this.defaultLimits });
    return this.makeResult(await this.waitForRun(run), session.id);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    const session = this.sessionValue ?? await this.sessionPromise;
    await archiveBestEffort(session);
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }

  private async sharedSession(): Promise<AgentSession> {
    this.sessionValue = await this.sessionPromise;
    return this.sessionValue;
  }
}

export async function pumpClientToolRun(
  client: Muzen,
  run: Run,
  tools: readonly Tool<any>[],
  signal: AbortSignal,
): Promise<void> {
  const toolsByName = new Map(tools.map((item) => [item.name, item]));
  const outcomes = new Map<string, Promise<AnswerToolCallOutcome>>();
  let after: number | undefined;
  let resumeFailures = 0;

  while (!signal.aborted) {
    try {
      for await (const event of run.events({ after, signal })) {
        if (event.type === "tool.requested" && event.payload.provider === LOCAL_TOOLS_PROVIDER_ID) {
          const callId = event.payload.callId;
          const name = event.payload.tool;
          if (typeof callId !== "string" || typeof name !== "string") {
            throw new MuzenError("internal", "tool.requested event is missing callId or tool", false);
          }
          let outcome = outcomes.get(callId);
          if (outcome === undefined) {
            outcome = executeClientTool(toolsByName.get(name), name, event.payload.arguments);
            outcomes.set(callId, outcome);
          }
          try {
            await client.answerToolCall(run.id, { callId, outcome: await outcome });
          } catch (error) {
            if (!isBenignToolAnswerError(error)) throw error;
          }
        }
        after = event.sequence;
        resumeFailures = 0;
        if (TERMINAL_RUN_EVENTS.has(event.type)) return;
      }
      if (!signal.aborted) {
        throw new MuzenError("unavailable", "run event stream ended before a terminal event", true);
      }
    } catch (error) {
      if (signal.aborted) return;
      if (!(error instanceof MuzenError) || !error.retryable || resumeFailures >= MAX_EVENT_RESUME_ATTEMPTS) {
        throw error;
      }
      resumeFailures += 1;
      await abortableDelay(25 * (2 ** (resumeFailures - 1)), signal);
    }
  }
}

const TERMINAL_RUN_EVENTS = new Set(["run.completed", "run.partial", "run.failed", "run.cancelled"]);

async function executeClientTool(
  selected: Tool<any> | undefined,
  name: string,
  arguments_: unknown,
): Promise<AnswerToolCallOutcome> {
  if (selected === undefined) {
    return { error: { message: `unknown client tool: ${name}`, retryable: false } };
  }
  if (!isJsonObject(arguments_)) {
    return { error: { message: `client tool ${name} arguments must be an object`, retryable: false } };
  }
  try {
    return { result: await selected.execute(arguments_) as JsonValue };
  } catch (error) {
    return {
      error: {
        message: error instanceof Error ? error.message : String(error),
        retryable: false,
      },
    };
  }
}

function isBenignToolAnswerError(error: unknown): boolean {
  return error instanceof MuzenError && (error.code === "conflict" || error.code === "not_found");
}

async function abortableDelay(milliseconds: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) return;
  await new Promise<void>((resolve) => {
    const timer = setTimeout(done, milliseconds);
    signal.addEventListener("abort", done, { once: true });
    function done(): void {
      clearTimeout(timer);
      signal.removeEventListener("abort", done);
      resolve();
    }
  });
}

export function discoverLocalRunnerBinary(): string | undefined {
  const configured = process.env.MUZEN_AGENT_RUNNER_BIN;
  if (configured) return configured;
  for (const directory of (process.env.PATH ?? "").split(delimiter)) {
    if (!directory) continue;
    const candidate = join(directory, "muzen-agent-runner");
    try {
      accessSync(candidate, constants.X_OK);
      return candidate;
    } catch {
      // Continue searching PATH.
    }
  }
  const repoBinary = fileURLToPath(new URL("../../../../../target/debug/muzen-agent-runner", import.meta.url));
  try {
    accessSync(repoBinary, constants.F_OK);
    return repoBinary;
  } catch {
    return undefined;
  }
}

function modelSettings(model: string): {
  provider: "anthropic" | "openai_compatible";
  protocol: "messages" | "chat_completions";
  model: string;
  environment: "ANTHROPIC_API_KEY" | "OPENAI_API_KEY";
} {
  if (typeof model !== "string" || model.trim().length === 0) {
    throw invalid("model", "must not be empty");
  }
  let name = model.trim();
  let override: "anthropic" | "openai" | undefined;
  const separator = name.indexOf(":");
  if (separator !== -1) {
    const prefix = name.slice(0, separator);
    if (prefix === "anthropic" || prefix === "openai") {
      override = prefix;
      name = name.slice(separator + 1);
      if (!name) throw invalid("model", "must include a name after the provider prefix");
    }
  }
  const anthropic = override === "anthropic" || (override === undefined && name.toLowerCase().startsWith("claude"));
  return anthropic
    ? { provider: "anthropic", protocol: "messages", model: name, environment: "ANTHROPIC_API_KEY" }
    : { provider: "openai_compatible", protocol: "chat_completions", model: name, environment: "OPENAI_API_KEY" };
}

function coerceInstructions(value: string | ContentBlock | readonly ContentBlock[]): ContentBlock[] {
  const blocks = typeof value === "string"
    ? [{ type: "text" as const, text: value }]
    : Array.isArray(value)
      ? [...value]
      : [value as ContentBlock];
  if (blocks.length === 0) throw invalid("instructions", "must contain at least one content block");
  for (const block of blocks) {
    if (!isContentBlock(block)) throw invalid("instructions", "must contain only content blocks");
    if (block.type === "text" && block.text.trim().length === 0) {
      throw invalid("instructions", "text blocks must not be empty");
    }
  }
  return blocks;
}

function isContentBlock(value: unknown): value is ContentBlock {
  if (!isJsonObject(value) || typeof value.type !== "string") return false;
  return (value.type === "text" && typeof value.text === "string") ||
    (value.type === "artifact" && typeof value.artifactId === "string") ||
    (value.type === "image" && typeof value.mediaType === "string" && typeof value.data === "string");
}

function rootOutput(raw: RunResult, sessionId: string): AgentOutput {
  const root = raw.outputs.find((output) => output.sessionId === sessionId && output.path.length === 0)
    ?? raw.outputs.find((output) => output.sessionId === sessionId)
    ?? raw.outputs[0];
  if (root === undefined) throw new MuzenError("internal", "run completed without an agent output", false);
  return root;
}

async function archiveBestEffort(session: AgentSession): Promise<void> {
  try {
    await session.archive();
  } catch {
    // Archival must not mask a run result or error.
  }
}

function isLoopbackUrl(value: string | undefined): boolean {
  if (!value) return false;
  try {
    return ["localhost", "127.0.0.1", "[::1]", "::1"].includes(new URL(value).hostname.toLowerCase());
  } catch {
    return false;
  }
}

function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function invalid(path: string, message: string): MuzenError {
  return new MuzenError("invalid_input", `${path} ${message}`, false, { path });
}
