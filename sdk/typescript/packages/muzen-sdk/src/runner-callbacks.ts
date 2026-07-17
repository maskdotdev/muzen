import type { RunnerClient } from "./protocol.js";
import { throwIfAborted } from "./review-flow.js";
import type {
  ReviewCallbackModelSpec,
  ReviewModelRequest,
  ReviewModelResult,
  ReviewHeartbeatHandler,
  MuzenSecretResolverOptions,
  ReviewOptions,
  ReviewSourceMaterializeRequest,
  ReviewSourceMaterializeResult,
  ReviewTool,
  ReviewToolResult,
} from "./types.js";

export function registerReviewCallbacks(
  runner: RunnerClient,
  options: ReviewOptions,
  localOptions: {
    secrets?: MuzenSecretResolverOptions;
    tools?: ReviewTool[];
  } = {},
): () => void {
  const unsubscribeSource = registerSourceMaterializeHandler(
    runner,
    options.sourceProvider,
    options.signal,
  );
  const unsubscribeModel = registerReviewModelHandler(
    runner,
    options.model,
    options.signal,
  );
  const unsubscribeTools = registerReviewToolHandlers(
    runner,
    localOptions.tools ?? [],
    options.signal,
  );
  const unsubscribeHeartbeat = registerReviewHeartbeatHandler(
    runner,
    options.hooks?.onHeartbeat,
    options.signal,
  );
  const unsubscribeSecrets = registerSecretResolver(
    runner,
    localOptions.secrets,
    options.signal,
  );
  return () => {
    unsubscribeSource();
    unsubscribeModel();
    unsubscribeTools();
    unsubscribeHeartbeat();
    unsubscribeSecrets();
  };
}

function registerSourceMaterializeHandler(
  runner: RunnerClient,
  sourceProvider: ReviewOptions["sourceProvider"],
  signal?: AbortSignal,
): () => void {
  const handler = sourceProvider?.handler;
  if (!handler) {
    return () => {};
  }
  return runner.onRequest("source.materialize", async (params) => {
    throwIfAborted(signal);
    const result = normalizeSourceMaterializeResult(
      await handler({
        ...parseSourceMaterializeRequest(params),
        signal,
      }),
    );
    throwIfAborted(signal);
    return result;
  });
}

function parseSourceMaterializeRequest(
  params: unknown,
): ReviewSourceMaterializeRequest {
  if (!isRecord(params)) {
    throw new Error("source.materialize params must be an object");
  }
  const changedFiles = params.changedFiles;
  if (
    !isRecord(params.source) ||
    !Array.isArray(changedFiles) ||
    !changedFiles.every((path) => typeof path === "string")
  ) {
    throw new Error("source.materialize params are invalid");
  }
  return {
    source: params.source as unknown as ReviewSourceMaterializeRequest["source"],
    changedFiles,
  };
}

function normalizeSourceMaterializeResult(
  result: ReviewSourceMaterializeResult,
): unknown {
  return {
    root: result.root,
    changedFiles: result.changedFiles ?? [],
  };
}

function registerReviewModelHandler(
  runner: RunnerClient,
  model: ReviewOptions["model"],
  signal?: AbortSignal,
): () => void {
  if (!isCallbackModel(model)) {
    return () => {};
  }
  return runner.onRequest("model.complete", async (params) => {
    throwIfAborted(signal);
    const result = normalizeModelResult(
      await model.handler({
        ...parseModelCompleteRequest(params),
        signal,
      }),
    );
    throwIfAborted(signal);
    return result;
  });
}

function isCallbackModel(
  model: ReviewOptions["model"],
): model is ReviewCallbackModelSpec {
  return typeof model === "object" && model !== null && model.kind === "callback";
}

function parseModelCompleteRequest(params: unknown): ReviewModelRequest {
  if (!isRecord(params)) {
    throw new Error("model.complete params must be an object");
  }
  const transcript = params.transcript;
  if (
    typeof params.runId !== "string" ||
    typeof params.sessionId !== "string" ||
    typeof params.role !== "string" ||
    typeof params.objective !== "string" ||
    typeof params.turn !== "number" ||
    !Array.isArray(transcript)
  ) {
    throw new Error("model.complete params are invalid");
  }
  return {
    runId: params.runId,
    sessionId: params.sessionId,
    role: params.role as ReviewModelRequest["role"],
    objective: params.objective,
    snapshotId: optionalString(params.snapshotId),
    modelProfileId: optionalString(params.modelProfileId),
    turn: params.turn,
    transcript,
  };
}

function normalizeModelResult(result: ReviewModelResult): unknown {
  return {
    content: result.content,
    toolCalls: result.toolCalls ?? [],
    usage: result.usage,
  };
}

function registerReviewToolHandlers(
  runner: RunnerClient,
  tools: ReviewTool[],
  signal?: AbortSignal,
): () => void {
  const handlers = new Map(
    tools
      .filter((tool) => tool.handler)
      .map((tool) => [tool.id, tool.handler] as const),
  );
  if (handlers.size === 0) {
    return () => {};
  }
  return runner.onRequest("tool.execute", async (params) => {
    throwIfAborted(signal);
    const request = parseToolExecuteRequest(params);
    const handler = handlers.get(request.toolId);
    if (!handler) {
      throw new Error(`no handler registered for review tool ${request.toolId}`);
    }
    const result = normalizeToolResult(
      await handler(
        {
          runId: request.runId,
          sessionId: request.sessionId,
          turn: request.turn,
          callId: request.callId,
          toolId: request.toolId,
          snapshotId: request.snapshotId,
          providerResources: request.providerResources,
          signal,
        },
        request.arguments,
      ),
    );
    throwIfAborted(signal);
    return result;
  });
}

interface ToolExecuteRequest {
  runId: string;
  sessionId: string;
  turn: number;
  callId: string;
  toolId: string;
  snapshotId: string;
  providerResources: string[];
  arguments: unknown;
}

function parseToolExecuteRequest(params: unknown): ToolExecuteRequest {
  if (!isRecord(params)) {
    throw new Error("tool.execute params must be an object");
  }
  const providerResources = params.providerResources;
  if (
    typeof params.runId !== "string" ||
    typeof params.sessionId !== "string" ||
    typeof params.turn !== "number" ||
    typeof params.callId !== "string" ||
    typeof params.toolId !== "string" ||
    typeof params.snapshotId !== "string" ||
    !Array.isArray(providerResources) ||
    !providerResources.every((resource) => typeof resource === "string")
  ) {
    throw new Error("tool.execute params are invalid");
  }
  return {
    runId: params.runId,
    sessionId: params.sessionId,
    turn: params.turn,
    callId: params.callId,
    toolId: params.toolId,
    snapshotId: params.snapshotId,
    providerResources,
    arguments: params.arguments,
  };
}

function normalizeToolResult(result: ReviewToolResult): unknown {
  return {
    data: result.data,
    artifact: result.artifact,
  };
}

function registerReviewHeartbeatHandler(
  runner: RunnerClient,
  handler: ReviewHeartbeatHandler | undefined,
  signal?: AbortSignal,
): () => void {
  if (!handler) {
    return () => {};
  }
  return runner.onRequest("run.heartbeat", async (params) => {
    throwIfAborted(signal);
    const result = await handler({
      ...parseHeartbeatRequest(params),
      signal,
    });
    throwIfAborted(signal);
    return normalizeHeartbeatResult(result);
  });
}

function parseHeartbeatRequest(params: unknown) {
  if (!isRecord(params)) {
    throw new Error("run.heartbeat params must be an object");
  }
  if (
    typeof params.runId !== "string" ||
    typeof params.sequence !== "number" ||
    typeof params.elapsedMs !== "number"
  ) {
    throw new Error("run.heartbeat params are invalid");
  }
  return {
    runId: params.runId,
    sequence: params.sequence,
    elapsedMs: params.elapsedMs,
    leaseSeconds:
      typeof params.leaseSeconds === "number" ? params.leaseSeconds : undefined,
  };
}

function normalizeHeartbeatResult(
  result: boolean | { continueRun?: boolean } | void,
): unknown {
  if (typeof result === "boolean") {
    return { continueRun: result };
  }
  return { continueRun: result?.continueRun ?? true };
}

function registerSecretResolver(
  runner: RunnerClient,
  secrets: MuzenSecretResolverOptions | undefined,
  signal?: AbortSignal,
): () => void {
  const resolver = secrets?.resolve;
  if (!resolver) {
    return () => {};
  }
  return runner.onRequest("secret.resolve", async (params) => {
    throwIfAborted(signal);
    const ref = parseSecretResolveRequest(params);
    const value = await resolver(ref);
    throwIfAborted(signal);
    if (typeof value !== "string" || value.length === 0) {
      throw new Error("secret resolver must return a non-empty string");
    }
    return { value };
  });
}

function parseSecretResolveRequest(params: unknown): string {
  if (!isRecord(params)) {
    throw new Error("secret.resolve params must be an object");
  }
  const ref = params.ref;
  if (typeof ref !== "string" || ref.trim().length === 0) {
    throw new Error("secret.resolve params are invalid");
  }
  return ref;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}
