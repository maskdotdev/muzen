import { createServer, type IncomingMessage, type ServerResponse } from "node:http";

import {
  github,
  local,
  type ReviewAgentSession,
  type ReviewOptions,
  type ReviewRole,
  type ReviewSource,
  sourceKey,
} from "@muzen/sdk";

import type { CreateReviewRequest } from "./shared.js";
import { parseGithubPullRequestInput } from "./server/github.js";
import { DurableReviewStore } from "./server/store.js";
import { executeReview } from "./server/worker.js";

const port = Number(process.env.PORT ?? 4077);
const runnerPath = process.env.MUZEN_RUNNER_PATH;
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
});

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

  if (request.method === "POST" && url.pathname === "/api/reviews") {
    const input = await readJson<CreateReviewRequest>(request);
    const source = reviewSource(input);
    const review = store.create(source, reviewOptions(input, source));
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

function reviewSource(input: CreateReviewRequest): ReviewSource {
  if (input.sourceKind === "github") {
    const pullRequest = parseGithubPullRequestInput(input.githubPullRequest ?? "");
    return github.pullRequest(pullRequest);
  }

  const repo = input.repo?.trim();
  if (!repo) {
    throw new Error("local repo path is empty");
  }
  return local(repo, { changedFiles: input.changedFiles });
}

function reviewOptions(
  input: CreateReviewRequest,
  source: ReviewSource,
): ReviewOptions {
  const target = sourceKey(source);
  return {
    change: {
      kind: source.type === "github_pull_request" ? "provider_review" : "revision_range",
      changedFiles: input.changedFiles.map((path) => ({ path })),
      reviewTarget: target,
    },
    scope: {
      files: input.changedFiles,
    },
    sessions: sessions(input.roles),
    metadata: {
      example: "tanstack-durable-review",
      requestedSource: target,
      requestedSourceKind: input.sourceKind,
    },
  };
}

function sessions(roles: ReviewRole[]): ReviewAgentSession[] {
  const selected: ReviewRole[] = roles.length > 0 ? roles : ["generalist"];
  return selected.map((role) => ({
    id: role,
    role,
    objective: `Review this change from the ${role} perspective.`,
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
