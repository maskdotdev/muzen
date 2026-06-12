import { MuzenUnsupportedFeatureError } from "./errors.js";
import {
  isCallbackReviewModelSpec,
  isHostedReviewModelSpec,
} from "./models.js";
import {
  parseTimeoutMs,
  pollUntilResult,
  throwIfAborted,
  withTimeout,
} from "./review-flow.js";
import { parseReviewSource } from "./sources.js";
import { UnsupportedMuzenWorkers } from "./unsupported.js";
import {
  unwrapModelProfile,
  unwrapModelProfiles,
  unwrapOptionalReviewResult,
  unwrapProviderProfile,
  unwrapProviderProfiles,
  unwrapReviewArtifact,
  unwrapReviewArtifactExport,
  unwrapReviewEvents,
  unwrapReviewSnapshot,
} from "./wire-validation.js";
import type {
  CreateMuzenClientOptions,
  ModelProfile,
  ModelProfileInput,
  Muzen,
  MuzenWorkers,
  MuzenWebhookHandler,
  MuzenWebhookProvider,
  MuzenWebhookResponseOptions,
  MuzenWebhooks,
  MuzenWorkspace,
  ProviderProfile,
  ProviderProfileInput,
  ReviewArtifact,
  ReviewArtifactExport,
  ReviewArtifactExportOptions,
  ReviewArtifactReadOptions,
  ReviewCancelOptions,
  ReviewEvent,
  ReviewModelSpec,
  ReviewOptions,
  ReviewResult,
  ReviewSession,
  ReviewSessionSnapshot,
  ReviewSource,
  ReviewSourceLike,
  ReviewStatus,
  SwarmOptions,
  SwarmResult,
  WorkspaceProfileCollection,
} from "./types.js";

export class RemoteMuzen implements Muzen {
  private readonly baseUrl: URL;
  private readonly fetch: typeof globalThis.fetch;
  readonly webhooks: MuzenWebhooks;
  readonly workers: MuzenWorkers;

  constructor(private readonly options: CreateMuzenClientOptions) {
    this.baseUrl = new URL(options.baseUrl);
    this.fetch = options.fetch ?? globalThis.fetch;
    if (!this.fetch) {
      throw new MuzenUnsupportedFeatureError(
        "createMuzenClient({ baseUrl }) requires a fetch implementation",
      );
    }
    this.webhooks = new RemoteMuzenWebhooks(this);
    this.workers = new UnsupportedMuzenWorkers(
      "remote workers are managed by the Muzen service; run workers from a host process created with createMuzen()",
    );
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
    const options = input.options ?? {};
    const response = await this.requestJson("/v1/reviews", {
      method: "POST",
      body: {
        source,
        options: remoteReviewOptions(options),
      },
      signal: options.signal,
    });
    const snapshot = unwrapReviewSnapshot(response);
    return new RemoteReviewSession(this, snapshot);
  }

  async runSwarm(_options: SwarmOptions): Promise<SwarmResult> {
    throw new MuzenUnsupportedFeatureError(
      "remote swarm runs are not available yet; use createMuzen() to run swarms against a local muzen-runner",
    );
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
  readonly providers: WorkspaceProfileCollection<
    ProviderProfileInput,
    ProviderProfile
  >;

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
          options: remoteReviewOptions(options),
        },
        signal: options.signal,
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
    const resultPromise = pollUntilResult(
      () => this.result(options),
      options.signal,
    );
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

function remoteReviewOptions(options: ReviewOptions): ReviewOptions {
  const {
    hooks: _hooks,
    signal: _signal,
    heartbeat: _heartbeat,
    sourceProvider,
    model,
    tools,
    ...serializable
  } = options;
  return {
    ...serializable,
    model: remoteModel(model),
    sessions: serializable.sessions?.map(remoteSession),
    sourceProvider: sourceProvider?.baseUrl
      ? { baseUrl: sourceProvider.baseUrl }
      : undefined,
    tools: tools?.map(({ handler: _handler, ...tool }) => tool),
  };
}

function remoteSession(
  session: NonNullable<ReviewOptions["sessions"]>[number],
): NonNullable<ReviewOptions["sessions"]>[number] {
  return {
    ...session,
    model: remoteModel(session.model),
  };
}

function remoteModel(
  model: ReviewOptions["model"],
): ReviewModelSpec | undefined {
  if (isHostedReviewModelSpec(model)) {
    return model;
  }
  if (isCallbackReviewModelSpec(model)) {
    return undefined;
  }
  return undefined;
}
