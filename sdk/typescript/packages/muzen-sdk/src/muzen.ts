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
  ModelProfile,
  ModelProfileInput,
  Muzen,
  MuzenWebhookHandler,
  MuzenWebhookProvider,
  MuzenWebhooks,
  MuzenWebhookResponseOptions,
  MuzenWorkspace,
  ProviderProfile,
  ProviderProfileInput,
  ReviewCancelOptions,
  ReviewEvent,
  ReviewEventType,
  ReviewArtifact,
  ReviewArtifactExport,
  ReviewArtifactExportOptions,
  ReviewArtifactReadOptions,
  ReviewFinding,
  ReviewLimits,
  ReviewOptions,
  ReviewResult,
  ReviewSession,
  ReviewSessionSnapshot,
  ReviewSource,
  ReviewSourceLike,
  ReviewStatus,
  WorkspaceProfileCollection,
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
  options: CreateMuzenClientOptions,
): Muzen {
  return new RemoteMuzen(options);
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

class RemoteMuzen implements Muzen {
  private readonly baseUrl: URL;
  private readonly fetch: typeof globalThis.fetch;
  readonly webhooks: MuzenWebhooks;

  constructor(private readonly options: CreateMuzenClientOptions) {
    this.baseUrl = new URL(options.baseUrl);
    this.fetch = options.fetch ?? globalThis.fetch;
    if (!this.fetch) {
      throw new MuzenUnsupportedFeatureError(
        "createMuzenClient({ baseUrl }) requires a fetch implementation",
      );
    }
    this.webhooks = new RemoteMuzenWebhooks(this);
  }

  async review(
    sourceLike: ReviewSourceLike,
    options: ReviewOptions = {},
  ): Promise<ReviewSession> {
    return this.createReviewSession({ source: sourceLike, options });
  }

  workspace(id: string): MuzenWorkspace {
    return new RemoteWorkspace(this, id);
  }

  async createReviewSession(input: {
    source: ReviewSourceLike;
    options?: ReviewOptions;
  }): Promise<ReviewSession> {
    const source = parseReviewSource(input.source);
    const response = await this.requestJson("/v1/reviews", {
      method: "POST",
      body: {
        source,
        options: input.options ?? {},
      },
    });
    const snapshot = unwrapReviewSnapshot(response);
    return new RemoteReviewSession(this, snapshot);
  }

  async resumeReview(id: string): Promise<ReviewSession> {
    const snapshot = unwrapReviewSnapshot(
      await this.requestJson(`/v1/reviews/${encodeURIComponent(id)}`),
    );
    return new RemoteReviewSession(this, snapshot);
  }

  async close(): Promise<void> {
    // Remote clients do not own a local process.
  }

  async requestJson(
    path: string,
    options: {
      method?: string;
      body?: unknown;
      signal?: AbortSignal;
    } = {},
  ): Promise<unknown> {
    const response = await this.rawRequest(path, {
      method: options.method,
      body:
        options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: options.signal,
      headers: {
        "Content-Type": "application/json",
      },
    });
    if (response.status === 204) {
      return undefined;
    }
    return response.json();
  }

  async rawRequest(path: string, init: RequestInit = {}): Promise<Response> {
    const url = new URL(path, this.baseUrl);
    const headers = new Headers(init.headers);
    if (this.options.token) {
      headers.set("Authorization", `Bearer ${this.options.token}`);
    }
    const response = await this.fetch(url, {
      ...init,
      headers,
    });
    if (!response.ok) {
      throw new Error(
        `Muzen remote request failed: ${response.status} ${response.statusText}`,
      );
    }
    return response;
  }
}

class RemoteMuzenWebhooks implements MuzenWebhooks {
  readonly github: MuzenWebhookHandler;
  readonly gitlab: MuzenWebhookHandler;

  constructor(client: RemoteMuzen) {
    this.github = new RemoteMuzenWebhookHandler(client, "github");
    this.gitlab = new RemoteMuzenWebhookHandler(client, "gitlab");
  }
}

class RemoteMuzenWebhookHandler implements MuzenWebhookHandler {
  constructor(
    private readonly client: RemoteMuzen,
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
    const body = await request.arrayBuffer();
    const path = options.workspaceId
      ? `/v1/workspaces/${encodeURIComponent(options.workspaceId)}/webhooks/${this.provider}`
      : `/v1/webhooks/${this.provider}`;
    return this.client.rawRequest(path, {
      method: "POST",
      headers: request.headers,
      body,
      signal: options.signal ?? request.signal,
    });
  }
}

class RemoteWorkspace implements MuzenWorkspace {
  readonly models: WorkspaceProfileCollection<ModelProfileInput, ModelProfile>;
  readonly providers: WorkspaceProfileCollection<ProviderProfileInput, ProviderProfile>;

  constructor(
    private readonly client: RemoteMuzen,
    readonly id: string,
  ) {
    this.models = new RemoteWorkspaceProfileCollection(
      client,
      id,
      "models",
      unwrapModelProfile,
      unwrapModelProfiles,
    );
    this.providers = new RemoteWorkspaceProfileCollection(
      client,
      id,
      "providers",
      unwrapProviderProfile,
      unwrapProviderProfiles,
    );
  }

  async review(
    sourceLike: ReviewSourceLike,
    options: ReviewOptions = {},
  ): Promise<ReviewSession> {
    const source = parseReviewSource(sourceLike);
    const response = await this.client.requestJson(
      `/v1/workspaces/${encodeURIComponent(this.id)}/reviews`,
      {
        method: "POST",
        body: {
          source,
          options,
        },
      },
    );
    return new RemoteReviewSession(this.client, unwrapReviewSnapshot(response));
  }
}

class RemoteWorkspaceProfileCollection<Input, Profile>
  implements WorkspaceProfileCollection<Input, Profile>
{
  constructor(
    private readonly client: RemoteMuzen,
    private readonly workspaceId: string,
    private readonly kind: "models" | "providers",
    private readonly unwrapOne: (value: unknown) => Profile,
    private readonly unwrapMany: (value: unknown) => Profile[],
  ) {}

  async set(name: string, input: Input): Promise<Profile> {
    return this.unwrapOne(
      await this.client.requestJson(this.profilePath(name), {
        method: "PUT",
        body: input,
      }),
    );
  }

  async get(name: string): Promise<Profile | undefined> {
    const response = await this.client.requestJson(this.profilePath(name));
    if (response === undefined || response === null) {
      return undefined;
    }
    return this.unwrapOne(response);
  }

  async list(): Promise<Profile[]> {
    return this.unwrapMany(await this.client.requestJson(this.collectionPath()));
  }

  private collectionPath(): string {
    return `/v1/workspaces/${encodeURIComponent(this.workspaceId)}/${this.kind}`;
  }

  private profilePath(name: string): string {
    return `${this.collectionPath()}/${encodeURIComponent(name)}`;
  }
}

class RemoteReviewSession implements ReviewSession {
  private currentStatus: ReviewStatus;
  private currentResult?: ReviewResult;

  constructor(
    private readonly client: RemoteMuzen,
    snapshot: ReviewSessionSnapshot,
  ) {
    this.id = snapshot.id;
    this.source = snapshot.source;
    this.currentStatus = snapshot.status;
    this.currentResult = snapshot.result;
  }

  readonly id: string;
  readonly source: ReviewSource;

  get status(): ReviewStatus {
    return this.currentStatus;
  }

  subscribe(
    listener: (event: ReviewEvent) => void,
    options: { replay?: boolean } = {},
  ): () => void {
    let cancelled = false;
    if (options.replay ?? true) {
      void (async () => {
        for await (const event of this.events()) {
          if (cancelled) {
            return;
          }
          listener(event);
        }
      })();
    }
    return () => {
      cancelled = true;
    };
  }

  async *events(options: {
    after?: string | null;
    signal?: AbortSignal;
  } = {}): AsyncIterable<ReviewEvent> {
    throwIfAborted(options.signal);
    const search = new URLSearchParams();
    if (options.after) {
      search.set("after", options.after);
    }
    const suffix = search.size > 0 ? `?${search}` : "";
    const response = await this.client.requestJson(
      `/v1/reviews/${encodeURIComponent(this.id)}/events${suffix}`,
      { signal: options.signal },
    );
    const events = unwrapReviewEvents(response);
    for (const event of events) {
      throwIfAborted(options.signal);
      yield event;
    }
  }

  eventsResponse(options: {
    after?: string | null;
    signal?: AbortSignal;
  } = {}): Response {
    const search = new URLSearchParams();
    if (options.after) {
      search.set("after", options.after);
    }
    const suffix = search.size > 0 ? `?${search}` : "";
    const response = this.client.rawRequest(
      `/v1/reviews/${encodeURIComponent(this.id)}/events/stream${suffix}`,
      { signal: options.signal },
    );
    return new Response(
      new ReadableStream({
        async start(controller) {
          try {
            const stream = (await response).body;
            if (!stream) {
              controller.close();
              return;
            }
            const reader = stream.getReader();
            while (true) {
              const next = await reader.read();
              if (next.done) {
                break;
              }
              controller.enqueue(next.value);
            }
            controller.close();
          } catch (error) {
            controller.error(error);
          }
        },
      }),
      {
        headers: {
          "Content-Type": "text/event-stream",
        },
      },
    );
  }

  async wait(options: {
    timeout?: string | number;
    signal?: AbortSignal;
  } = {}): Promise<ReviewResult> {
    throwIfAborted(options.signal);
    const timeoutMs = parseTimeoutMs(options.timeout);
    const resultPromise = pollUntilResult(() => this.result(options), options.signal);
    if (timeoutMs === undefined) {
      return resultPromise;
    }
    return withTimeout(resultPromise, timeoutMs);
  }

  async result(options: { signal?: AbortSignal } = {}): Promise<ReviewResult | undefined> {
    if (this.currentResult) {
      return this.currentResult;
    }
    const response = await this.client.requestJson(
      `/v1/reviews/${encodeURIComponent(this.id)}/result`,
      { signal: options.signal },
    );
    this.currentResult = unwrapOptionalReviewResult(response);
    if (this.currentResult) {
      this.currentStatus = this.currentResult.status;
    }
    return this.currentResult;
  }

  async readArtifact(
    artifactId: string,
    options: ReviewArtifactReadOptions = {},
  ): Promise<ReviewArtifact> {
    const search = new URLSearchParams({
      view: options.view ?? "redacted",
    });
    const response = await this.client.requestJson(
      `/v1/reviews/${encodeURIComponent(this.id)}/artifacts/${encodeURIComponent(artifactId)}?${search}`,
    );
    return unwrapReviewArtifact(response);
  }

  async exportArtifacts(
    options: ReviewArtifactExportOptions = {},
  ): Promise<ReviewArtifactExport> {
    return unwrapReviewArtifactExport(
      await this.client.requestJson(
        `/v1/reviews/${encodeURIComponent(this.id)}/artifacts/export`,
        {
          method: "POST",
          body: options,
        },
      ),
    );
  }

  async cancel(reason?: string | ReviewCancelOptions): Promise<void> {
    const cancelReason =
      typeof reason === "string" ? reason : reason?.reason ?? "cancelled";
    await this.client.requestJson(
      `/v1/reviews/${encodeURIComponent(this.id)}/cancel`,
      {
        method: "POST",
        body: { reason: cancelReason },
      },
    );
    this.currentStatus = "cancelled";
  }

  async refresh(): Promise<ReviewSessionSnapshot> {
    const snapshot = unwrapReviewSnapshot(
      await this.client.requestJson(`/v1/reviews/${encodeURIComponent(this.id)}`),
    );
    this.currentStatus = snapshot.status;
    this.currentResult = snapshot.result;
    return snapshot;
  }
}

class RunnerBackedMuzen implements Muzen {
  private readonly sessions = new Map<string, RunnerBackedReviewSession>();
  readonly webhooks: MuzenWebhooks;

  constructor(private readonly runner: RunnerStdioClient) {
    this.webhooks = new RunnerBackedMuzenWebhooks(runner);
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

class UnsupportedWorkspaceProfileCollection<Input, Profile>
  implements WorkspaceProfileCollection<Input, Profile>
{
  constructor(private readonly kind: string) {}

  set(_name: string, _input: Input): Promise<Profile> {
    return Promise.reject(this.error());
  }

  get(_name: string): Promise<Profile | undefined> {
    return Promise.reject(this.error());
  }

  list(): Promise<Profile[]> {
    return Promise.reject(this.error());
  }

  private error(): MuzenUnsupportedFeatureError {
    return new MuzenUnsupportedFeatureError(
      `workspace ${this.kind} profiles require remote workspace storage; createMuzen() only supports local runner review execution in this preview`,
    );
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

async function pollUntilResult(
  load: () => Promise<ReviewResult | undefined>,
  signal: AbortSignal | undefined,
): Promise<ReviewResult> {
  while (true) {
    throwIfAborted(signal);
    const result = await load();
    if (result) {
      return result;
    }
    await delay(250, signal);
  }
}

function delay(ms: number, signal: AbortSignal | undefined): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        reject(new Error("operation aborted"));
      },
      { once: true },
    );
  });
}

function unwrapReviewSnapshot(value: unknown): ReviewSessionSnapshot {
  const snapshot = isRecord(value) && isRecord(value.review) ? value.review : value;
  if (!isReviewSessionSnapshot(snapshot)) {
    throw new Error("Muzen remote returned an invalid review session snapshot");
  }
  return snapshot;
}

function unwrapOptionalReviewResult(value: unknown): ReviewResult | undefined {
  if (value === undefined || value === null) {
    return undefined;
  }
  const result = isRecord(value) && "result" in value ? value.result : value;
  if (result === undefined || result === null) {
    return undefined;
  }
  if (!isReviewResult(result)) {
    throw new Error("Muzen remote returned an invalid review result");
  }
  return result;
}

function unwrapReviewEvents(value: unknown): ReviewEvent[] {
  const events = isRecord(value) && Array.isArray(value.events) ? value.events : value;
  if (!Array.isArray(events) || !events.every(isReviewEvent)) {
    throw new Error("Muzen remote returned invalid review events");
  }
  return events;
}

function unwrapReviewArtifact(value: unknown): ReviewArtifact {
  const artifact = isRecord(value) && isRecord(value.artifact) ? value.artifact : value;
  if (!isReviewArtifact(artifact)) {
    throw new Error("Muzen remote returned an invalid review artifact");
  }
  return artifact;
}

function unwrapReviewArtifactExport(value: unknown): ReviewArtifactExport {
  if (!isReviewArtifactExport(value)) {
    throw new Error("Muzen remote returned an invalid artifact export");
  }
  return value;
}

function unwrapModelProfile(value: unknown): ModelProfile {
  const profile = isRecord(value) && isRecord(value.profile) ? value.profile : value;
  if (!isModelProfile(profile)) {
    throw new Error("Muzen remote returned an invalid model profile");
  }
  return profile;
}

function unwrapModelProfiles(value: unknown): ModelProfile[] {
  const profiles = isRecord(value) && Array.isArray(value.profiles) ? value.profiles : value;
  if (!Array.isArray(profiles) || !profiles.every(isModelProfile)) {
    throw new Error("Muzen remote returned invalid model profiles");
  }
  return profiles;
}

function unwrapProviderProfile(value: unknown): ProviderProfile {
  const profile = isRecord(value) && isRecord(value.profile) ? value.profile : value;
  if (!isProviderProfile(profile)) {
    throw new Error("Muzen remote returned an invalid provider profile");
  }
  return profile;
}

function unwrapProviderProfiles(value: unknown): ProviderProfile[] {
  const profiles = isRecord(value) && Array.isArray(value.profiles) ? value.profiles : value;
  if (!Array.isArray(profiles) || !profiles.every(isProviderProfile)) {
    throw new Error("Muzen remote returned invalid provider profiles");
  }
  return profiles;
}

function unwrapWebhookHttpResponse(value: unknown): {
  statusCode: number;
  headers: Record<string, string>;
  body: string;
} {
  if (
    isRecord(value) &&
    typeof value.statusCode === "number" &&
    isRecord(value.headers) &&
    typeof value.body === "string"
  ) {
    return {
      statusCode: value.statusCode,
      headers: stringRecord(value.headers),
      body: value.body,
    };
  }
  throw new Error("muzen-runner returned an invalid webhook response");
}

function stringRecord(value: Record<string, unknown>): Record<string, string> {
  const result: Record<string, string> = {};
  for (const [key, item] of Object.entries(value)) {
    if (typeof item === "string") {
      result[key] = item;
    }
  }
  return result;
}

function isReviewSessionSnapshot(value: unknown): value is ReviewSessionSnapshot {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    isReviewStatus(value.status) &&
    isReviewSource(value.source) &&
    (value.result === undefined || isReviewResult(value.result))
  );
}

function isReviewSource(value: unknown): value is ReviewSource {
  return (
    isRecord(value) &&
    (value.type === "local" ||
      value.type === "github_pull_request" ||
      value.type === "gitlab_merge_request")
  );
}

function isReviewResult(value: unknown): value is ReviewResult {
  return (
    isRecord(value) &&
    typeof value.reviewId === "string" &&
    typeof value.sessionId === "string" &&
    isReviewStatus(value.status) &&
    (value.conclusion === "approved" ||
      value.conclusion === "commented" ||
      value.conclusion === "changes_requested") &&
    typeof value.summary === "string" &&
    Array.isArray(value.findings) &&
    isRecord(value.coverage)
  );
}

function isReviewEvent(value: unknown): value is ReviewEvent {
  return (
    isRecord(value) &&
    typeof value.cursor === "string" &&
    typeof value.type === "string" &&
    typeof value.reviewId === "string" &&
    typeof value.timestampUtc === "string"
  );
}

function isReviewArtifact(value: unknown): value is ReviewArtifact {
  return (
    isRecord(value) &&
    typeof value.artifactId === "string" &&
    typeof value.bytes === "number" &&
    typeof value.contentHash === "string" &&
    typeof value.content === "string"
  );
}

function isReviewArtifactExport(value: unknown): value is ReviewArtifactExport {
  return (
    isRecord(value) &&
    (value.view === "redacted" || value.view === "raw") &&
    typeof value.artifactCount === "number" &&
    typeof value.totalBytes === "number" &&
    Array.isArray(value.artifacts) &&
    value.artifacts.every(isReviewArtifact)
  );
}

function isReviewStatus(value: unknown): value is ReviewStatus {
  return (
    value === "created" ||
    value === "queued" ||
    value === "running" ||
    value === "completed" ||
    value === "failed" ||
    value === "cancelled"
  );
}

function isModelProfile(value: unknown): value is ModelProfile {
  return (
    isRecord(value) &&
    typeof value.workspaceId === "string" &&
    typeof value.name === "string" &&
    typeof value.version === "string" &&
    isModelProviderKind(value.provider) &&
    typeof value.model === "string" &&
    typeof value.updatedAtUtc === "string"
  );
}

function isProviderProfile(value: unknown): value is ProviderProfile {
  return (
    isRecord(value) &&
    typeof value.workspaceId === "string" &&
    typeof value.name === "string" &&
    typeof value.version === "string" &&
    isSourceProviderKind(value.provider) &&
    typeof value.updatedAtUtc === "string"
  );
}

function isModelProviderKind(value: unknown): value is ModelProfile["provider"] {
  return (
    value === "openai" ||
    value === "anthropic" ||
    value === "openai_compatible"
  );
}

function isSourceProviderKind(value: unknown): value is ProviderProfile["provider"] {
  return value === "github" || value === "gitlab";
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

function mapRunnerArtifact(artifact: RunnerArtifact): ReviewArtifact {
  return {
    artifactId: artifact.artifactId,
    bytes: artifact.bytes,
    contentHash: artifact.contentHash,
    content: artifact.content,
  };
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

function isRunnerArtifactReadResult(value: unknown): value is RunnerArtifactReadResult {
  return (
    isRecord(value) &&
    typeof value.runId === "string" &&
    (value.view === "redacted" || value.view === "raw") &&
    isRunnerArtifact(value.artifact)
  );
}

function isRunnerArtifactExportResult(value: unknown): value is RunnerArtifactExportResult {
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
