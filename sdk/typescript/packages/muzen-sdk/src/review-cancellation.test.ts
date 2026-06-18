import { afterEach, describe, it } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { createMuzen, createMuzenClient, local, type Muzen } from "./index.js";

const tempDirs: string[] = [];
let muzen: Muzen | undefined;

afterEach(async () => {
  await muzen?.close();
  muzen = undefined;
  await Promise.all(
    tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

describe("review cancellation", () => {
  it("passes the review signal into callback models and rejects when aborted", async () => {
    const runnerScript = await writeCallbackModelRunner();
    const controller = new AbortController();
    let sawSignal = false;
    muzen = await createMuzen({
      runnerPath: process.execPath,
      runnerArgs: [runnerScript],
    });

    await assert.rejects(
      () =>
        muzen!.review(local("/repo"), {
          signal: controller.signal,
          model: {
            kind: "callback",
            handler: (request) => {
              sawSignal = request.signal === controller.signal;
              controller.abort();
              return { content: "cancelled" };
            },
          },
        }),
      /operation aborted/,
    );
    assert.equal(sawSignal, true);
  });

  it("sends run.cancel when a local review signal aborts during run.start", async () => {
    const runnerScript = await writeAbortAwareRunner();
    const controller = new AbortController();
    muzen = await createMuzen({
      runnerPath: process.execPath,
      runnerArgs: [runnerScript],
    });

    await assert.rejects(
      () =>
        withTimeout(
          muzen!.review(local("/repo"), {
            signal: controller.signal,
            model: {
              kind: "callback",
              handler: () => {
                controller.abort();
                return { content: "cancelled" };
              },
            },
          }),
          1_000,
        ),
      /cancelled|operation aborted/,
    );
  });

  it("uses review signals for remote requests without serializing local callbacks", async () => {
    const controller = new AbortController();
    const requests: Array<{ signal: AbortSignal | null; body: unknown }> = [];
    const remote = createMuzenClient({
      baseUrl: "https://muzen.example",
      fetch: async (_input, init = {}) => {
        const body =
          typeof init.body === "string" ? JSON.parse(init.body) : undefined;
        requests.push({
          signal: (init.signal as AbortSignal | null) ?? null,
          body,
        });
        return Response.json({
          review: {
            id: "review-remote-1",
            status: "queued",
            source: body.source,
          },
        });
      },
    });

    await remote.review(local("/repo"), {
      signal: controller.signal,
      hooks: { onEvent: () => {} },
      sourceProvider: {
        baseUrl: "https://source.example",
        handler: () => ({ root: "/bundle" }),
      },
    });

    const body = requests[0].body as { options: Record<string, unknown> };
    assert.equal(requests[0].signal, controller.signal);
    assert.deepEqual(body.options, {
      sourceProvider: { baseUrl: "https://source.example" },
    });
  });
});

async function writeCallbackModelRunner(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "muzen-review-cancel-"));
  tempDirs.push(dir);
  const script = join(dir, "runner.js");
  await writeFile(
    script,
    `
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
let pendingRunStart = null;

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\\n");
}

function runResult(runId) {
  return {
    protocolVersion: "muzen.runner.v1",
    runId,
    status: "completed",
    summary: {
      sessions: 1,
      completedSessions: 1,
      modelCalls: 1,
      toolCalls: 0,
      totalTokens: 0
    },
    findings: [],
    snapshots: [{ files: 1, capturedFiles: 1 }],
    metadata: {}
  };
}

rl.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === "runner.handshake") {
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: {
        protocolVersion: "muzen.runner.v1",
        runnerName: "fake-cancellation-runner",
        runnerVersion: "0.0.0",
        capabilities: {}
      }
    });
    return;
  }
  if (message.method === "run.start") {
    pendingRunStart = message;
    send({
      jsonrpc: "2.0",
      id: "model-1",
      method: "model.complete",
      params: {
        runId: message.params.runId,
        sessionId: "generalist",
        role: "generalist",
        objective: "Review the repository change.",
        turn: 1,
        transcript: []
      }
    });
    return;
  }
  if (message.id === "model-1" && pendingRunStart) {
    const runId = pendingRunStart.params.runId;
    send({
      jsonrpc: "2.0",
      id: pendingRunStart.id,
      result: runResult(runId)
    });
    setTimeout(() => process.exit(0), 20);
    return;
  }
});
`,
  );
  return script;
}

async function writeAbortAwareRunner(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "muzen-review-cancel-aware-"));
  tempDirs.push(dir);
  const script = join(dir, "runner.js");
  await writeFile(
    script,
    `
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
let pendingRunStart = null;

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\\n");
}

rl.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === "runner.handshake") {
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: {
        protocolVersion: "muzen.runner.v1",
        runnerName: "fake-abort-aware-runner",
        runnerVersion: "0.0.0",
        capabilities: {}
      }
    });
    return;
  }
  if (message.method === "run.start") {
    pendingRunStart = message;
    send({
      jsonrpc: "2.0",
      id: "model-1",
      method: "model.complete",
      params: {
        runId: message.params.runId,
        sessionId: "generalist",
        role: "generalist",
        objective: "Review the repository change.",
        turn: 1,
        transcript: []
      }
    });
    return;
  }
  if (message.method === "run.cancel" && pendingRunStart) {
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: {
        runId: message.params.runId,
        status: "cancelling",
        cancelled: true,
        reason: "operation aborted"
      }
    });
    send({
      jsonrpc: "2.0",
      method: "run.failed",
      params: {
        error: "run cancelled",
        kind: "runner_error",
        failureKind: "cancelled",
        retryHint: "not_retryable"
      }
    });
    send({
      jsonrpc: "2.0",
      id: pendingRunStart.id,
      error: {
        code: -32002,
        message: "run cancelled",
        data: { kind: "runner_error" }
      }
    });
    setTimeout(() => process.exit(0), 20);
    return;
  }
});
`,
  );
  return script;
}

async function withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_resolve, reject) => {
        timeout = setTimeout(
          () => reject(new Error("timed out waiting for abort cancellation")),
          timeoutMs,
        );
      }),
    ]);
  } finally {
    if (timeout) {
      clearTimeout(timeout);
    }
  }
}
