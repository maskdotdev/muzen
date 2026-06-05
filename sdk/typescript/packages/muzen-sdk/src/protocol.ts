import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface, type Interface as ReadlineInterface } from "node:readline";

export const RUNNER_PROTOCOL_VERSION = "muzen.runner.v1";

export interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: number;
  method: string;
  params?: unknown;
}

export interface JsonRpcResponse {
  jsonrpc: "2.0";
  id?: number | string | null;
  result?: unknown;
  error?: JsonRpcErrorShape;
}

export interface JsonRpcNotification {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
}

export interface JsonRpcErrorShape {
  code: number;
  message: string;
  data?: {
    kind?: string;
  };
}

export type RunnerNotificationListener = (
  notification: JsonRpcNotification,
) => void;

export interface RunnerClientOptions {
  runnerPath: string;
  runnerArgs?: string[];
}

export class RunnerProtocolError extends Error {
  readonly code: number;
  readonly kind?: string;

  constructor(error: JsonRpcErrorShape) {
    super(error.message);
    this.name = "RunnerProtocolError";
    this.code = error.code;
    this.kind = error.data?.kind;
  }
}

export class RunnerStdioClient {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly lines: ReadlineInterface;
  private readonly pending = new Map<
    number,
    {
      resolve(value: unknown): void;
      reject(error: Error): void;
    }
  >();
  private readonly notificationListeners = new Set<RunnerNotificationListener>();
  private nextRequestId = 1;
  private closed = false;

  constructor(options: RunnerClientOptions) {
    this.child = spawn(options.runnerPath, options.runnerArgs ?? ["stdio"], {
      stdio: "pipe",
    });
    this.child.once("error", (error) => this.rejectAll(error));
    this.child.once("exit", (code, signal) => {
      if (!this.closed) {
        this.rejectAll(
          new Error(`muzen-runner exited unexpectedly: code=${code} signal=${signal}`),
        );
      }
    });
    this.lines = createInterface({
      input: this.child.stdout,
      crlfDelay: Number.POSITIVE_INFINITY,
    });
    this.lines.on("line", (line) => this.handleLine(line));
  }

  async handshake(input: {
    clientName?: string;
    clientVersion?: string;
  } = {}): Promise<unknown> {
    return this.request("runner.handshake", {
      protocolVersion: RUNNER_PROTOCOL_VERSION,
      clientName: input.clientName ?? "@muzen/sdk",
      clientVersion: input.clientVersion,
    });
  }

  request(method: string, params?: unknown): Promise<unknown> {
    if (this.closed) {
      return Promise.reject(new Error("muzen-runner client is closed"));
    }
    const id = this.nextRequestId++;
    const request: JsonRpcRequest = {
      jsonrpc: "2.0",
      id,
      method,
      params,
    };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child.stdin.write(`${JSON.stringify(request)}\n`, (error) => {
        if (error) {
          this.pending.delete(id);
          reject(error);
        }
      });
    });
  }

  onNotification(listener: RunnerNotificationListener): () => void {
    this.notificationListeners.add(listener);
    return () => {
      this.notificationListeners.delete(listener);
    };
  }

  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.lines.close();
    this.child.stdin.end();
    if (!this.child.killed) {
      this.child.kill();
    }
    this.rejectAll(new Error("muzen-runner client closed"));
  }

  private handleLine(line: string): void {
    if (line.trim().length === 0) {
      return;
    }
    let message: JsonRpcResponse | JsonRpcNotification;
    try {
      message = JSON.parse(line) as JsonRpcResponse | JsonRpcNotification;
    } catch (error) {
      this.rejectAll(
        new Error(`invalid JSON-RPC frame from muzen-runner: ${String(error)}`),
      );
      return;
    }
    if ("method" in message) {
      for (const listener of this.notificationListeners) {
        listener(message);
      }
      return;
    }
    if (typeof message.id !== "number") {
      return;
    }
    const pending = this.pending.get(message.id);
    if (!pending) {
      return;
    }
    this.pending.delete(message.id);
    if (message.error) {
      pending.reject(new RunnerProtocolError(message.error));
      return;
    }
    pending.resolve(message.result);
  }

  private rejectAll(error: Error): void {
    for (const pending of this.pending.values()) {
      pending.reject(error);
    }
    this.pending.clear();
  }
}
