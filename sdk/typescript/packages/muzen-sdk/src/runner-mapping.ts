import type { JsonRpcNotification } from "./protocol.js";
import { sourceKey } from "./sources.js";
import type {
  ReviewArtifact,
  ReviewChangeSpec,
  ReviewEvent,
  ReviewEventType,
  ReviewFinding,
  ReviewHostedModelSpec,
  ReviewLimits,
  ReviewModelSpec,
  ReviewOptions,
  ReviewResult,
  ReviewSource,
  ReviewStatus,
  ReviewTool,
  SwarmAgent,
  SwarmAgentOutput,
  SwarmAgentStatus,
  SwarmOptions,
  SwarmResult,
  SwarmRunStatus,
} from "./types.js";
import {
  isCallbackReviewModelSpec,
  isHostedReviewModelSpec,
} from "./models.js";

export function toRunnerStartParams(
  reviewId: string,
  source: ReviewSource,
  options: ReviewOptions,
): unknown {
  const changedFiles = options.scope?.files ?? [];
  const modelPlan = mapReviewModels(options);
  const params: Record<string, unknown> = {
    protocolVersion: "muzen.runner.v1",
    runId: reviewId,
    source,
    sourceProvider: mapReviewSourceProvider(options.sourceProvider),
    changedFiles,
    metadata: options.metadata ?? {},
    change: mapReviewChange(options.change),
    instructions: (options.instructions ?? []).map((instruction) => ({
      kind: instruction.kind,
      text: instruction.text,
      trusted: instruction.trusted ?? false,
    })),
    sessions: [],
    limits: mapReviewLimits(options.limits),
    model: modelPlan.runnerModel,
    tools: [],
    heartbeat: mapReviewHeartbeat(options),
  };
  if (source.type === "local") {
    params.repo = source.repo;
  }
  return params;
}

export function toSwarmStartParams(runId: string, options: SwarmOptions): unknown {
  if (options.agents.length === 0) {
    throw new Error("runSwarm requires at least one agent");
  }
  const modelPlan = mapModelPlan(options.model, options.agents);
  return {
    protocolVersion: "muzen.runner.v1",
    runId,
    repo: options.repo,
    source: { type: "local", repo: options.repo },
    changedFiles: options.files ?? [],
    metadata: options.metadata,
    instructions: [],
    sessions: options.agents.map((agent) => ({
      id: agent.id,
      role: "generalist",
      objective: agent.objective,
      cwd: undefined,
      modelProfileId: modelPlan.sessionProfileIds.get(agent.id),
      instructions: (agent.instructions ?? []).map((instruction) => ({
        kind: instruction.kind,
        text: instruction.text,
        trusted: instruction.trusted ?? false,
      })),
      toolGrants: agent.toolGrants ?? [],
      budget: agent.budget,
    })),
    limits: mapReviewLimits(options.limits),
    model: modelPlan.runnerModel,
    tools: mapRunnerTools(options.tools ?? []),
    heartbeat: mapHeartbeat(options),
  };
}

export function mapSwarmResult(runId: string, value: unknown): SwarmResult {
  if (!isRunnerRunResult(value)) {
    throw new Error("muzen-runner returned an invalid run result");
  }
  const summary = value.summary;
  const outputs = (value.sessionOutputs ?? []).map(mapSwarmAgentOutput);
  return {
    runId,
    status: mapSwarmRunStatus(value.status),
    outputs,
    usage: {
      agents: summary.sessions,
      completedAgents: summary.completedSessions,
      modelCalls: summary.modelCalls,
      toolCalls: summary.toolCalls,
      inputTokens: summary.inputTokens ?? 0,
      outputTokens: summary.outputTokens ?? 0,
      totalTokens: summary.totalTokens,
    },
    metadata: isRecord(value.metadata) ? value.metadata : undefined,
  };
}

function mapSwarmRunStatus(status: string): SwarmRunStatus {
  switch (status) {
    case "completed":
    case "partial":
    case "failed":
    case "cancelled":
      return status;
    default:
      return "failed";
  }
}

function mapSwarmAgentOutput(output: RunnerSessionOutput): SwarmAgentOutput {
  return {
    agentId: output.sessionId,
    status: mapSwarmAgentStatus(output.status),
    completed: output.completed,
    output: output.output ?? undefined,
  };
}

function mapSwarmAgentStatus(status: string): SwarmAgentStatus {
  switch (status) {
    case "done":
    case "failed":
    case "cancelled":
    case "partial":
      return status;
    default:
      return "failed";
  }
}

function mapReviewHeartbeat(options: ReviewOptions): unknown {
  return mapHeartbeat(options);
}

function mapHeartbeat(options: Pick<ReviewOptions, "hooks" | "heartbeat">): unknown {
  if (!options.hooks?.onHeartbeat) {
    return undefined;
  }
  return {
    callback: true,
    intervalMs: options.heartbeat?.intervalMs,
    leaseSeconds: options.heartbeat?.leaseSeconds,
  };
}

function mapReviewModel(model: ReviewModelSpec | undefined): unknown {
  if (isCallbackReviewModelSpec(model)) {
    return { callback: true };
  }
  return undefined;
}

interface ModelPlan {
  runnerModel: unknown;
  sessionProfileIds: Map<string, string>;
}

interface HostedProfilePlan {
  profile: unknown;
  profileId: string;
}

function mapReviewModels(options: ReviewOptions): ModelPlan {
  return mapModelPlan(options.model, []);
}

function mapModelPlan(
  model: ReviewModelSpec | undefined,
  sessions: SessionModelOverride[],
): ModelPlan {
  if (isCallbackReviewModelSpec(model)) {
    rejectHostedSessionOverrides(sessions);
    return {
      runnerModel: mapReviewModel(model),
      sessionProfileIds: new Map(),
    };
  }

  const sessionProfileIds = new Map<string, string>();
  const profiles = new Map<string, HostedProfilePlan>();
  let defaultProfileId: string | undefined;

  if (isHostedReviewModelSpec(model)) {
    const planned = addHostedProfile(
      profiles,
      "default",
      model,
    );
    defaultProfileId = planned.profileId;
  }

  for (const session of sessions) {
    if (isHostedReviewModelSpec(session.model)) {
      const planned = addHostedProfile(
        profiles,
        `session:${session.id}`,
        session.model,
      );
      sessionProfileIds.set(session.id, planned.profileId);
      defaultProfileId ??= planned.profileId;
    }
  }

  if (profiles.size === 0) {
    return {
      runnerModel: mapReviewModel(model),
      sessionProfileIds,
    };
  }

  if (!isHostedReviewModelSpec(model)) {
    const sessionsWithoutModel = sessions.filter(
      (session) =>
        !session.model &&
        !sessionProfileIds.has(session.id),
    );
    if (sessionsWithoutModel.length > 0) {
      throw new Error(
        "session model overrides require a run-level model when any session omits its own model",
      );
    }
  }

  return {
    runnerModel: {
      callback: false,
      defaultModelProfileId: defaultProfileId,
      modelProfiles: [...profiles.values()].map((planned) => planned.profile),
    },
    sessionProfileIds,
  };
}

interface SessionModelOverride {
  id: string;
  model?: ReviewModelSpec;
}

function rejectHostedSessionOverrides(sessions: SessionModelOverride[]): void {
  if (sessions.some((session) => isHostedReviewModelSpec(session.model))) {
    throw new Error(
      "hosted session model overrides cannot be mixed with a callback run model",
    );
  }
}

function mapRunnerTools(tools: ReviewTool[]): unknown[] {
  return tools.map((tool) => ({
    id: tool.id,
    description: tool.description,
    parameters: tool.parameters,
    effects: tool.effects ?? [],
    cacheable: tool.cacheable ?? false,
    providerResources: tool.providerResources ?? [],
  }));
}

function addHostedProfile(
  profiles: Map<string, HostedProfilePlan>,
  requestedId: string,
  model: ReviewHostedModelSpec,
): HostedProfilePlan {
  const key = hostedProfileKey(model);
  const existing = profiles.get(key);
  if (existing) {
    return existing;
  }
  const planned = {
    profileId: requestedId,
    profile: {
      id: requestedId,
      provider: runnerProviderForHostedModel(model.provider),
      model: model.model,
      credential: model.credential,
      baseUrl: model.baseUrl,
      apiProtocol: model.apiProtocol,
      maxInputTokens: model.maxInputTokens,
      maxOutputTokens: model.maxOutputTokens,
      temperature: model.temperature,
      topP: model.topP,
    },
  };
  profiles.set(key, planned);
  return planned;
}

function runnerProviderForHostedModel(provider: ReviewHostedModelSpec["provider"]): string {
  if (provider === "openai") {
    return "openai_compatible";
  }
  return provider;
}

function hostedProfileKey(model: ReviewHostedModelSpec): string {
  return JSON.stringify({
    provider: model.provider,
    model: model.model,
    credential: model.credential,
    baseUrl: model.baseUrl,
    apiProtocol: model.apiProtocol,
    maxInputTokens: model.maxInputTokens,
    maxOutputTokens: model.maxOutputTokens,
    temperature: model.temperature,
    topP: model.topP,
  });
}

function mapReviewSourceProvider(
  provider: ReviewOptions["sourceProvider"],
): unknown {
  if (!provider) {
    return undefined;
  }
  return {
    baseUrl: provider.baseUrl,
    callback: Boolean(provider.handler),
  };
}

function mapReviewChange(change: ReviewChangeSpec | undefined): unknown {
  if (!change) {
    return undefined;
  }
  return {
    kind: change.kind,
    baseRevision: change.baseRevision,
    startRevision: change.startRevision,
    headRevision: change.headRevision,
    changedFiles: change.changedFiles ?? [],
    diff: change.diff,
    reviewTarget: change.reviewTarget,
  };
}

export function mapNotification(
  notification: JsonRpcNotification,
): ReviewEvent | undefined {
  if (notification.method !== "event.review") {
    return undefined;
  }
  const record = notification.params;
  if (!isRunnerReviewEventRecord(record)) {
    return undefined;
  }
  return {
    cursor: String(record.seq),
    type: mapRunnerEventType(record.event),
    reviewId: record.runId ?? "unknown",
    timestampUtc: record.timestampUtc,
    payload: attachRunnerEventContext(record.event, record),
  };
}

function attachRunnerEventContext(
  event: unknown,
  record: RunnerReviewEventRecord,
): unknown {
  if (!isRecord(event)) {
    return event;
  }
  const context: Record<string, unknown> = {};
  for (const key of [
    "snapshotId",
    "sessionId",
    "turn",
    "toolCallId",
    "artifactId",
    "findingId",
  ] as const) {
    const value = record[key];
    if (value !== undefined && value !== null) {
      context[key] = value;
    }
  }
  if (Object.keys(context).length === 0) {
    return event;
  }
  return {
    ...event,
    context,
  };
}

export function mapRunnerResult(
  reviewId: string,
  source: ReviewSource,
  value: unknown,
): ReviewResult {
  if (!isRunnerRunResult(value)) {
    throw new Error("muzen-runner returned an invalid run result");
  }
  const findings = value.findings.map(mapRunnerFinding);
  const status = mapRunnerStatus(value.status);
  const metadata = isRecord(value.metadata) ? value.metadata : {};
  return {
    reviewId,
    sessionId: reviewId,
    status,
    conclusion:
      status === "failed" || status === "cancelled"
        ? "changes_requested"
        : conclusionFromFindings(findings),
    summary: `Review completed ${value.summary.completedSessions}/${value.summary.sessions} session(s), produced ${findings.length} finding(s), used ${value.summary.modelCalls} model call(s), ${value.summary.toolCalls} tool call(s), and ${value.summary.totalTokens} total token(s).`,
    findings,
    coverage: coverageFromSnapshots(value.snapshots),
    metadata: {
      ...metadata,
      runnerRunId: value.runId,
      runnerStatus: value.status,
      source: sourceKey(source),
    },
  };
}

export function mapRunnerStatus(status: string): ReviewStatus {
  switch (status) {
    case "created":
    case "queued":
    case "running":
    case "completed":
    case "failed":
    case "cancelled":
      return status;
    case "partial":
    default:
      return "failed";
  }
}

export function isTerminalStatus(status: ReviewStatus): boolean {
  return status === "completed" || status === "failed" || status === "cancelled";
}

export function mapRunnerArtifact(artifact: RunnerArtifact): ReviewArtifact {
  return {
    artifactId: artifact.artifactId,
    bytes: artifact.bytes,
    contentHash: artifact.contentHash,
    content: artifact.content,
  };
}

export function isRunnerStatusResult(value: unknown): value is RunnerStatusResult {
  return (
    isRecord(value) &&
    typeof value.runId === "string" &&
    typeof value.status === "string"
  );
}

export function isRunnerArtifactReadResult(value: unknown): value is RunnerArtifactReadResult {
  return (
    isRecord(value) &&
    typeof value.runId === "string" &&
    (value.view === "redacted" || value.view === "raw") &&
    isRunnerArtifact(value.artifact)
  );
}

export function isRunnerArtifactExportResult(value: unknown): value is RunnerArtifactExportResult {
  return (
    isRecord(value) &&
    typeof value.runId === "string" &&
    (value.view === "redacted" || value.view === "raw") &&
    typeof value.artifactCount === "number" &&
    typeof value.totalBytes === "number" &&
    Array.isArray(value.artifacts) &&
    value.artifacts.every(isRunnerArtifact)
  );
}

function mapReviewLimits(limits: ReviewLimits | undefined): unknown {
  if (!limits) {
    return undefined;
  }
  return {
    maxActiveSessions: limits.maxActiveSessions,
    maxFileBytes: limits.maxFileBytes,
    maxSearchMatches: limits.maxSearchMatches,
  };
}

function mapRunnerEventType(event: unknown): ReviewEventType {
  const kind = eventKind(event);
  switch (kind) {
    case "runStarted":
      return "session.started";
    case "repoManifestCompleted":
      return "scope.inferred";
    case "sessionStarted":
      return "agent.started";
    case "sessionFinished":
      return "agent.completed";
    case "toolBatchStarted":
      return "tool.started";
    case "toolCallCompleted":
    case "toolCallDenied":
    case "artifactCreated":
      return "tool.completed";
    case "findingRecorded":
      return "finding.created";
    case "snapshotFinished":
      return "repo.materialized";
    case "runFinished":
      return runFinishedEventType(event);
    default:
      return "runner.event";
  }
}

function runFinishedEventType(event: unknown): ReviewEventType {
  if (!isRecord(event)) {
    return "session.failed";
  }
  const value = event.runFinished;
  if (isRecord(value) && value.status === "completed") {
    return "session.completed";
  }
  if (isRecord(value) && value.status === "cancelled") {
    return "session.cancelled";
  }
  return "session.failed";
}

function eventKind(event: unknown): string | undefined {
  if (!isRecord(event)) {
    return undefined;
  }
  return Object.keys(event)[0];
}

function mapRunnerFinding(finding: RunnerFinding): ReviewFinding {
  return {
    id: finding.id,
    severity: mapRunnerFindingSeverity(finding),
    category: "other",
    title: finding.title,
    message: finding.claim,
    location: finding.location,
    confidence: finding.confidence,
    validationStatus: finding.validationStatus,
    evidence: finding.evidence ?? [],
    discoveredBy: finding.discoveredBy ?? [],
    validatedBy: finding.validatedBy ?? [],
    challengedBy: finding.challengedBy ?? [],
  };
}

function mapRunnerFindingSeverity(
  finding: RunnerFinding,
): ReviewFinding["severity"] {
  switch (finding.severity) {
    case "blocker":
    case "high":
      return "error";
    case "medium":
    case "low":
      return "warning";
    case "nit":
      return "info";
    default:
      return finding.publishable ? "error" : "info";
  }
}

function conclusionFromFindings(findings: ReviewFinding[]): ReviewResult["conclusion"] {
  if (findings.some((finding) => finding.severity === "error")) {
    return "changes_requested";
  }
  return findings.length === 0 ? "approved" : "commented";
}

function coverageFromSnapshots(snapshots: RunnerSnapshotSummary[]): ReviewResult["coverage"] {
  const filesConsidered = snapshots.reduce(
    (sum, snapshot) => sum + snapshot.files,
    0,
  );
  const filesReviewed = snapshots.reduce(
    (sum, snapshot) => sum + snapshot.capturedFiles,
    0,
  );
  return {
    filesConsidered,
    filesReviewed,
    filesSkipped: Math.max(0, filesConsidered - filesReviewed),
  };
}

interface RunnerReviewEventRecord {
  seq: number;
  timestampUtc: string;
  runId?: string;
  snapshotId?: string | { value?: string };
  sessionId?: string;
  turn?: number;
  toolCallId?: string;
  artifactId?: string | { value?: string };
  findingId?: string;
  event: unknown;
}

interface RunnerRunResult {
  runId: string;
  status: string;
  summary: RunnerRunSummary;
  findings: RunnerFinding[];
  snapshots: RunnerSnapshotSummary[];
  sessionOutputs?: RunnerSessionOutput[];
  metadata?: Record<string, unknown>;
}

interface RunnerRunSummary {
  sessions: number;
  completedSessions: number;
  modelCalls: number;
  toolCalls: number;
  inputTokens?: number;
  outputTokens?: number;
  totalTokens: number;
}

interface RunnerSessionOutput {
  sessionId: string;
  status: string;
  completed: boolean;
  output?: string | null;
}

interface RunnerFinding {
  id: string;
  title: string;
  claim: string;
  publishable: boolean;
  severity?: string;
  confidence?: number;
  validationStatus?: string;
  evidence?: RunnerFindingEvidence[];
  discoveredBy?: string[];
  validatedBy?: string[];
  challengedBy?: string[];
  location?: RunnerFindingLocation;
}

interface RunnerFindingEvidence {
  evidenceId: string;
  artifactId: string;
  kind: string;
  contentHash: string;
  producingToolCallId: string;
}

interface RunnerFindingLocation {
  path: string;
  revision?: string;
  startLine?: number;
  endLine?: number;
  startColumn?: number;
  endColumn?: number;
  side?: string;
  providerAnchor?: Record<string, unknown>;
}

interface RunnerSnapshotSummary {
  files: number;
  capturedFiles: number;
}

interface RunnerStatusResult {
  runId: string;
  status: string;
}

interface RunnerArtifact {
  artifactId: string;
  bytes: number;
  contentHash: string;
  content: string;
}

interface RunnerArtifactReadResult {
  runId: string;
  view: "redacted" | "raw";
  artifact: RunnerArtifact;
}

interface RunnerArtifactExportResult {
  runId: string;
  view: "redacted" | "raw";
  artifactCount: number;
  totalBytes: number;
  artifacts: RunnerArtifact[];
}

function isRunnerReviewEventRecord(value: unknown): value is RunnerReviewEventRecord {
  return (
    isRecord(value) &&
    typeof value.seq === "number" &&
    typeof value.timestampUtc === "string" &&
    "event" in value
  );
}

function isRunnerRunResult(value: unknown): value is RunnerRunResult {
  return (
    isRecord(value) &&
    typeof value.runId === "string" &&
    typeof value.status === "string" &&
    isRecord(value.summary) &&
    Array.isArray(value.findings) &&
    Array.isArray(value.snapshots)
  );
}

function isRunnerArtifact(value: unknown): value is RunnerArtifact {
  return (
    isRecord(value) &&
    typeof value.artifactId === "string" &&
    typeof value.bytes === "number" &&
    typeof value.contentHash === "string" &&
    typeof value.content === "string"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
