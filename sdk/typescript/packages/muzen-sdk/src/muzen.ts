import { randomUUID } from "node:crypto";

import {
  RunnerProtocolError,
  RunnerStdioClient,
  type JsonRpcNotification,
} from "./protocol.js";
import {
  parseReviewSource,
  sourceKey,
} from "./sources.js";
import type {
  CreateMuzenClientOptions,
  CreateMuzenOptions,
  CreateReviewSessionInput,
  CreateReviewSessionResult,
  Muzen,
  ReviewCancelOptions,
  ReviewEvent,
  ReviewEventType,
  ReviewFinding,
  ReviewLimits,
  ReviewOptions,
  ReviewResult,
  ReviewSession,
  ReviewSessionSnapshot,
  ReviewSource,
  ReviewSourceLike,
  ReviewStatus,
} from "./types.js";

export async function createMuzen(
  options: CreateMuzenOptions = {},
): Promise<Muzen> {
  const runner = new RunnerStdioClient({
    runnerPath:
      options.runnerPath ?? process.env.MUZEN_RUNNER_PATH ?? "muzen-runner",
    runnerArgs: options.runnerArgs ?? ["stdio"],
  });
  await runner.handshake({
    clientName: options.clientName ?? "@muzen/sdk",
    clientVersion: options.clientVersion,
  });
  return new RunnerBackedMuzen(runner);
}

export function createMuzenClient(
  _options: CreateMuzenClientOptions,
): Muzen {
  throw new MuzenUnsupportedFeatureError(
    "createMuzenClient({ baseUrl }) requires the remote HTTP transport, which is not implemented in this preview",
  );
}

export async function createReviewSession(
  input: CreateReviewSessionInput,
): Promise<CreateReviewSessionResult> {
  const muzen = await createMuzen(input.muzen);
  const review = await muzen.review(input.source, input.options);
  return { muzen, review };
}

export class MuzenUnsupportedFeatureError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "MuzenUnsupportedFeatureError";
  }
}

class RunnerBackedMuzen implements Muzen {
  private readonly sessions = new Map<string, RunnerBackedReviewSession>();

  constructor(private readonly runner: RunnerStdioClient) {}

  async review(
    sourceLike: ReviewSourceLike,
    options: ReviewOptions = {},
  ): Promise<ReviewSession> {
    return this.createReviewSession({ source: sourceLike, options });
  }

  async createReviewSession(input: {
    source: ReviewSourceLike;
    options?: ReviewOptions;
  }): Promise<ReviewSession> {
    const source = parseReviewSource(input.source);
    const reviewId = `review-${randomUUID()}`;
    const events: ReviewEvent[] = [];
    const unsubscribe = this.runner.onNotification((notification) => {
      const event = mapNotification(notification);
      if (event && event.reviewId === reviewId) {
        events.push(event);
      }
    });
    try {
      const runnerResult = await this.runner.request(
        "run.start",
        toRunnerStartParams(reviewId, source, input.options ?? {}),
      );
      const result = mapRunnerResult(reviewId, source, runnerResult);
      const review = new RunnerBackedReviewSession(
        this.runner,
        reviewId,
        "completed",
        source,
        events,
        result,
      );
      this.sessions.set(reviewId, review);
      return review;
    } catch (error) {
      if (
        error instanceof RunnerProtocolError &&
        error.kind === "invalid_input"
      ) {
        throw error;
      }
      throw error;
    } finally {
      unsubscribe();
    }
  }

  async resumeReview(id: string): Promise<ReviewSession> {
    const existing = this.sessions.get(id);
    if (!existing) {
      throw new MuzenUnsupportedFeatureError(
        "resumeReview currently supports sessions created by this SDK process; durable session lookup is not implemented yet",
      );
    }
    return existing;
  }

  async close(): Promise<void> {
    await this.runner.close();
  }
}

class RunnerBackedReviewSession implements ReviewSession {
  private readonly listeners = new Set<(event: ReviewEvent) => void>();

  constructor(
    private readonly runner: RunnerStdioClient,
    readonly id: string,
    private currentStatus: ReviewStatus,
    readonly source: ReviewSource,
    private readonly recordedEvents: ReviewEvent[],
    private currentResult?: ReviewResult,
  ) {}

  get status(): ReviewStatus {
    return this.currentStatus;
  }

  subscribe(
    listener: (event: ReviewEvent) => void,
    options: { replay?: boolean } = {},
  ): () => void {
    if (options.replay ?? true) {
      for (const event of this.recordedEvents) {
        listener(event);
      }
    }
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  async *events(options: {
    after?: string | null;
    signal?: AbortSignal;
  } = {}): AsyncIterable<ReviewEvent> {
    throwIfAborted(options.signal);
    const start = afterCursorIndex(this.recordedEvents, options.after);
    for (const event of this.recordedEvents.slice(start)) {
      throwIfAborted(options.signal);
      yield event;
    }
  }

  eventsResponse(options: {
    after?: string | null;
    signal?: AbortSignal;
  } = {}): Response {
    const events = this.events(options);
    const encoder = new TextEncoder();
    const stream = new ReadableStream<Uint8Array>({
      async start(controller) {
        try {
          for await (const event of events) {
            controller.enqueue(
              encoder.encode(
                `id: ${event.cursor}\nevent: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`,
              ),
            );
          }
          controller.close();
        } catch (error) {
          controller.error(error);
        }
      },
    });
    return new Response(stream, {
      headers: {
        "Content-Type": "text/event-stream",
      },
    });
  }

  async wait(options: {
    timeout?: string | number;
    signal?: AbortSignal;
  } = {}): Promise<ReviewResult> {
    throwIfAborted(options.signal);
    if (this.currentResult) {
      return this.currentResult;
    }
    const timeoutMs = parseTimeoutMs(options.timeout);
    const resultPromise = this.result().then((result) => {
      if (!result) {
        throw new Error(`review ${this.id} has no final result yet`);
      }
      return result;
    });
    if (timeoutMs === undefined) {
      return resultPromise;
    }
    return withTimeout(resultPromise, timeoutMs);
  }

  async result(): Promise<ReviewResult | undefined> {
    if (this.currentResult) {
      return this.currentResult;
    }
    const runnerResult = await this.runner.request("run.result", {
      runId: this.id,
    });
    this.currentResult = mapRunnerResult(this.id, this.source, runnerResult);
    this.currentStatus = this.currentResult.status;
    return this.currentResult;
  }

  async cancel(reason?: string | ReviewCancelOptions): Promise<void> {
    const cancelReason =
      typeof reason === "string" ? reason : reason?.reason ?? "cancelled";
    await this.runner.request("run.cancel", {
      runId: this.id,
      reason: cancelReason,
    });
    if (!isTerminalStatus(this.currentStatus)) {
      this.currentStatus = "cancelled";
      this.record({
        cursor: String(this.recordedEvents.length + 1),
        type: "session.cancelled",
        reviewId: this.id,
        timestampUtc: new Date().toISOString(),
        payload: { reason: cancelReason },
      });
    }
  }

  async refresh(): Promise<ReviewSessionSnapshot> {
    const status = await this.runner.request("run.status", {
      runId: this.id,
    });
    if (isRunnerStatusResult(status)) {
      this.currentStatus = mapRunnerStatus(status.status);
    }
    return {
      id: this.id,
      status: this.currentStatus,
      source: this.source,
      result: this.currentResult,
    };
  }

  private record(event: ReviewEvent): void {
    this.recordedEvents.push(event);
    for (const listener of this.listeners) {
      listener(event);
    }
  }
}

function toRunnerStartParams(
  reviewId: string,
  source: ReviewSource,
  options: ReviewOptions,
): unknown {
  if (source.type !== "local") {
    throw new MuzenUnsupportedFeatureError(
      `review source ${sourceKey(source)} requires provider materialization, which is not implemented in this preview`,
    );
  }
  const changedFiles = options.scope?.files ?? source.changedFiles ?? [];
  return {
    protocolVersion: "muzen.runner.v1",
    runId: reviewId,
    repo: source.repo,
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

function mapNotification(
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

function mapRunnerResult(
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

function mapRunnerStatus(status: string): ReviewStatus {
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

function isTerminalStatus(status: ReviewStatus): boolean {
  return status === "completed" || status === "failed" || status === "cancelled";
}

function afterCursorIndex(
  events: ReviewEvent[],
  after: string | null | undefined,
): number {
  if (!after) {
    return 0;
  }
  const index = events.findIndex((event) => event.cursor === after);
  return index === -1 ? 0 : index + 1;
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw new Error("operation aborted");
  }
}

function parseTimeoutMs(timeout: string | number | undefined): number | undefined {
  if (timeout === undefined) {
    return undefined;
  }
  if (typeof timeout === "number") {
    return timeout;
  }
  const match = timeout.trim().match(/^(\d+)(ms|s|m)?$/);
  if (!match) {
    throw new Error(`invalid timeout: ${timeout}`);
  }
  const amount = Number(match[1]);
  const unit = match[2] ?? "ms";
  switch (unit) {
    case "ms":
      return amount;
    case "s":
      return amount * 1000;
    case "m":
      return amount * 60_000;
  }
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`review wait timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    promise
      .then(resolve, reject)
      .finally(() => clearTimeout(timer));
  });
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

function isRunnerStatusResult(value: unknown): value is RunnerStatusResult {
  return (
    isRecord(value) &&
    typeof value.runId === "string" &&
    typeof value.status === "string"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
