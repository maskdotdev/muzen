import { after, describe, it } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  createMuzen,
  createMuzenClient,
  local,
  MuzenUnsupportedFeatureError,
  type Muzen,
  type ReviewResult,
} from "./index.js";

const runnerPath = process.env.MUZEN_RUNNER_PATH;
const tempDirs: string[] = [];
let muzen: Muzen | undefined;

after(async () => {
  await muzen?.close();
  await Promise.all(
    tempDirs.map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

describe("runner-backed Muzen preview", () => {
  it(
    "runs a local review, replays events, and waits for a result",
    { skip: runnerPath ? false : "MUZEN_RUNNER_PATH is not set" },
    async () => {
      const repo = await mkdtemp(join(tmpdir(), "muzen-sdk-"));
      tempDirs.push(repo);
      await writeFile(
        join(repo, "Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
      );
      muzen = await createMuzen({ runnerPath });

      const review = await muzen.review(
        local(repo, { changedFiles: ["Cargo.toml"] }),
        {
          sessions: [
            {
              id: "security",
              role: "security",
              objective: "Find security regressions",
            },
          ],
        },
      );
      const result = await review.wait();
      const artifacts = await review.exportArtifacts();
      const artifact = await review.readArtifact(artifacts.artifacts[0].artifactId);
      const replayed: string[] = [];
      review.subscribe((event) => replayed.push(event.type));

      assert.equal(review.status, "completed");
      assert.equal(result.status, "completed");
      assert.match(result.summary, /Review completed/);
      assert.ok(artifacts.artifactCount > 0);
      assert.equal(artifact.artifactId, artifacts.artifacts[0].artifactId);
      assert.ok(artifact.content.length > 0);
      assert.ok(replayed.includes("session.completed"));
      assert.equal((await review.refresh()).id, review.id);
    },
  );

  it(
    "keeps provider-backed sources explicit until materialization exists",
    { skip: runnerPath ? false : "MUZEN_RUNNER_PATH is not set" },
    async () => {
      muzen ??= await createMuzen({ runnerPath });

      await assert.rejects(
        () => muzen!.review("github:maskdotdev/heimdaal#123"),
        MuzenUnsupportedFeatureError,
      );
    },
  );
});

describe("remote Muzen client", () => {
  it("uses the preview HTTP contract for reviews, events, results, and artifacts", async () => {
    const requests: Array<{
      method: string;
      path: string;
      authorization: string | null;
      body?: unknown;
    }> = [];
    const result: ReviewResult = {
      reviewId: "review-remote-1",
      sessionId: "review-remote-1",
      status: "completed",
      conclusion: "approved",
      summary: "Remote review completed.",
      findings: [],
      coverage: {
        filesConsidered: 1,
        filesReviewed: 1,
        filesSkipped: 0,
      },
    };
    const fetchMock: typeof fetch = async (input, init = {}) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const method = init.method ?? "GET";
      const headers = new Headers(init.headers);
      const body =
        typeof init.body === "string" ? JSON.parse(init.body) : undefined;
      requests.push({
        method,
        path: `${url.pathname}${url.search}`,
        authorization: headers.get("authorization"),
        body,
      });
      if (url.pathname === "/v1/reviews" && method === "POST") {
        return Response.json({
          review: {
            id: "review-remote-1",
            status: "queued",
            source: body.source,
          },
        });
      }
      if (url.pathname === "/v1/reviews/review-remote-1") {
        return Response.json({
          id: "review-remote-1",
          status: "completed",
          source: {
            type: "github_pull_request",
            owner: "maskdotdev",
            repo: "heimdaal",
            number: 123,
          },
          result,
        });
      }
      if (url.pathname === "/v1/reviews/review-remote-1/events") {
        return Response.json({
          events: [
            {
              cursor: "1",
              type: "session.queued",
              reviewId: "review-remote-1",
              timestampUtc: "1780620000.000000000Z",
              payload: {},
            },
          ],
        });
      }
      if (url.pathname === "/v1/reviews/review-remote-1/result") {
        return Response.json({ result });
      }
      if (
        url.pathname ===
        "/v1/reviews/review-remote-1/artifacts/artifact-1"
      ) {
        return Response.json({
          artifact: {
            artifactId: "artifact-1",
            bytes: 4,
            contentHash: "hash",
            content: "data",
          },
        });
      }
      if (
        url.pathname ===
          "/v1/reviews/review-remote-1/artifacts/export" &&
        method === "POST"
      ) {
        return Response.json({
          view: "redacted",
          artifactCount: 1,
          totalBytes: 4,
          artifacts: [
            {
              artifactId: "artifact-1",
              bytes: 4,
              contentHash: "hash",
              content: "data",
            },
          ],
        });
      }
      if (
        url.pathname === "/v1/reviews/review-remote-1/cancel" &&
        method === "POST"
      ) {
        return new Response(null, { status: 204 });
      }
      return new Response("not found", { status: 404, statusText: "Not Found" });
    };
    const remote = createMuzenClient({
      baseUrl: "https://muzen.example",
      token: "test-token",
      fetch: fetchMock,
    });

    const review = await remote.review("github:maskdotdev/heimdaal#123", {
      dedupe: "source",
    });
    const events = [];
    for await (const event of review.events()) {
      events.push(event);
    }
    const finalResult = await review.wait({ timeout: "1s" });
    const artifact = await review.readArtifact("artifact-1");
    const exported = await review.exportArtifacts();
    assert.equal(review.status, "completed");
    await review.cancel("superseded");
    const resumed = await remote.resumeReview("review-remote-1");

    assert.equal(review.id, "review-remote-1");
    assert.equal(review.status, "cancelled");
    assert.equal(events[0]?.type, "session.queued");
    assert.equal(finalResult.conclusion, "approved");
    assert.equal(artifact.content, "data");
    assert.equal(exported.artifactCount, 1);
    assert.equal(resumed.status, "completed");
    assert.equal(requests[0]?.authorization, "Bearer test-token");
    assert.deepEqual(requests.map((request) => request.path), [
      "/v1/reviews",
      "/v1/reviews/review-remote-1/events",
      "/v1/reviews/review-remote-1/result",
      "/v1/reviews/review-remote-1/artifacts/artifact-1?view=redacted",
      "/v1/reviews/review-remote-1/artifacts/export",
      "/v1/reviews/review-remote-1/cancel",
      "/v1/reviews/review-remote-1",
    ]);
  });
});
