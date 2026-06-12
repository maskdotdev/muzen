export type ReviewStatus =
  | "created"
  | "queued"
  | "running"
  | "completed"
  | "failed"
  | "cancelled";

export type ReviewConclusion = "approved" | "commented" | "changes_requested";

export type ReviewRole =
  | "generalist"
  | "security"
  | "performance"
  | "maintainability"
  | "correctness"
  | "architecture"
  | "validator";

export type ReviewSource =
  | LocalReviewSource
  | RawSnapshotReviewSource
  | GithubPullRequestSource
  | GitlabMergeRequestSource
  | PerforceChangelistSource
  | CustomReviewSource;

export type ReviewSourceLike = ReviewSource | string;

export interface LocalReviewSource {
  type: "local";
  repo: string;
  changedFiles?: string[];
}

export interface RawSnapshotReviewSource {
  type: "raw_snapshot";
  root: string;
  changedFiles?: string[];
}

export interface GithubPullRequestSource {
  type: "github_pull_request";
  owner: string;
  repo: string;
  number: number;
}

export interface GitlabMergeRequestSource {
  type: "gitlab_merge_request";
  owner: string;
  repo: string;
  number: number;
}

export interface PerforceChangelistSource {
  type: "perforce_changelist";
  server: string;
  changelist: string;
  client?: string;
  depotPaths?: string[];
}

export interface CustomReviewSource {
  type: "custom";
  provider: string;
  id: string;
}

export type DedupePolicy =
  | "none"
  | "source"
  | "source_head"
  | { key: string };

export interface ReviewOptions {
  dedupe?: DedupePolicy;
  cancelSuperseded?: boolean;
  model?: ReviewModelSpec;
  sourceProvider?: ReviewSourceProvider;
  hooks?: ReviewHooks;
  heartbeat?: ReviewHeartbeatOptions;
  signal?: AbortSignal;
  change?: ReviewChangeSpec;
  scope?: ReviewScope;
  metadata?: Record<string, unknown>;
  instructions?: ReviewInstruction[];
  tools?: ReviewTool[];
  sessions?: ReviewAgentSession[];
  limits?: ReviewLimits;
}

export interface ReviewSourceProvider {
  baseUrl?: string;
  handler?: ReviewSourceMaterializeHandler;
}

export type ReviewSourceMaterializeHandler = (
  request: ReviewSourceMaterializeRequest,
) => ReviewSourceMaterializeResult | Promise<ReviewSourceMaterializeResult>;

export interface ReviewSourceMaterializeRequest {
  source: ReviewSource;
  changedFiles: string[];
  signal?: AbortSignal;
}

export interface ReviewSourceMaterializeResult {
  root: string;
  changedFiles?: string[];
}

export interface ReviewHooks {
  onEvent?: ReviewEventHandler;
  onHeartbeat?: ReviewHeartbeatHandler;
}

export type ReviewEventHandler = (event: ReviewEvent) => void;

export interface ReviewHeartbeatOptions {
  intervalMs?: number;
  leaseSeconds?: number;
}

export type ReviewHeartbeatHandler = (
  heartbeat: ReviewHeartbeat,
) => boolean | { continueRun?: boolean } | void | Promise<boolean | { continueRun?: boolean } | void>;

export interface ReviewHeartbeat {
  runId: string;
  sequence: number;
  elapsedMs: number;
  leaseSeconds?: number;
  signal?: AbortSignal;
}

export type ReviewModelSpec = ReviewCallbackModelSpec | ReviewHostedModelSpec;

export interface ReviewCallbackModelSpec {
  kind: "callback";
  handler: ReviewModelHandler;
}

export interface OpenAIReviewModelSpec {
  kind: "provider";
  provider: "openai";
  model: string;
  credential?: ReviewModelCredential;
  baseUrl?: string;
  apiProtocol?: "responses" | "chat_completions";
  maxInputTokens?: number;
  maxOutputTokens?: number;
  temperature?: number;
  topP?: number;
}

export type ReviewHostedModelSpec = OpenAIReviewModelSpec;

export type ReviewModelCredential =
  | { env: string }
  | { secretRef: string };

export type ReviewModelHandler = (
  request: ReviewModelRequest,
) => ReviewModelResult | Promise<ReviewModelResult>;

export interface ReviewModelRequest {
  runId: string;
  sessionId: string;
  role: ReviewRole;
  objective: string;
  snapshotId?: string;
  modelProfileId?: string;
  turn: number;
  transcript: unknown[];
  signal?: AbortSignal;
}

export interface ReviewModelResult {
  content?: string;
  toolCalls?: ReviewModelToolCall[];
  usage?: ReviewTokenUsage;
}

export interface ReviewModelToolCall {
  callId?: string;
  toolId: string;
  arguments?: unknown;
}

export interface ReviewTokenUsage {
  inputTokens: number;
  outputTokens: number;
  totalTokens: number;
}

export interface ReviewChangeSpec {
  kind: "revision_range" | "snapshot_pair" | "diff" | "provider_review" | string;
  baseRevision?: string | null;
  startRevision?: string | null;
  headRevision?: string | null;
  changedFiles?: ReviewChangedFile[];
  diff?: string | null;
  reviewTarget?: string | null;
  metadata?: Record<string, unknown>;
}

export interface ReviewChangedFile {
  path: string;
  status?: string;
}

export interface ReviewInstruction {
  kind:
    | "host_policy"
    | "organization_policy"
    | "repository_policy"
    | "session_objective"
    | "provider_context"
    | string;
  text: string;
  trusted?: boolean;
}

export type JsonSchema = boolean | Record<string, unknown>;

export interface ReviewTool {
  id: string;
  description: string;
  parameters: JsonSchema;
  effects?: ReviewToolEffect[];
  cacheable?: boolean;
  providerResources?: string[];
  handler?: ReviewToolHandler;
}

export type ReviewToolEffect =
  | "read_repo"
  | "read_diff"
  | "read_artifact"
  | "read_host"
  | "read_network"
  | "read_scratch"
  | "write_artifact"
  | "write_scratch";

export type ReviewToolHandler = (
  context: ReviewToolExecutionContext,
  arguments_: unknown,
) => ReviewToolResult | Promise<ReviewToolResult>;

export interface ReviewToolExecutionContext {
  runId: string;
  sessionId: string;
  turn: number;
  callId: string;
  toolId: string;
  snapshotId: string;
  providerResources: string[];
  signal?: AbortSignal;
}

export interface ReviewToolResult {
  data?: unknown;
  artifact?: ReviewToolArtifact;
}

export interface ReviewToolArtifact {
  key: string;
  content: string;
}

export interface ReviewScope {
  files?: string[];
  include?: string[];
  exclude?: string[];
}

export interface ReviewAgentSession {
  id: string;
  role: ReviewRole;
  objective: string;
  cwd?: string;
  model?: ReviewModelSpec;
  instructions?: ReviewInstruction[];
  toolGrants?: string[];
  budget?: ReviewAgentBudget;
}

export interface ReviewAgentBudget {
  maxTurns: number;
  maxToolCalls: number;
  maxPromptTokens: number;
  maxOutputTokens: number;
}

export interface ReviewLimits {
  maxActiveSessions?: number;
  maxFileBytes?: number;
  maxSearchMatches?: number;
}

/** One agent in a swarm run. Unlike review sessions, agents carry no review
 * role or findings semantics; just an objective, optional instructions, and
 * the tools/model/budget they run with. */
export interface SwarmAgent {
  id: string;
  objective: string;
  instructions?: ReviewInstruction[];
  model?: ReviewModelSpec;
  toolGrants?: string[];
  budget?: ReviewAgentBudget;
}

export interface SwarmOptions {
  agents: SwarmAgent[];
  /** Root directory the agents' repo tools see (snapshotted at start). */
  repo: string;
  /** Paths surfaced to agents as the working set; defaults to none. */
  files?: string[];
  /** Default model for agents without their own. */
  model?: ReviewModelSpec;
  tools?: ReviewTool[];
  limits?: ReviewLimits;
  hooks?: ReviewHooks;
  metadata?: Record<string, unknown>;
  signal?: AbortSignal;
}

export type SwarmAgentStatus = "done" | "failed" | "cancelled" | "partial";

/** "partial" means the run finished but not every agent completed; the
 * per-agent outputs say which. */
export type SwarmRunStatus = "completed" | "partial" | "failed" | "cancelled";

export interface SwarmAgentOutput {
  agentId: string;
  status: SwarmAgentStatus;
  completed: boolean;
  /** The agent's final text turn; undefined when the agent did not finish. */
  output?: string;
}

export interface SwarmUsage {
  agents: number;
  completedAgents: number;
  modelCalls: number;
  toolCalls: number;
  inputTokens: number;
  outputTokens: number;
  totalTokens: number;
}

export interface SwarmResult {
  runId: string;
  status: SwarmRunStatus;
  outputs: SwarmAgentOutput[];
  usage: SwarmUsage;
  metadata?: Record<string, unknown>;
}

export type ModelProviderKind =
  | "openai"
  | "anthropic"
  | "openai_compatible";

export type SourceProviderKind = "github" | "gitlab";

export interface ModelProfileInput {
  provider: ModelProviderKind;
  model: string;
  secretRef?: string;
  baseUrl?: string;
  routing?: Record<string, string>;
}

export interface ProviderProfileInput {
  provider: SourceProviderKind;
  secretRef?: string;
  baseUrl?: string;
  routing?: Record<string, string>;
}

export interface ModelProfile {
  workspaceId: string;
  name: string;
  version: string;
  provider: ModelProviderKind;
  model: string;
  secretRef?: string;
  baseUrl?: string;
  routing?: Record<string, string>;
  updatedAtUtc: string;
}

export interface ProviderProfile {
  workspaceId: string;
  name: string;
  version: string;
  provider: SourceProviderKind;
  secretRef?: string;
  baseUrl?: string;
  routing?: Record<string, string>;
  updatedAtUtc: string;
}

export interface WorkspaceProfileCollection<Input, Profile> {
  set(name: string, input: Input): Promise<Profile>;
  get(name: string): Promise<Profile | undefined>;
  list(): Promise<Profile[]>;
}

export interface MuzenWorkspace {
  readonly id: string;
  readonly models: WorkspaceProfileCollection<ModelProfileInput, ModelProfile>;
  readonly providers: WorkspaceProfileCollection<ProviderProfileInput, ProviderProfile>;
  review(source: ReviewSourceLike, options?: ReviewOptions): Promise<ReviewSession>;
}

export interface ReviewSessionSnapshot {
  id: string;
  status: ReviewStatus;
  source: ReviewSource;
  result?: ReviewResult;
}

export interface ReviewResult {
  reviewId: string;
  sessionId: string;
  status: ReviewStatus;
  conclusion: ReviewConclusion;
  summary: string;
  findings: ReviewFinding[];
  coverage: ReviewCoverage;
  metadata?: Record<string, unknown>;
}

export interface ReviewFinding {
  id: string;
  severity: "info" | "warning" | "error";
  category:
    | "bug"
    | "security"
    | "performance"
    | "maintainability"
    | "style"
    | "test"
    | "docs"
    | "other";
  title: string;
  message: string;
  location?: ReviewFindingLocation;
  suggestedFix?: ReviewSuggestedFix;
  confidence?: number;
  validationStatus?: string;
  evidence?: ReviewFindingEvidence[];
  discoveredBy?: string[];
  validatedBy?: string[];
  challengedBy?: string[];
}

export interface ReviewFindingEvidence {
  evidenceId: string;
  artifactId: string;
  kind: string;
  contentHash: string;
  producingToolCallId: string;
}

export interface ReviewFindingLocation {
  path: string;
  revision?: "base" | "head" | string;
  startLine?: number;
  endLine?: number;
  startColumn?: number;
  endColumn?: number;
  side?: "base" | "head" | "additions" | "deletions" | string;
  providerAnchor?: Record<string, unknown>;
}

export interface ReviewSuggestedFix {
  description?: string;
  patch?: string;
}

export interface ReviewCoverage {
  filesConsidered: number;
  filesReviewed: number;
  filesSkipped: number;
}

export type ReviewEventType =
  | "session.created"
  | "session.queued"
  | "session.started"
  | "source.resolved"
  | "scope.inferred"
  | "scope.overridden"
  | "repo.materialized"
  | "plan.created"
  | "agent.started"
  | "agent.completed"
  | "tool.started"
  | "tool.completed"
  | "finding.created"
  | "finding.updated"
  | "review.result_created"
  | "session.completed"
  | "session.failed"
  | "session.cancelled"
  | "runner.event";

export interface ReviewEvent {
  cursor: string;
  type: ReviewEventType;
  reviewId: string;
  timestampUtc: string;
  payload?: unknown;
}

export interface ReviewCancelOptions {
  reason?: string;
}

export type ReviewArtifactView = "redacted" | "raw";

export interface ReviewArtifactReadOptions {
  view?: ReviewArtifactView;
}

export interface ReviewArtifactExportOptions {
  view?: ReviewArtifactView;
  artifactIds?: string[];
  maxArtifacts?: number;
  maxBytes?: number;
}

export interface ReviewArtifactExport {
  view: ReviewArtifactView;
  artifactCount: number;
  totalBytes: number;
  artifacts: ReviewArtifact[];
}

export interface ReviewArtifact {
  artifactId: string;
  bytes: number;
  contentHash: string;
  content: string;
}

export interface CreateMuzenOptions {
  runnerPath?: string;
  runnerArgs?: string[];
  clientName?: string;
  clientVersion?: string;
  secrets?: MuzenSecretResolverOptions;
}

export interface MuzenSecretResolverOptions {
  resolve: MuzenSecretResolver;
}

export type MuzenSecretResolver = (ref: string) => string | Promise<string>;

export interface CreateMuzenClientOptions {
  baseUrl: string;
  token?: string;
  fetch?: typeof fetch;
}

export interface CreateReviewSessionInput {
  source: ReviewSourceLike;
  options?: ReviewOptions;
  muzen?: CreateMuzenOptions;
}

export interface CreateReviewSessionResult {
  muzen: Muzen;
  review: ReviewSession;
}

export type MuzenWebhookProvider = "github" | "gitlab";

export interface MuzenWebhookResponseOptions {
  workspaceId?: string;
  secret?: string;
  review?: ReviewOptions;
  signal?: AbortSignal;
}

export interface MuzenWebhookHandler {
  response(
    request: Request,
    options?: MuzenWebhookResponseOptions,
  ): Promise<Response>;
}

export interface MuzenWebhooks {
  readonly github: MuzenWebhookHandler;
  readonly gitlab: MuzenWebhookHandler;
}

export interface ReviewRetryPolicy {
  maxAttempts: number;
  initialBackoffSeconds: number;
  maxBackoffSeconds: number;
}

export interface ReviewWorkerConcurrencyLimits {
  maxRunningGlobal?: number;
  maxRunningPerWorkspace?: number;
  maxRunningPerUser?: number;
  maxRunningPerModelProfile?: number;
  maxRunningPerProviderProfile?: number;
}

export interface HostSchedulingConfiguration {
  leaseSeconds?: number;
  defaultRetryPolicy?: ReviewRetryPolicy;
  concurrency?: ReviewWorkerConcurrencyLimits;
  fairness?: "fifo" | "round_robin_by_workspace";
}

export interface HostConfiguration {
  scheduling?: HostSchedulingConfiguration;
}

export interface MuzenWorkerRun {
  workerId: string;
  claimed: number;
  completed: number;
  retried: number;
  failed: number;
  skipped: number;
}

export interface MuzenWorkerRunOnceOptions {
  workerId?: string;
  maxSessions?: number;
  hostConfig?: HostConfiguration;
  signal?: AbortSignal;
}

export interface MuzenWorkerStartOptions extends MuzenWorkerRunOnceOptions {
  queues?: string[];
  concurrency?: number;
  pollIntervalMs?: number;
  stopWhenIdle?: boolean;
}

export interface MuzenWorkers {
  runOnce(options?: MuzenWorkerRunOnceOptions): Promise<MuzenWorkerRun>;
  start(options?: MuzenWorkerStartOptions): Promise<void>;
}

export interface Muzen {
  readonly webhooks: MuzenWebhooks;
  readonly workers: MuzenWorkers;
  review(source: ReviewSourceLike, options?: ReviewOptions): Promise<ReviewSession>;
  runSwarm(options: SwarmOptions): Promise<SwarmResult>;
  workspace(id: string): MuzenWorkspace;
  resumeReview(id: string): Promise<ReviewSession>;
  createReviewSession(input: {
    source: ReviewSourceLike;
    options?: ReviewOptions;
  }): Promise<ReviewSession>;
  close(): Promise<void>;
}

export interface ReviewSession {
  readonly id: string;
  readonly status: ReviewStatus;
  readonly source: ReviewSource;
  subscribe(
    listener: (event: ReviewEvent) => void,
    options?: { replay?: boolean },
  ): () => void;
  events(options?: {
    after?: string | null;
    signal?: AbortSignal;
  }): AsyncIterable<ReviewEvent>;
  eventsResponse(options?: {
    after?: string | null;
    signal?: AbortSignal;
  }): Response;
  wait(options?: {
    timeout?: string | number;
    signal?: AbortSignal;
  }): Promise<ReviewResult>;
  result(): Promise<ReviewResult | undefined>;
  readArtifact(
    artifactId: string,
    options?: ReviewArtifactReadOptions,
  ): Promise<ReviewArtifact>;
  exportArtifacts(
    options?: ReviewArtifactExportOptions,
  ): Promise<ReviewArtifactExport>;
  cancel(reason?: string | ReviewCancelOptions): Promise<void>;
  refresh(): Promise<ReviewSessionSnapshot>;
}
