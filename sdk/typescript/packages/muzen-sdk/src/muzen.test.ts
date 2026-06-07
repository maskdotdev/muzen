import { after, describe, it } from "node:test";
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  createWebhookHttpResponse,
  createWebhookResponse,
  createMuzen,
  createMuzenClient,
  local,
  openai,
  MuzenUnsupportedFeatureError,
  type Muzen,
  type ReviewOptions,
  type ReviewResult,
} from "./index.js";
import { RunnerBackedMuzen } from "./local.js";

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
  it("uses runner context RPCs for local workspace context methods", async () => {
    const calls: Array<{ method: string; params: unknown }> = [];
    const runner = {
      request: async (method: string, params: unknown) => {
        calls.push({ method, params });
        if (method === "context.index") {
          return {
            schemaVersion: "muzen.context_manifest.v1",
            engineVersion: "0.1.0",
            snapshotId: "snap-1",
            ruleCount: 1,
            evidenceCount: 2,
            relationshipCount: 0,
            skippedCount: 0,
            createdAtUtc: "1780620000.000000000Z",
          };
        }
        if (method === "context.pack") {
          return {
            id: "ctxpack-1",
            snapshotId: "snap-1",
            purpose: "security",
            evidence: [],
            relationships: [],
            omittedCandidates: [],
            budget: { maxTokens: 4000, usedTokens: 0 },
            sufficiency: { status: "probably_sufficient", missing: [] },
            compilerVersion: "0.1.0",
            createdAtUtc: "1780620000.000000000Z",
          };
        }
        if (method === "context.query") {
          return {
            kind: "related_tests",
            evidence: [],
            omitted: 0,
          };
        }
        throw new Error(`unexpected method ${method}`);
      },
      onNotification: () => () => {},
      close: async () => {},
    };
    const muzen = new RunnerBackedMuzen(runner as never);
    const workspace = muzen.workspace("local");

    const manifest = await workspace.context.index({
      source: local("/repo", { changedFiles: ["src/auth.ts"] }),
    });
    const pack = await workspace.context.buildPack({
      source: local("/repo", { changedFiles: ["src/auth.ts"] }),
      purpose: "security",
      maxTokens: 4000,
    });
    const query = await workspace.context.query({
      source: local("/repo", { changedFiles: ["src/auth.ts"] }),
      kind: "related_tests",
      arguments: { path: "src/auth.ts" },
    });

    assert.equal(manifest.snapshotId, "snap-1");
    assert.equal(pack.purpose, "security");
    assert.equal(query.kind, "related_tests");
    assert.deepEqual(calls.map((call) => call.method), [
      "context.index",
      "context.index",
      "context.pack",
      "context.index",
      "context.query",
    ]);
  });

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
          model: smokeReviewModel("Cargo.toml"),
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
    "handles GitHub webhooks through the Rust runner core",
    { skip: runnerPath ? false : "MUZEN_RUNNER_PATH is not set" },
    async () => {
      muzen ??= await createMuzen({ runnerPath });
      const body = JSON.stringify({
        action: "opened",
        repository: {
          full_name: "maskdotdev/heimdaal",
        },
        pull_request: {
          number: 123,
        },
      });
      const signature = `sha256=${createHmac("sha256", "secret")
        .update(body)
        .digest("hex")}`;
      const request = new Request("https://app.example/webhooks/github", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-GitHub-Event": "pull_request",
          "X-GitHub-Delivery": "delivery-1",
          "X-Hub-Signature-256": signature,
        },
        body,
      });

      const response = await muzen.webhooks.github.response(request, {
        workspaceId: "acme",
        secret: "secret",
      });

      assert.equal(response.status, 202);
      assert.deepEqual(await response.json(), {
        type: "review_created",
        deliveryId: "delivery-1",
        reviewId: "review-1",
        status: "queued",
      });
    },
  );

  it(
    "runs a 20-agent swarm with custom tools through the generic loop",
    { skip: runnerPath ? false : "MUZEN_RUNNER_PATH is not set" },
    async () => {
      const repo = await mkdtemp(join(tmpdir(), "muzen-swarm-"));
      tempDirs.push(repo);
      await writeFile(
        join(repo, "Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
      );
      muzen ??= await createMuzen({ runnerPath });
      const lookups = new Set<string>();

      const result = await muzen.runSwarm({
        repo,
        files: ["Cargo.toml"],
        model: {
          kind: "callback",
          handler: (request) => {
            const toolResults = request.transcript.filter(
              (item) => isRecord(item) && item.kind === "tool_result",
            );
            if (toolResults.length === 0) {
              return {
                toolCalls: [
                  {
                    toolId: "host.lookup",
                    arguments: { key: request.sessionId },
                  },
                ],
                usage: { inputTokens: 8, outputTokens: 4, totalTokens: 12 },
              };
            }
            return {
              content: JSON.stringify({
                agent: request.sessionId,
                objective: request.objective,
              }),
              usage: { inputTokens: 8, outputTokens: 4, totalTokens: 12 },
            };
          },
        },
        tools: [
          {
            id: "host.lookup",
            description: "Look up a host-side value by key.",
            parameters: {
              type: "object",
              properties: { key: { type: "string" } },
              required: ["key"],
              additionalProperties: false,
            },
            effects: ["read_host"],
            handler: (_context, args) => {
              const key = (args as { key: string }).key;
              lookups.add(key);
              return { data: { value: `host-value-for-${key}` } };
            },
          },
        ],
        agents: Array.from({ length: 20 }, (_, index) => ({
          id: `agent-${index}`,
          objective: `Report objective ${index}.`,
          budget: {
            maxTurns: 3,
            maxToolCalls: 2,
            maxPromptTokens: 32_000,
            maxOutputTokens: 1_024,
          },
        })),
      });

      assert.equal(result.status, "completed");
      assert.equal(result.outputs.length, 20);
      assert.equal(result.usage.agents, 20);
      assert.equal(result.usage.completedAgents, 20);
      assert.equal(lookups.size, 20);
      for (const [index, output] of result.outputs.entries()) {
        assert.equal(output.agentId, `agent-${index}`);
        assert.equal(output.completed, true);
        assert.equal(output.status, "done");
        const parsed = JSON.parse(output.output ?? "{}") as {
          agent: string;
          objective: string;
        };
        assert.equal(parsed.agent, `agent-${index}`);
        assert.equal(parsed.objective, `Report objective ${index}.`);
      }
    },
  );
});

function smokeReviewModel(path: string): ReviewOptions["model"] {
  return {
    kind: "callback",
    handler: (request) => {
      const toolResults = request.transcript.filter(
        (item) => isRecord(item) && item.kind === "tool_result",
      );
      if (toolResults.length === 0) {
        return {
          toolCalls: [
            { toolId: "read_diff", arguments: {} },
            { toolId: "read_file", arguments: { path } },
            { toolId: "search_text", arguments: { query: "fixture" } },
          ],
          usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
        };
      }
      // A unit completes on a structured text result; the clean verdict is
      // accepted because the read_file call above recorded file evidence.
      return {
        content: JSON.stringify({
          summary: "Smoke review completed.",
          fileVerdicts: [
            {
              path,
              verdict: "clean",
              summary: "No issues found in the fixture manifest.",
            },
          ],
          findings: [],
        }),
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      };
    },
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

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

  it("manages workspace profiles through the preview HTTP contract", async () => {
    const requests: Array<{
      method: string;
      path: string;
      body?: unknown;
    }> = [];
    const modelProfile = {
      workspaceId: "acme",
      name: "default",
      version: "1",
      provider: "openai_compatible",
      model: "gpt-5",
      secretRef: "vault://workspaces/acme/models/default",
      baseUrl: "https://models.example.test",
      routing: { region: "us-east" },
      updatedAtUtc: "1780620000.000000000Z",
    };
    const providerProfile = {
      workspaceId: "acme",
      name: "github",
      version: "1",
      provider: "github",
      secretRef: "vault://workspaces/acme/providers/github",
      baseUrl: "https://api.github.com",
      routing: { installation: "123" },
      updatedAtUtc: "1780620000.000000000Z",
    };
    const fetchMock: typeof fetch = async (input, init = {}) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const method = init.method ?? "GET";
      const body =
        typeof init.body === "string" ? JSON.parse(init.body) : undefined;
      requests.push({
        method,
        path: `${url.pathname}${url.search}`,
        body,
      });
      if (
        url.pathname === "/v1/workspaces/acme/models/default" &&
        method === "PUT"
      ) {
        return Response.json({ profile: modelProfile });
      }
      if (
        url.pathname === "/v1/workspaces/acme/models/default" &&
        method === "GET"
      ) {
        return Response.json(modelProfile);
      }
      if (url.pathname === "/v1/workspaces/acme/models") {
        return Response.json({ profiles: [modelProfile] });
      }
      if (
        url.pathname === "/v1/workspaces/acme/providers/github" &&
        method === "PUT"
      ) {
        return Response.json({ profile: providerProfile });
      }
      if (
        url.pathname === "/v1/workspaces/acme/providers/github" &&
        method === "GET"
      ) {
        return Response.json(providerProfile);
      }
      if (url.pathname === "/v1/workspaces/acme/providers") {
        return Response.json({ profiles: [providerProfile] });
      }
      if (
        url.pathname === "/v1/workspaces/acme/reviews" &&
        method === "POST"
      ) {
        return Response.json({
          review: {
            id: "review-workspace-1",
            status: "queued",
            source: body.source,
          },
        });
      }
      if (
        url.pathname === "/v1/workspaces/acme/context/index" &&
        method === "POST"
      ) {
        return Response.json({
          manifest: {
            schemaVersion: "muzen.context_manifest.v1",
            engineVersion: "0.1.0",
            snapshotId: "snap-1",
            ruleCount: 1,
            evidenceCount: 3,
            relationshipCount: 0,
            skippedCount: 0,
            createdAtUtc: "1780620000.000000000Z",
          },
        });
      }
      if (
        url.pathname === "/v1/workspaces/acme/context/packs" &&
        method === "POST"
      ) {
        return Response.json({
          pack: {
            id: "ctxpack-1",
            snapshotId: "snap-1",
            purpose: body.purpose,
            evidence: [],
            relationships: [],
            omittedCandidates: [],
            budget: { maxTokens: body.maxTokens, usedTokens: 0 },
            sufficiency: { status: "probably_sufficient", missing: [] },
            compilerVersion: "0.1.0",
            createdAtUtc: "1780620000.000000000Z",
          },
        });
      }
      if (
        url.pathname === "/v1/workspaces/acme/context/query" &&
        method === "POST"
      ) {
        return Response.json({
          result: {
            kind: body.kind,
            evidence: [],
            omitted: 0,
          },
        });
      }
      return new Response("not found", { status: 404, statusText: "Not Found" });
    };
    const workspace = createMuzenClient({
      baseUrl: "https://muzen.example",
      fetch: fetchMock,
    }).workspace("acme");

    const savedModel = await workspace.models.set("default", {
      provider: "openai_compatible",
      model: "gpt-5",
      secretRef: "vault://workspaces/acme/models/default",
      baseUrl: "https://models.example.test",
      routing: { region: "us-east" },
    });
    const loadedModel = await workspace.models.get("default");
    const modelProfiles = await workspace.models.list();
    const savedProvider = await workspace.providers.set("github", {
      provider: "github",
      secretRef: "vault://workspaces/acme/providers/github",
      baseUrl: "https://api.github.com",
      routing: { installation: "123" },
    });
    const loadedProvider = await workspace.providers.get("github");
    const providerProfiles = await workspace.providers.list();
    const review = await workspace.review("github:maskdotdev/heimdaal#123", {
      model: openai({
        model: "gpt-5",
        credential: { secretRef: "vault://workspaces/acme/models/default" },
        baseUrl: "https://models.example.test",
      }),
    });
    const manifest = await workspace.context.index({
      source: local("/repo", { changedFiles: ["src/auth.ts"] }),
    });
    const pack = await workspace.context.buildPack({
      source: local("/repo", { changedFiles: ["src/auth.ts"] }),
      purpose: "security",
      maxTokens: 4000,
    });
    const query = await workspace.context.query({
      source: local("/repo", { changedFiles: ["src/auth.ts"] }),
      kind: "related_tests",
      arguments: { path: "src/auth.ts" },
      limits: { maxResults: 10, maxTokens: 1000 },
    });

    assert.equal(workspace.id, "acme");
    assert.equal(savedModel.model, "gpt-5");
    assert.equal(loadedModel?.secretRef, "vault://workspaces/acme/models/default");
    assert.equal(modelProfiles.length, 1);
    assert.equal(savedProvider.provider, "github");
    assert.equal(
      loadedProvider?.secretRef,
      "vault://workspaces/acme/providers/github",
    );
    assert.equal(providerProfiles.length, 1);
    assert.equal(review.id, "review-workspace-1");
    assert.equal(manifest.schemaVersion, "muzen.context_manifest.v1");
    assert.equal(pack.purpose, "security");
    assert.equal(query.kind, "related_tests");
    assert.deepEqual(requests.map((request) => request.path), [
      "/v1/workspaces/acme/models/default",
      "/v1/workspaces/acme/models/default",
      "/v1/workspaces/acme/models",
      "/v1/workspaces/acme/providers/github",
      "/v1/workspaces/acme/providers/github",
      "/v1/workspaces/acme/providers",
      "/v1/workspaces/acme/reviews",
      "/v1/workspaces/acme/context/index",
      "/v1/workspaces/acme/context/packs",
      "/v1/workspaces/acme/context/query",
    ]);
    assert.equal(
      (requests.at(-1)?.body as { kind?: string }).kind,
      "related_tests",
    );
  });

  it("forwards webhook requests to the remote HTTP contract", async () => {
    const requests: Array<{
      method: string;
      path: string;
      authorization: string | null;
      githubEvent: string | null;
      body: string;
    }> = [];
    const fetchMock: typeof fetch = async (input, init = {}) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      const headers = new Headers(init.headers);
      const body = await new Response(init.body).text();
      requests.push({
        method: init.method ?? "GET",
        path: `${url.pathname}${url.search}`,
        authorization: headers.get("authorization"),
        githubEvent: headers.get("x-github-event"),
        body,
      });
      return Response.json(
        {
          type: "review_created",
          deliveryId: "delivery-1",
          reviewId: "review-1",
          status: "queued",
        },
        { status: 202 },
      );
    };
    const muzen = createMuzenClient({
      baseUrl: "https://muzen.example",
      token: "test-token",
      fetch: fetchMock,
    });
    const request = new Request("https://app.example/webhooks/github", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-GitHub-Event": "pull_request",
        "X-GitHub-Delivery": "delivery-1",
      },
      body: JSON.stringify({ action: "opened" }),
    });

    const response = await muzen.webhooks.github.response(request, {
      workspaceId: "acme",
    });

    assert.equal(response.status, 202);
    assert.deepEqual(await response.json(), {
      type: "review_created",
      deliveryId: "delivery-1",
      reviewId: "review-1",
      status: "queued",
    });
    assert.deepEqual(requests, [
      {
        method: "POST",
        path: "/v1/workspaces/acme/webhooks/github",
        authorization: "Bearer test-token",
        githubEvent: "pull_request",
        body: '{"action":"opened"}',
      },
    ]);
  });

  it("keeps remote worker execution service-owned", async () => {
    const remote = createMuzenClient({
      baseUrl: "https://muzen.example",
      fetch: async () => Response.json({}),
    });

    await assert.rejects(
      () => remote.workers.runOnce(),
      MuzenUnsupportedFeatureError,
    );
  });
});

describe("webhook response helpers", () => {
  it("creates framework-facing JSON responses for webhook deliveries", async () => {
    const created = {
      type: "review_created" as const,
      deliveryId: "delivery-1",
      reviewId: "review-1",
      status: "queued" as const,
    };
    const deduped = {
      type: "review_deduped" as const,
      deliveryId: "delivery-1",
      reviewId: "review-1",
      status: "queued" as const,
    };
    const ignored = {
      type: "ignored" as const,
      deliveryId: "delivery-2",
      reason: "unsupported event",
    };

    const createdHttp = createWebhookHttpResponse(created, {
      headers: { "X-Muzen-Test": "yes" },
    });
    const dedupedResponse = createWebhookResponse(deduped);
    const ignoredResponse = createWebhookResponse(ignored);

    assert.equal(createdHttp.statusCode, 202);
    assert.equal(createdHttp.headers["content-type"], "application/json");
    assert.equal(createdHttp.headers["x-muzen-test"], "yes");
    assert.deepEqual(JSON.parse(createdHttp.body), created);
    assert.equal(dedupedResponse.status, 200);
    assert.deepEqual(await dedupedResponse.json(), deduped);
    assert.equal(ignoredResponse.status, 202);
    assert.deepEqual(await ignoredResponse.json(), ignored);
  });
});
