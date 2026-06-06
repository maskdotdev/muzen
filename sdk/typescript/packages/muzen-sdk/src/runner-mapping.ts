import type { JsonRpcNotification } from "./protocol.js";
import { sourceKey } from "./sources.js";
import type {
  ReviewChangeSpec,
  ReviewArtifact,
  ReviewEvent,
  ReviewEventType,
  ReviewFinding,
  ReviewLimits,
  ReviewOptions,
  ReviewResult,
  ReviewSource,
  ReviewStatus,
} from "./types.js";

export function toRunnerStartParams(
  reviewId: string,
  source: ReviewSource,
  options: ReviewOptions,
): unknown {
  const changedFiles =
    options.scope?.files ??
    changedFilePaths(options.change) ??
    (source.type === "local" ? source.changedFiles ?? [] : []);
  const defaultModelProfileId =
    typeof options.model === "string" ? options.model : undefined;
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
    sessions: (options.sessions ?? []).map((session) => ({
      id: session.id,
      role: session.role,
      objective: session.objective,
      cwd: session.cwd,
      modelProfileId: session.modelProfileId ?? defaultModelProfileId,
      instructions: (session.instructions ?? []).map((instruction) => ({
        kind: instruction.kind,
        text: instruction.text,
        trusted: instruction.trusted ?? false,
      })),
      toolGrants: session.toolGrants ?? [],
      budget: session.budget,
    })),
    limits: mapReviewLimits(options.limits),
    model: mapReviewModel(options.model),
    tools: (options.tools ?? []).map((tool) => ({
      id: tool.id,
      description: tool.description,
      parameters: tool.parameters,
      effects: tool.effects ?? [],
      cacheable: tool.cacheable ?? false,
      providerResources: tool.providerResources ?? [],
    })),
    heartbeat: mapReviewHeartbeat(options),
  };
  if (source.type === "local") {
    params.repo = source.repo;
  }
  return params;
}

function mapReviewHeartbeat(options: ReviewOptions): unknown {
  if (!options.hooks?.onHeartbeat) {
    return undefined;
  }
  return {
    callback: true,
    intervalMs: options.heartbeat?.intervalMs,
    leaseSeconds: options.heartbeat?.leaseSeconds,
  };
}

function mapReviewModel(model: ReviewOptions["model"]): unknown {
  if (!model || typeof model === "string") {
    return undefined;
  }
  if (model.kind === "callback") {
    return { callback: true };
  }
  return undefined;
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

function changedFilePaths(change: ReviewChangeSpec | undefined): string[] | undefined {
  if (!change?.changedFiles?.length) {
    return undefined;
  }
  return change.changedFiles.map((file) => file.path);
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
    metadata: change.metadata ?? {},
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
    payload: record.event,
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
    conclusion: conclusionFromFindings(findings),
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
  event: unknown;
}

interface RunnerRunResult {
  runId: string;
  status: string;
  summary: RunnerRunSummary;
  findings: RunnerFinding[];
  snapshots: RunnerSnapshotSummary[];
  metadata?: Record<string, unknown>;
}

interface RunnerRunSummary {
  sessions: number;
  completedSessions: number;
  modelCalls: number;
  toolCalls: number;
  totalTokens: number;
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
