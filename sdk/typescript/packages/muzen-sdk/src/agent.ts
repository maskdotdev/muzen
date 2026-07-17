export type SessionId = string;
export type RunId = string;
export type ArtifactId = string;
export type SecretRef = string;
export type ModelProfileId = string;
export type ToolProviderId = string;
export type AgentName = string;
export type IdempotencyKey = string;
export type JsonObject = Record<string, unknown>;
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "artifact"; artifactId: ArtifactId }
  | { type: "image"; mediaType: string; data: string };

export interface AgentInput {
  content: ContentBlock[];
}

export type AgentInputLike = AgentInput | string;

export interface AgentDefinition {
  name: AgentName;
  instructions: ContentBlock[];
  model: ModelProfileId;
  tools: ToolGrant[];
  budget?: AgentBudget;
  output?: OutputContract;
  metadata?: JsonObject;
}

export interface AgentBudget {
  maxTurns: number;
  maxToolCalls: number;
  maxPromptTokens: number;
  maxOutputTokens: number;
}

export interface OutputContract {
  schema: boolean | JsonObject;
  name?: string;
}

export interface ModelProfile {
  id: ModelProfileId;
  provider: "openai_compatible" | "anthropic";
  protocol: "responses" | "chat_completions" | "messages";
  model: string;
  baseUrl?: string;
  credential: SecretRef;
  maxInputTokens: number;
  maxOutputTokens: number;
  temperature?: number;
  topP?: number;
}

export type HeaderValue = string | { secret: SecretRef };

export type ToolProvider =
  | { id: ToolProviderId; kind: "builtin" }
  | {
      id: ToolProviderId;
      kind: "mcp_http";
      url: string;
      credential?: SecretRef;
      headers?: Record<string, HeaderValue>;
    };

export interface ToolGrant {
  provider: ToolProviderId;
  tool: string;
  effects: ToolEffect[];
  maxCalls?: number;
}

export type ToolEffect =
  | "workspace_read"
  | "workspace_write"
  | "network_read"
  | "network_write"
  | "artifact_write"
  | "agent_spawn"
  | "agent_message";

export interface WorkspaceSpec {
  base: WorkspaceBase;
  limits?: WorkspaceLimits;
}

export type WorkspaceBase =
  | { kind: "path"; root: string }
  | { kind: "git"; url: string; revision: string; credential?: SecretRef }
  | { kind: "snapshot"; id: string };

export interface WorkspaceLimits {
  maxFiles?: number;
  maxFileBytes?: number;
  maxBaseBytes?: number;
  maxOverlayBytes?: number;
}

export interface SessionSpec {
  agent: AgentDefinition;
  models: ModelProfile[];
  toolProviders: ToolProvider[];
  workspace: WorkspaceSpec;
  sessionBudget?: SessionBudget;
  metadata?: JsonObject;
}

export interface SessionBudget {
  maxRuns?: number;
  maxLifetimeTokens?: number;
  maxLifetimeToolCalls?: number;
}

export interface RunSpec {
  roots: RunRoot[];
  limits: RunLimits;
  idempotencyKey?: IdempotencyKey;
  metadata?: JsonObject;
}

export type RunRoot =
  | { sessionId: SessionId; input: AgentInput }
  | { session: SessionSpec; input: AgentInput; idempotencyKey?: IdempotencyKey };

export interface RunLimits {
  maxActiveAgents: number;
  maxAgents: number;
  maxDepth: number;
  maxInputBytes: number;
  maxTotalTokens?: number;
  maxTotalToolCalls?: number;
  deadlineMs?: number;
}

export interface PutSecretInput {
  value: string;
  idempotencyKey?: IdempotencyKey;
}

export type ConnectionOptions =
  | {
      transport: "local_runner";
      executable?: string;
      allowLoopbackHttp?: boolean;
    }
  | { transport: "remote"; baseUrl: string; bearerToken?: string };

export interface CreateOptions {
  idempotencyKey?: IdempotencyKey;
}

export interface CommandOptions {
  idempotencyKey?: IdempotencyKey;
}

export interface SingleRunOptions {
  limits: RunLimits;
  idempotencyKey?: IdempotencyKey;
  metadata?: JsonObject;
}

export interface EventOptions {
  after?: number;
  signal?: AbortSignal;
}

export interface CancelOptions {
  reason?: string;
  idempotencyKey?: IdempotencyKey;
}

export interface Usage {
  inputTokens: number;
  outputTokens: number;
  toolCalls: number;
}

export type SessionStatus = "open" | "archived";
export type AgentStatus =
  | "queued"
  | "running"
  | "waiting"
  | "completed"
  | "failed"
  | "cancelled"
  | "budget_exhausted";
export type RunStatus =
  | "queued"
  | "running"
  | "waiting"
  | "completed"
  | "partial"
  | "failed"
  | "cancelled";

export interface SessionSnapshot {
  id: SessionId;
  status: SessionStatus;
  activeRunId?: RunId;
  createdAt: string;
  updatedAt: string;
  metadata: JsonObject;
}

export interface AgentSnapshot {
  sessionId: SessionId;
  parentSessionId?: SessionId;
  path: number[];
  status: AgentStatus;
  model: ModelProfileId;
  usage: Usage;
}

export interface RunSnapshot {
  id: RunId;
  status: RunStatus;
  roots: SessionId[];
  agents: AgentSnapshot[];
  lastSequence: number;
  createdAt: string;
  updatedAt: string;
}

export interface ExecutionError {
  code:
    | "model_error"
    | "tool_error"
    | "secret_unavailable"
    | "workspace_error"
    | "budget_exhausted"
    | "cancelled";
  message: string;
  retryable: boolean;
  details?: JsonObject;
}

export interface AgentOutput {
  sessionId: SessionId;
  path: number[];
  status: Extract<AgentStatus, "completed" | "failed" | "cancelled" | "budget_exhausted">;
  output?: JsonValue;
  usage: Usage;
  error?: ExecutionError;
}

export interface ArtifactRef {
  id: ArtifactId;
  mediaType: string;
  bytes: number;
}

export interface Artifact {
  ref: ArtifactRef;
  data: AsyncIterable<Uint8Array>;
}

export interface RunResult {
  runId: RunId;
  status: Extract<RunStatus, "completed" | "partial" | "failed" | "cancelled">;
  outputs: AgentOutput[];
  usage: Usage;
  artifacts: ArtifactRef[];
  metadata: JsonObject;
}

export interface AgentMessage {
  id: string;
  sessionId: SessionId;
  role: "system" | "user" | "assistant" | "tool";
  content: ContentBlock[];
  createdAt: string;
}

export interface Page<T> {
  items: T[];
  next?: string;
}

export interface AgentEvent {
  runId: RunId;
  sequence: number;
  type: string;
  timestamp: string;
  sessionId?: SessionId;
  payload: JsonObject;
}

export interface CommandReceipt {
  sequence: number;
}

export interface SendCommand {
  sessionId: SessionId;
  input: AgentInput;
  delivery: "steer" | "follow_up";
  idempotencyKey?: IdempotencyKey;
}

export interface SpawnCommand {
  parentSessionId: SessionId;
  agent: AgentDefinition;
  input: AgentInput;
  idempotencyKey?: IdempotencyKey;
}

export interface Capabilities {
  protocolVersion: string;
  workspaceBases: Array<"path" | "git" | "snapshot">;
  toolProviderKinds: Array<"builtin" | "mcp_http">;
  modelProtocols: Array<"responses" | "chat_completions" | "messages">;
  maxReplayBatch: number;
}

export interface Muzen extends AsyncDisposable {
  capabilities(): Promise<Capabilities>;
  putSecret(input: PutSecretInput): Promise<SecretRef>;
  deleteSecret(secret: SecretRef): Promise<void>;
  createSession(spec: SessionSpec, options?: CreateOptions): Promise<AgentSession>;
  getSession(id: SessionId): Promise<AgentSession>;
  startRun(spec: RunSpec): Promise<Run>;
  getRun(id: RunId): Promise<Run>;
  close(): Promise<void>;
}

export interface AgentSession {
  readonly id: SessionId;
  snapshot(): Promise<SessionSnapshot>;
  messages(options?: { after?: string; limit?: number }): Promise<Page<AgentMessage>>;
  run(input: AgentInputLike, options: SingleRunOptions): Promise<Run>;
  archive(options?: CommandOptions): Promise<void>;
}

export interface Run {
  readonly id: RunId;
  snapshot(): Promise<RunSnapshot>;
  events(options?: EventOptions): AsyncIterable<AgentEvent>;
  wait(): Promise<RunResult>;
  result(): Promise<RunResult | undefined>;
  send(command: SendCommand): Promise<CommandReceipt>;
  spawn(command: SpawnCommand): Promise<AgentSession>;
  cancel(options?: CancelOptions): Promise<CommandReceipt>;
  artifact(id: ArtifactId): Promise<Artifact>;
}

export type ErrorCode =
  | "invalid_input"
  | "not_found"
  | "conflict"
  | "unauthenticated"
  | "permission_denied"
  | "resource_exhausted"
  | "unsupported"
  | "unavailable"
  | "deadline_exceeded"
  | "internal";

export class MuzenError extends Error {
  constructor(
    readonly code: ErrorCode,
    message: string,
    readonly retryable: boolean,
    readonly details?: JsonObject,
  ) {
    super(message);
    this.name = "MuzenError";
  }
}

export function defineAgent(definition: AgentDefinition): Readonly<AgentDefinition> {
  requireNonEmpty(definition.name, "name");
  requireNonEmpty(definition.model, "model");
  if (definition.instructions.length === 0) {
    throw invalid("instructions", "must contain at least one content block");
  }
  definition.instructions.forEach((block, index) => validateBlock(block, `instructions[${index}]`));
  definition.tools.forEach((grant, index) => {
    requireNonEmpty(grant.provider, `tools[${index}].provider`);
    requireNonEmpty(grant.tool, `tools[${index}].tool`);
    if (grant.maxCalls !== undefined) {
      requirePositiveInteger(grant.maxCalls, `tools[${index}].maxCalls`);
    }
  });
  if (definition.budget !== undefined) {
    requirePositiveInteger(definition.budget.maxTurns, "budget.maxTurns");
    requireNonNegativeInteger(definition.budget.maxToolCalls, "budget.maxToolCalls");
    requirePositiveInteger(definition.budget.maxPromptTokens, "budget.maxPromptTokens");
    requirePositiveInteger(definition.budget.maxOutputTokens, "budget.maxOutputTokens");
  }
  if (
    definition.output !== undefined &&
    typeof definition.output.schema !== "boolean" &&
    !isObject(definition.output.schema)
  ) {
    throw invalid("output.schema", "must be a JSON Schema boolean or object");
  }
  return Object.freeze({ ...definition });
}

export function normalizeAgentInput(input: AgentInputLike): AgentInput {
  if (typeof input === "string") {
    requireNonEmpty(input, "content[0].text");
    return { content: [{ type: "text", text: input }] };
  }
  if (input.content.length === 0) {
    throw invalid("content", "must contain at least one block");
  }
  input.content.forEach((block, index) => validateBlock(block, `content[${index}]`));
  return input;
}

function validateBlock(block: ContentBlock, path: string): void {
  if (!isObject(block)) {
    throw invalid(path, "must be an object");
  }
  if (block.type === "text") {
    requireNonEmpty(block.text, `${path}.text`);
  } else if (block.type === "artifact") {
    requireNonEmpty(block.artifactId, `${path}.artifactId`);
  } else if (block.type === "image") {
    requireNonEmpty(block.mediaType, `${path}.mediaType`);
    requireNonEmpty(block.data, `${path}.data`);
  } else {
    throw invalid(`${path}.type`, "is unknown");
  }
}

function requireNonEmpty(value: string, path: string): void {
  if (value.trim().length === 0) {
    throw invalid(path, "must not be empty");
  }
}

function requirePositiveInteger(value: number, path: string): void {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw invalid(path, "must be a positive safe integer");
  }
}

function requireNonNegativeInteger(value: number, path: string): void {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw invalid(path, "must be a non-negative safe integer");
  }
}

function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function invalid(path: string, message: string): MuzenError {
  return new MuzenError("invalid_input", `${path} ${message}`, false, { path });
}

export {
  connectHttp,
  connectLocalRunner,
  type HttpOptions,
  type LocalRunnerOptions,
} from "./agent-transports.js";

export {
  Agent,
  AgentConversation,
  AgentResult,
  discoverLocalRunnerBinary,
  type AgentOptions,
  type AgentRunOptions,
} from "./agent-facade.js";

export {
  tool,
  type FunctionToolOptions,
  type Tool,
  type ToolInputSchema,
} from "./tools.js";
