import { afterEach, describe, it } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  createMuzen,
  local,
  type Muzen,
  type ReviewEvent,
} from "./index.js";

const tempDirs: string[] = [];
let muzen: Muzen | undefined;

afterEach(async () => {
  await muzen?.close();
  muzen = undefined;
  await Promise.all(
    tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

describe("review event hooks", () => {
  it("streams runner review events to host hooks while creating a review", async () => {
    const runnerScript = await writeFakeReviewRunner();
    const observed: ReviewEvent[] = [];
    muzen = await createMuzen({
      runnerPath: process.execPath,
      runnerArgs: [runnerScript],
    });

    const review = await muzen.review(
      local("/repo", { changedFiles: ["src/lib.ts"] }),
      {
        hooks: {
          onEvent: (event) => observed.push(event),
        },
      },
    );
    const replayed: string[] = [];
    review.subscribe((event) => replayed.push(event.type));

    assert.equal(review.status, "completed");
    assert.equal(observed[0]?.reviewId, review.id);
    assert.deepEqual(observed.map((event) => event.type), [
      "session.started",
      "session.completed",
    ]);
    assert.deepEqual(replayed, ["session.started", "session.completed"]);
  });
});

async function writeFakeReviewRunner(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "muzen-review-events-"));
  tempDirs.push(dir);
  const script = join(dir, "runner.js");
  await writeFile(
    script,
    `
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\\n");
}

rl.on("line", (line) => {
  const request = JSON.parse(line);
  if (request.method === "runner.handshake") {
    send({
      jsonrpc: "2.0",
      id: request.id,
      result: {
        protocolVersion: "muzen.runner.v1",
        runnerName: "fake-review-runner",
        runnerVersion: "0.0.0",
        capabilities: {}
      }
    });
    return;
  }
  if (request.method === "run.start") {
    const runId = request.params.runId;
    send({
      jsonrpc: "2.0",
      method: "event.review",
      params: {
        seq: 1,
        timestampUtc: "2026-06-05T00:00:00Z",
        runId,
        event: { runStarted: { runId } }
      }
    });
    send({
      jsonrpc: "2.0",
      method: "event.review",
      params: {
        seq: 2,
        timestampUtc: "2026-06-05T00:00:01Z",
        runId,
        event: { runFinished: { runId, status: "completed" } }
      }
    });
    send({
      jsonrpc: "2.0",
      id: request.id,
      result: {
        runId,
        status: "completed",
        summary: {
          sessions: 1,
          completedSessions: 1,
          modelCalls: 0,
          toolCalls: 0,
          totalTokens: 0
        },
        findings: [],
        snapshots: [{ files: 1, capturedFiles: 1 }]
      }
    });
    setTimeout(() => process.exit(0), 20);
    return;
  }
  send({
    jsonrpc: "2.0",
    id: request.id,
    error: {
      code: -32601,
      message: "unsupported method"
    }
  });
});
`,
  );
  return script;
}
