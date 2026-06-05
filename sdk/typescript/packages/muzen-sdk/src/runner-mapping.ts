import type { JsonRpcNotification } from "./protocol.js";
import { sourceKey } from "./sources.js";
import type {
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
    options.scope?.files ?? (source.type === "local" ? source.changedFiles ?? [] : []);
  const params: Record<string, unknown> = {
    protocolVersion: "muzen.runner.v1",
    runId: reviewId,
    source,
    changedFiles,
    sessions: (options.sessions ?? []).map((session) => ({
      id: session.id,
      role: session.role,
      objective: session.objective,
      cwd: session.cwd,
      modelProfileId: session.modelProfileId ?? options.model,
      budget: session.budget,
    })),
    limits: mapReviewLimits(options.limits),
  };
  if (source.type === "local") {
    params.repo = source.repo;
  }
  return params;
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
  return {
    reviewId,
    sessionId: reviewId,
    status,
    conclusion: conclusionFromFindings(findings),
    summary: `Review completed ${value.summary.completedSessions}/${value.summary.sessions} session(s), produced ${findings.length} finding(s), used ${value.summary.modelCalls} model call(s), ${value.summary.toolCalls} tool call(s), and ${value.summary.totalTokens} total token(s).`,
    findings,
    coverage: coverageFromSnapshots(value.snapshots),
    metadata: {
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
    severity: finding.publishable ? "error" : "info",
    category: "other",
    title: finding.title,
    message: finding.claim,
  };
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
