import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  github,
  local,
  type ReviewInstruction,
  type ReviewOptions,
  type ReviewRole,
  type ReviewSource,
  sourceKey,
} from "@muzen/sdk";

import type { CreateReviewRequest } from "./shared.js";
import { loadDemoEnv } from "./server/env.js";
import { parseGithubPullRequestInput } from "./server/github.js";
import {
  createDemoReviewModel,
  modelPreflightErrorMessage,
  runOpenAIModelPreflight,
} from "./server/openai-model.js";
import { evaluateReviewQuality } from "./server/review-quality.js";
import { DurableReviewStore } from "./server/store.js";
import { executeReview } from "./server/worker.js";

loadDemoEnv();

const port = Number(process.env.PORT ?? 4077);
const runnerPath = process.env.MUZEN_RUNNER_PATH ?? defaultRunnerPath();
const reviewModel = createDemoReviewModel();
const store = new DurableReviewStore();

createServer(async (request, response) => {
  try {
    await route(request, response);
  } catch (error) {
    sendJson(response, 500, {
      error: error instanceof Error ? error.message : String(error),
    });
  }
}).listen(port, () => {
  console.log(`durable review example service listening on http://localhost:${port}`);
  console.log(`review model: ${reviewModel.label}`);
});

function defaultRunnerPath(): string {
  const serverDir = dirname(fileURLToPath(import.meta.url));
  return resolve(serverDir, "../../../target/debug/muzen-runner");
}

async function route(
  request: IncomingMessage,
  response: ServerResponse,
): Promise<void> {
  const url = new URL(request.url ?? "/", `http://${request.headers.host}`);
  const parts = url.pathname.split("/").filter(Boolean);

  if (request.method === "GET" && url.pathname === "/api/health") {
    sendJson(response, 200, { ok: true });
    return;
  }

  if (request.method === "GET" && url.pathname === "/api/model/preflight") {
    const preflight = await runOpenAIModelPreflight();
    sendJson(response, preflight.ok ? 200 : 503, {
      ...preflight,
      ...(preflight.ok ? {} : { error: modelPreflightErrorMessage(preflight) }),
      model: reviewModel.metadata.model,
      provider: reviewModel.metadata.modelProvider,
    });
    return;
  }

  if (request.method === "POST" && url.pathname === "/api/reviews") {
    const preflight = await runOpenAIModelPreflight();
    if (!preflight.ok) {
      sendJson(response, 503, {
        ...preflight,
        error: modelPreflightErrorMessage(preflight),
        model: preflight.model ?? reviewModel.metadata.model,
        provider: preflight.provider ?? reviewModel.metadata.modelProvider,
      });
      return;
    }
    const input = await readJson<CreateReviewRequest>(request);
    const reviewInput = {
      ...input,
      changedFiles: validateChangedFiles(input),
    };
    const source = reviewSource(reviewInput);
    const review = store.create(source, reviewOptions(reviewInput, source));
    void executeReview(store, review.id, { runnerPath });
    sendJson(response, 202, { review });
    return;
  }

  if (parts[0] !== "api" || parts[1] !== "reviews" || !parts[2]) {
    sendJson(response, 404, { error: "not found" });
    return;
  }

  const reviewId = parts[2];
  const child = parts.slice(3).join("/");

  if (!store.has(reviewId)) {
    if (child === "events/stream") {
      response.writeHead(404, { "Content-Type": "text/plain" });
      response.end(`review ${reviewId} was not found`);
      return;
    }
    sendJson(response, 404, { error: `review ${reviewId} was not found` });
    return;
  }

  if (request.method === "GET" && child === "") {
    sendJson(response, 200, { review: store.snapshot(reviewId) });
    return;
  }

  if (request.method === "GET" && child === "events") {
    sendJson(response, 200, {
      events: store.eventsAfter(reviewId, url.searchParams.get("after")),
    });
    return;
  }

  if (request.method === "GET" && child === "quality") {
    const snapshot = store.snapshot(reviewId);
    const changedFiles = url.searchParams.getAll("changedFile");
    sendJson(response, 200, {
      quality: evaluateReviewQuality(
        snapshot.result,
        store.eventsAfter(reviewId),
        {
          changedFiles: changedFiles.length > 0 ? changedFiles : snapshot.changedFiles,
          requiredIssuePhrases: url.searchParams.getAll("requiredIssuePhrase"),
        },
      ),
    });
    return;
  }

  if (request.method === "GET" && child === "events/stream") {
    streamEvents(response, reviewId, url.searchParams.get("after"));
    return;
  }

  if (request.method === "GET" && child === "result") {
    const result = store.snapshot(reviewId).result;
    if (!result) {
      response.writeHead(204).end();
      return;
    }
    sendJson(response, 200, { result });
    return;
  }

  sendJson(response, 404, { error: "not found" });
}

function validateChangedFiles(input: CreateReviewRequest): string[] {
  const changedFiles = input.changedFiles ?? [];
  if (!Array.isArray(changedFiles)) {
    throw new Error("changedFiles must be an array of repo-relative paths");
  }
  if (changedFiles.some((file) => typeof file !== "string")) {
    throw new Error("changedFiles must be an array of repo-relative paths");
  }
  const normalized = changedFiles.map((file) => file.trim());
  if (normalized.some((file) => file.length === 0)) {
    throw new Error("changedFiles must not include blank paths");
  }
  if (input.sourceKind === "local" && normalized.length === 0) {
    throw new Error("changedFiles must include at least one repo-relative path");
  }
  return normalized;
}

function reviewSource(input: CreateReviewRequest): ReviewSource {
  if (input.sourceKind === "github") {
    const pullRequest = parseGithubPullRequestInput(input.githubPullRequest ?? "");
    return github.pullRequest(pullRequest);
  }

  const repo = input.repo?.trim();
  if (!repo) {
    throw new Error("local repo path is empty");
  }
  return local(repo);
}

function reviewOptions(
  input: CreateReviewRequest,
  source: ReviewSource,
): ReviewOptions {
  const target = sourceKey(source);
  const changedFiles = input.changedFiles.map((path) => ({ path }));
  return {
    model: reviewModel.model,
    change: {
      kind: source.type === "github_pull_request" ? "provider_review" : "revision_range",
      changedFiles: changedFiles.length > 0 ? changedFiles : undefined,
      reviewTarget: target,
    },
    scope: input.changedFiles.length > 0 ? { files: input.changedFiles } : undefined,
    limits: {
      maxActiveSessions: validateMaxActiveSessions(input.maxActiveSessions),
    },
    instructions: roleInstructions(input.roles),
    metadata: {
      example: "tanstack-durable-review",
      requestedSource: target,
      requestedSourceKind: input.sourceKind,
      requestedRoles: input.roles.length > 0 ? input.roles : ["generalist"],
      ...reviewModel.metadata,
    },
  };
}

function validateMaxActiveSessions(value: number | undefined): number {
  if (value === undefined) {
    return 4;
  }
  if (!Number.isInteger(value) || value < 1 || value > 8) {
    throw new Error("maxActiveSessions must be an integer from 1 to 8");
  }
  return value;
}

function roleInstructions(roles: ReviewRole[]): ReviewInstruction[] {
  const selected: ReviewRole[] = roles.length > 0 ? roles : ["generalist"];
  return selected.map((role) => ({
    kind: "host_policy",
    text:
      role === "generalist"
        ? "Review this change for concrete correctness, security, API-contract, data-loss, and integration bugs."
        : `Review this change from the ${role} perspective.`,
    trusted: true,
  }));
}

function streamEvents(
  response: ServerResponse,
  reviewId: string,
  after?: string | null,
): void {
  response.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
    "Connection": "keep-alive",
  });

  const send = (event: unknown) => {
    const record = event as { cursor: string };
    response.write(`id: ${record.cursor}\n`);
    response.write(`data: ${JSON.stringify(event)}\n\n`);
  };

  for (const event of store.eventsAfter(reviewId, after)) {
    send(event);
  }

  const unsubscribe = store.subscribe(reviewId, send);
  response.on("close", unsubscribe);
}

async function readJson<T>(request: IncomingMessage): Promise<T> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8")) as T;
}

function sendJson(
  response: ServerResponse,
  status: number,
  body: unknown,
): void {
  response.writeHead(status, {
    "Content-Type": "application/json",
  });
  response.end(JSON.stringify(body));
}
