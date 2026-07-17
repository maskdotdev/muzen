import { execFileSync } from "node:child_process";

import { RunnerBackedMuzen } from "./local.js";
import {
  RunnerStdioClient,
  type RunnerClient,
  type RunnerNotificationListener,
  type RunnerRequestHandler,
} from "./protocol.js";
import { RemoteMuzen } from "./remote.js";
import type {
  CreateMuzenClientOptions,
  CreateMuzenOptions,
  CreateReviewSessionInput,
  CreateReviewSessionResult,
  Muzen,
} from "./types.js";

export { MuzenUnsupportedFeatureError } from "./errors.js";

const DEFAULT_RECYCLE_OPTIONS = {
  rssBytes: 512 * 1024 * 1024,
  completedRuns: 100,
  totalTokens: 25_000_000,
};

export async function createMuzen(
  options: CreateMuzenOptions = {},
): Promise<Muzen> {
  const runner = new RecyclableRunnerClient({
    runnerPath:
      options.runnerPath ?? process.env.MUZEN_RUNNER_PATH ?? "muzen-runner",
    runnerArgs: options.runnerArgs ?? ["stdio"],
    clientName: options.clientName ?? "@muzen/sdk",
    clientVersion: options.clientVersion,
    recycle: options.runnerRecycle,
  });
  await runner.start();
  return new RunnerBackedMuzen(runner, {
    secrets: options.secrets,
  });
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

class RecyclableRunnerClient implements RunnerClient {
  private current?: RunnerStdioClient;
  private readonly notificationListeners = new Map<RunnerNotificationListener, () => void>();
  private readonly requestHandlers = new Map<string, {
    handler: RunnerRequestHandler;
    unsubscribe?: () => void;
  }>();
  private activeRequests = 0;
  private completedRuns = 0;
  private totalTokens = 0;
  private recycleGate: Promise<void> | undefined;
  private closed = false;

  constructor(
    private readonly options: {
      runnerPath: string;
      runnerArgs: string[];
      clientName: string;
      clientVersion?: string;
      recycle?: false | Partial<{
        rssBytes: number;
        completedRuns: number;
        totalTokens: number;
      }>;
    },
  ) {}

  get pid(): number | undefined {
    return this.current?.pid;
  }

  async start(): Promise<void> {
    this.current = await this.spawnRunner();
  }

  async handshake(input: {
    clientName?: string;
    clientVersion?: string;
  } = {}): Promise<unknown> {
    return this.runner().handshake(input);
  }

  async request(method: string, params?: unknown): Promise<unknown> {
    if (this.closed) {
      throw new Error("muzen-runner client is closed");
    }
    while (this.recycleGate) {
      await this.recycleGate;
    }
    this.activeRequests += 1;
    try {
      const result = await this.runner().request(method, params);
      if (method === "run.start") {
        this.completedRuns += 1;
        this.totalTokens += totalTokensFromRunResult(result);
      }
      return result;
    } finally {
      this.activeRequests -= 1;
      if (method === "run.release") {
        void this.maybeRecycle();
      }
    }
  }

  onNotification(listener: RunnerNotificationListener): () => void {
    const unsubscribe = this.runner().onNotification(listener);
    this.notificationListeners.set(listener, unsubscribe);
    return () => {
      this.notificationListeners.get(listener)?.();
      this.notificationListeners.delete(listener);
    };
  }

  onRequest(method: string, handler: RunnerRequestHandler): () => void {
    const existing = this.requestHandlers.get(method);
    existing?.unsubscribe?.();
    const entry = {
      handler,
      unsubscribe: this.runner().onRequest(method, handler),
    };
    this.requestHandlers.set(method, entry);
    return () => {
      const current = this.requestHandlers.get(method);
      if (current?.handler === handler) {
        current.unsubscribe?.();
        this.requestHandlers.delete(method);
      }
    };
  }

  async close(): Promise<void> {
    this.closed = true;
    await this.recycleGate;
    await this.current?.close();
  }

  private runner(): RunnerStdioClient {
    if (!this.current) {
      throw new Error("muzen-runner has not started");
    }
    return this.current;
  }

  private async spawnRunner(): Promise<RunnerStdioClient> {
    const runner = new RunnerStdioClient({
      runnerPath: this.options.runnerPath,
      runnerArgs: this.options.runnerArgs,
    });
    await runner.handshake({
      clientName: this.options.clientName,
      clientVersion: this.options.clientVersion,
    });
    for (const listener of this.notificationListeners.keys()) {
      this.notificationListeners.set(listener, runner.onNotification(listener));
    }
    for (const [method, entry] of this.requestHandlers) {
      entry.unsubscribe = runner.onRequest(method, entry.handler);
    }
    return runner;
  }

  private maybeRecycle(): Promise<void> | undefined {
    if (this.options.recycle === false || this.closed) {
      return undefined;
    }
    if (this.recycleGate) {
      return this.recycleGate;
    }
    this.recycleGate = this.recycleIfIdle().finally(() => {
      this.recycleGate = undefined;
    });
    return this.recycleGate;
  }

  private async recycleIfIdle(): Promise<void> {
    if (this.activeRequests !== 0) {
      return;
    }
    const runner = this.runner();
    const debugState = await runner.request("runner.debugState");
    if (!isRunnerIdle(debugState)) {
      return;
    }
    const recycle = {
      ...DEFAULT_RECYCLE_OPTIONS,
      ...(this.options.recycle ?? {}),
    };
    const rssBytes = runner.pid ? currentRssBytes(runner.pid) : 0;
    const shouldRecycle =
      rssBytes > recycle.rssBytes ||
      this.completedRuns >= recycle.completedRuns ||
      this.totalTokens >= recycle.totalTokens;
    if (!shouldRecycle) {
      return;
    }
    await runner.close();
    this.current = await this.spawnRunner();
    this.completedRuns = 0;
    this.totalTokens = 0;
  }
}

function isRunnerIdle(value: unknown): boolean {
  if (!isRecord(value)) {
    return false;
  }
  return (
    value.activeRuns === 0 &&
    value.reports === 0 &&
    value.runThreads === 0
  );
}

function totalTokensFromRunResult(value: unknown): number {
  if (!isRecord(value) || !isRecord(value.summary)) {
    return 0;
  }
  return typeof value.summary.totalTokens === "number"
    ? value.summary.totalTokens
    : 0;
}

function currentRssBytes(pid: number): number {
  try {
    const output = execFileSync("ps", ["-o", "rss=", "-p", String(pid)], {
      encoding: "utf8",
    }).trim();
    const rssKb = Number(output);
    return Number.isFinite(rssKb) ? rssKb * 1024 : 0;
  } catch {
    return 0;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
