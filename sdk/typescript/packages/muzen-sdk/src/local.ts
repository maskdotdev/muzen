import { randomUUID } from "node:crypto";

import { MuzenUnsupportedFeatureError } from "./errors.js";
import {
  RunnerProtocolError,
  RunnerStdioClient,
} from "./protocol.js";
import {
  delay,
  parseTimeoutMs,
  throwIfAborted,
  withTimeout,
} from "./review-flow.js";
import {
  isRunnerArtifactExportResult,
  isRunnerArtifactReadResult,
  isRunnerStatusResult,
  isTerminalStatus,
  mapNotification,
  mapRunnerArtifact,
  mapRunnerResult,
  mapRunnerStatus,
  toRunnerStartParams,
} from "./runner-mapping.js";
import { registerReviewCallbacks } from "./runner-callbacks.js";
import { parseReviewSource } from "./sources.js";
import { UnsupportedWorkspaceProfileCollection } from "./unsupported.js";
import {
  unwrapWebhookHttpResponse,
  unwrapWorkerRun,
} from "./wire-validation.js";
import type {
  HostConfiguration,
  ModelProfile,
  ModelProfileInput,
  Muzen,
  MuzenWorkerRun,
  MuzenWorkerRunOnceOptions,
  MuzenWorkers,
  MuzenWorkerStartOptions,
  MuzenWebhookHandler,
  MuzenWebhookProvider,
  MuzenWebhooks,
  MuzenWebhookResponseOptions,
  MuzenWorkspace,
  ProviderProfile,
  ProviderProfileInput,
  ReviewArtifact,
  ReviewArtifactExport,
  ReviewArtifactExportOptions,
  ReviewArtifactReadOptions,
  ReviewCancelOptions,
  ReviewEvent,
  ReviewOptions,
  ReviewResult,
  ReviewSession,
  ReviewSessionSnapshot,
  ReviewSource,
  ReviewSourceLike,
  ReviewStatus,
} from "./types.js";

export class RunnerBackedMuzen implements Muzen {
  private readonly sessions = new Map<string, RunnerBackedReviewSession>();
  readonly webhooks: MuzenWebhooks;
  readonly workers: MuzenWorkers;

  constructor(private readonly runner: RunnerStdioClient) {
    this.webhooks = new RunnerBackedMuzenWebhooks(runner);
    this.workers = new RunnerBackedMuzenWorkers(runner);
  }

  async review(
    sourceLike: ReviewSourceLike,
    options: ReviewOptions = {},
  ): Promise<ReviewSession> {
    return this.createReviewSession({ source: sourceLike, options });
  }

  workspace(id: string): MuzenWorkspace {
    return new RunnerBackedWorkspace(this, id);
  }

  async createReviewSession(input: {
    source: ReviewSourceLike;
    options?: ReviewOptions;
  }): Promise<ReviewSession> {
    throwIfAborted(input.options?.signal);
    const source = parseReviewSource(input.source);
    const reviewId = `review-${randomUUID()}`;
    const events: ReviewEvent[] = [];
    let hookError: Error | undefined;
    const unsubscribe = this.runner.onNotification((notification) => {
      const event = mapNotification(notification);
      if (event && event.reviewId === reviewId) {
        events.push(event);
        try {
          input.options?.hooks?.onEvent?.(event);
        } catch (error) {
          hookError ??= reviewHookError(error);
        }
      }
    });
    const unsubscribeCallbacks = registerReviewCallbacks(
      this.runner,
      input.options ?? {},
    );
    let startSent = false;
    let startSettled = false;
    const stopCancelOnAbort = onAbort(input.options?.signal, () => {
      if (!startSent || startSettled) {
        return;
      }
      void this.runner.request("run.cancel", {
        runId: reviewId,
        reason: "operation aborted",
      }).catch(() => {});
    });
    try {
      throwIfAborted(input.options?.signal);
      startSent = true;
      const runnerResult = await this.runner.request(
        "run.start",
        toRunnerStartParams(reviewId, source, input.options ?? {}),
      );
      startSettled = true;
      throwIfAborted(input.options?.signal);
      if (hookError) {
        throw hookError;
      }
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
      startSettled = true;
      stopCancelOnAbort();
      unsubscribeCallbacks();
      unsubscribe();
    }
  }

  async resumeReview(id: string): Promise<ReviewSession> {
    const existing = this.sessions.get(id);
    if (!existing) {
      throw new MuzenUnsupportedFeatureError(
        "local resumeReview is process-local for createMuzen(); use createMuzenClient({ baseUrl }).resumeReview(id) for durable review lookup",
      );
    }
    return existing;
  }

  async close(): Promise<void> {
    await this.runner.close();
  }
}

class RunnerBackedWorkspace implements MuzenWorkspace {
  readonly models = new UnsupportedWorkspaceProfileCollection<
    ModelProfileInput,
    ModelProfile
  >("model");
  readonly providers = new UnsupportedWorkspaceProfileCollection<
    ProviderProfileInput,
    ProviderProfile
  >("provider");

  constructor(
    private readonly muzen: Muzen,
    readonly id: string,
  ) {}

  review(
    source: ReviewSourceLike,
    options?: ReviewOptions,
  ): Promise<ReviewSession> {
    return this.muzen.review(source, options);
  }
}

class RunnerBackedMuzenWebhooks implements MuzenWebhooks {
  readonly github: MuzenWebhookHandler;
  readonly gitlab: MuzenWebhookHandler;

  constructor(runner: RunnerStdioClient) {
    this.github = new RunnerBackedMuzenWebhookHandler(runner, "github");
    this.gitlab = new RunnerBackedMuzenWebhookHandler(runner, "gitlab");
  }
}

class RunnerBackedMuzenWebhookHandler implements MuzenWebhookHandler {
  constructor(
    private readonly runner: RunnerStdioClient,
    private readonly provider: MuzenWebhookProvider,
  ) {}

  async response(
    request: Request,
    options: MuzenWebhookResponseOptions = {},
  ): Promise<Response> {
    if (request.method !== "POST") {
      throw new MuzenUnsupportedFeatureError(
        `Muzen ${this.provider} webhook handlers expect POST requests`,
      );
    }
    throwIfAborted(options.signal ?? request.signal);
    const result = unwrapWebhookHttpResponse(
      await this.runner.request(`webhook.${this.provider}.handle`, {
        workspaceId: options.workspaceId,
        headers: headersToRecord(request.headers),
        body: await request.text(),
        secret: webhookSecret(this.provider, options.secret),
        options: {
          reviewOptions: options.review ?? {},
        },
      }),
    );
    return new Response(result.body, {
      status: result.statusCode,
      headers: result.headers,
    });
  }
}

class RunnerBackedMuzenWorkers implements MuzenWorkers {
  constructor(private readonly runner: RunnerStdioClient) {}

  async runOnce(
    options: MuzenWorkerRunOnceOptions = {},
  ): Promise<MuzenWorkerRun> {
    throwIfAborted(options.signal);
    const run = unwrapWorkerRun(
      await this.runner.request("worker.runOnce", {
        workerId: options.workerId,
        maxSessions: options.maxSessions ?? 1,
        hostConfig: options.hostConfig ?? {},
      }),
    );
    throwIfAborted(options.signal);
    return run;
  }

  async start(options: MuzenWorkerStartOptions = {}): Promise<void> {
    validateWorkerQueues(options.queues);
    const maxSessions = options.maxSessions ?? options.concurrency ?? 1;
    const pollIntervalMs = options.pollIntervalMs ?? 1_000;
    while (true) {
      const run = await this.runOnce({
        workerId: options.workerId,
        maxSessions,
        hostConfig: workerHostConfig(options),
        signal: options.signal,
      });
      if (options.stopWhenIdle && run.claimed === 0) {
        return;
      }
      await delay(pollIntervalMs, options.signal);
    }
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

  async readArtifact(
    artifactId: string,
    options: ReviewArtifactReadOptions = {},
  ): Promise<ReviewArtifact> {
    const result = await this.runner.request("artifact.read", {
      runId: this.id,
      artifactId,
      view: options.view ?? "redacted",
    });
    if (!isRunnerArtifactReadResult(result)) {
      throw new Error("muzen-runner returned an invalid artifact read result");
    }
    return mapRunnerArtifact(result.artifact);
  }

  async exportArtifacts(
    options: ReviewArtifactExportOptions = {},
  ): Promise<ReviewArtifactExport> {
    const result = await this.runner.request("artifact.export", {
      runId: this.id,
      artifactIds: options.artifactIds ?? [],
      view: options.view ?? "redacted",
      maxArtifacts: options.maxArtifacts,
      maxBytes: options.maxBytes,
    });
    if (!isRunnerArtifactExportResult(result)) {
      throw new Error("muzen-runner returned an invalid artifact export result");
    }
    return {
      view: result.view,
      artifactCount: result.artifactCount,
      totalBytes: result.totalBytes,
      artifacts: result.artifacts.map(mapRunnerArtifact),
    };
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

function reviewHookError(error: unknown): Error {
  if (error instanceof Error) {
    return error;
  }
  return new Error(`review event hook failed: ${String(error)}`);
}

function onAbort(signal: AbortSignal | undefined, handler: () => void): () => void {
  if (!signal) {
    return () => {};
  }
  if (signal.aborted) {
    handler();
    return () => {};
  }
  signal.addEventListener("abort", handler, { once: true });
  return () => signal.removeEventListener("abort", handler);
}

function headersToRecord(headers: Headers): Record<string, string> {
  const result: Record<string, string> = {};
  headers.forEach((value, key) => {
    result[key] = value;
  });
  return result;
}

function webhookSecret(
  provider: MuzenWebhookProvider,
  explicit: string | undefined,
): string | undefined {
  if (explicit !== undefined) {
    return explicit;
  }
  if (provider === "github") {
    return process.env.GITHUB_WEBHOOK_SECRET;
  }
  return process.env.GITLAB_WEBHOOK_TOKEN ?? process.env.GITLAB_WEBHOOK_SECRET;
}

function validateWorkerQueues(queues: string[] | undefined): void {
  if (!queues) {
    return;
  }
  for (const queue of queues) {
    if (queue !== "reviews") {
      throw new MuzenUnsupportedFeatureError(
        `unsupported Muzen worker queue: ${queue}`,
      );
    }
  }
}

function workerHostConfig(options: MuzenWorkerStartOptions): HostConfiguration {
  const hostConfig: HostConfiguration = {
    ...(options.hostConfig ?? {}),
  };
  if (options.concurrency !== undefined) {
    hostConfig.scheduling = {
      ...(hostConfig.scheduling ?? {}),
      concurrency: {
        ...(hostConfig.scheduling?.concurrency ?? {}),
        maxRunningGlobal: options.concurrency,
      },
    };
  }
  return hostConfig;
}
