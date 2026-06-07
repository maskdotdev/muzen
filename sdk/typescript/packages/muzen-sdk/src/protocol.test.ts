import { afterEach, describe, it } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { RunnerStdioClient, type JsonRpcNotification } from "./protocol.js";
import { registerReviewCallbacks } from "./runner-callbacks.js";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(
    tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

describe("runner stdio client", () => {
  it("responds to runner callback requests", async () => {
    const dir = await mkdtemp(join(tmpdir(), "muzen-protocol-"));
    tempDirs.push(dir);
    const script = join(dir, "runner-callback.js");
    await writeFile(
      script,
      `
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const response = JSON.parse(line);
  process.stdout.write(JSON.stringify({
    jsonrpc: "2.0",
    method: "event.review",
    params: {
      seq: 1,
      timestampUtc: "2026-06-05T00:00:00Z",
      runId: "review-1",
      event: { callbackResponse: response.result }
    }
  }) + "\\n");
  setTimeout(() => process.exit(0), 10);
});
process.stdout.write(JSON.stringify({
  jsonrpc: "2.0",
  id: "callback-1",
  method: "tool.execute",
  params: {
    runId: "review-1",
    sessionId: "security",
    turn: 1,
    callId: "call-1",
    toolId: "argus.issue_context",
    snapshotId: "snapshot-1",
    providerResources: ["issue:123"],
    arguments: { issue: 123 }
  }
}) + "\\n");
`,
    );
    const client = new RunnerStdioClient({
      runnerPath: process.execPath,
      runnerArgs: [script],
    });
    const notification = waitForNotification(client);
    const unsubscribe = client.onRequest("tool.execute", (params) => ({
      data: {
        toolId: (params as { toolId: string }).toolId,
        arguments: (params as { arguments: unknown }).arguments,
      },
    }));

    const event = await notification;

    unsubscribe();
    await client.close();
    assert.deepEqual(event.params, {
      seq: 1,
      timestampUtc: "2026-06-05T00:00:00Z",
      runId: "review-1",
      event: {
        callbackResponse: {
          data: {
            toolId: "argus.issue_context",
            arguments: { issue: 123 },
          },
        },
      },
    });
  });

  it("materializes sources through registered source provider callbacks", async () => {
    const dir = await mkdtemp(join(tmpdir(), "muzen-source-callback-"));
    tempDirs.push(dir);
    const script = join(dir, "runner-source-callback.js");
    await writeFile(
      script,
      `
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const response = JSON.parse(line);
  process.stdout.write(JSON.stringify({
    jsonrpc: "2.0",
    method: "event.review",
    params: {
      seq: 1,
      timestampUtc: "2026-06-05T00:00:00Z",
      runId: "review-1",
      event: { materialized: response.result }
    }
  }) + "\\n");
  setTimeout(() => process.exit(0), 10);
});
process.stdout.write(JSON.stringify({
  jsonrpc: "2.0",
  id: "callback-1",
  method: "source.materialize",
  params: {
    protocolVersion: "muzen.runner.v1",
    source: {
      type: "custom",
      provider: "acme",
      id: "review-123"
    },
    changedFiles: ["src/lib.rs"]
  }
}) + "\\n");
`,
    );
    const client = new RunnerStdioClient({
      runnerPath: process.execPath,
      runnerArgs: [script],
    });
    const notification = waitForNotification(client);
    const unsubscribe = registerReviewCallbacks(client, {
      sourceProvider: {
        handler: (request) => ({
          root: `/bundle/${request.source.type}`,
          changedFiles: request.changedFiles,
        }),
      },
    });

    const event = await notification;

    unsubscribe();
    await client.close();
    assert.deepEqual(event.params, {
      seq: 1,
      timestampUtc: "2026-06-05T00:00:00Z",
      runId: "review-1",
      event: {
        materialized: {
          root: "/bundle/custom",
          changedFiles: ["src/lib.rs"],
        },
      },
    });
  });

  it("renews active runs through registered heartbeat callbacks", async () => {
    const dir = await mkdtemp(join(tmpdir(), "muzen-heartbeat-callback-"));
    tempDirs.push(dir);
    const script = join(dir, "runner-heartbeat-callback.js");
    await writeFile(
      script,
      `
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const response = JSON.parse(line);
  process.stdout.write(JSON.stringify({
    jsonrpc: "2.0",
    method: "event.review",
    params: {
      seq: 1,
      timestampUtc: "2026-06-05T00:00:00Z",
      runId: "review-1",
      event: { heartbeat: response.result }
    }
  }) + "\\n");
  setTimeout(() => process.exit(0), 10);
});
process.stdout.write(JSON.stringify({
  jsonrpc: "2.0",
  id: "callback-1",
  method: "run.heartbeat",
  params: {
    protocolVersion: "muzen.runner.v1",
    runId: "review-1",
    sequence: 2,
    elapsedMs: 1500,
    leaseSeconds: 30
  }
}) + "\\n");
`,
    );
    const client = new RunnerStdioClient({
      runnerPath: process.execPath,
      runnerArgs: [script],
    });
    const notification = waitForNotification(client);
    const seen: unknown[] = [];
    const unsubscribe = registerReviewCallbacks(client, {
      hooks: {
        onHeartbeat: (heartbeat) => {
          seen.push(heartbeat);
          return { continueRun: false };
        },
      },
    });

    const event = await notification;

    unsubscribe();
    await client.close();
    assert.deepEqual(event.params, {
      seq: 1,
      timestampUtc: "2026-06-05T00:00:00Z",
      runId: "review-1",
      event: { heartbeat: { continueRun: false } },
    });
    assert.deepEqual(seen, [
      {
        runId: "review-1",
        sequence: 2,
        elapsedMs: 1500,
        leaseSeconds: 30,
        signal: undefined,
      },
    ]);
  });
});

function waitForNotification(
  client: RunnerStdioClient,
): Promise<JsonRpcNotification> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      unsubscribe();
      reject(new Error("timed out waiting for runner notification"));
    }, 2_000);
    const unsubscribe = client.onNotification((notification) => {
      clearTimeout(timeout);
      unsubscribe();
      resolve(notification);
    });
  });
}
