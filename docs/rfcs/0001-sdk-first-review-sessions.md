# RFC: SDK-first Review Sessions API

**Status:** Draft
**Owner:** Muzen team
**Target release:** Session API preview
**Related work:** current runner SDK, durable review session direction, Argus durable queue architecture

## Summary

Muzen should expose a beautiful, SDK-first developer experience centered around a live `ReviewSession` object.

The primary getting-started flow should feel like:

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

const review = await muzen.review("github:maskdotdev/heimdaal#123");

review.subscribe((event) => {
  if (event.type === "finding.created") {
    console.log(event.finding.title);
  }
});

const result = await review.wait();

console.log(result.conclusion);
console.log(result.summary);
```

The durable architecture remains the same: reviews are backed by sessions, workers, leases, persisted events, retries, and final structured results. But those details should not dominate the first-contact API.

The public API should feel like an SDK for creating and controlling review sessions, not a thin set of queue commands.

Muzen must also support deployments where the host application owns
infrastructure, but users or workspaces own model and provider configuration.
This is a first-class product requirement, not an advanced escape hatch.

Calling `muzen.review(...)` or `workspace.review(...)` schedules durable review
work. It should not imply that the SDK caller is manually orchestrating the
review swarm inline.

## Decision

Adopt an **SDK-first object model** for Muzen review workflows.

The primary public abstraction becomes:

```ts
const review = await muzen.review(source, options);
```

where `review` is a live `ReviewSession` handle with methods for events, waiting, results, cancellation, and refresh.

Lower-level command APIs may still exist, but they should be secondary, generated, or advanced. The happy path should be organized around:

```ts
createMuzen()
createMuzenClient()
createReviewSession()
ReviewSession
```

not:

```ts
submitReview()
streamReviewEvents()
getReviewResult()
cancelReview()
```

## Context

The current API direction correctly identifies Muzen as a durable review workflow system rather than a runner SDK. It models sessions, events, workers, results, cancellation, provider-backed sources, and concurrency policies.

However, the proposed getting-started DX still exposes too much of the system’s internal decomposition too early. Developers have to understand stores, providers, model profiles, source builders, workers, stream helpers, result polling, and cancellation commands before the product feeling is clear.

The SDKs that feel closer to the desired experience lead with a runtime/session object. OpenCode’s SDK presents a type-safe JS/TS client, supports `createOpencode()` for starting a server and client together, supports client-only connection through `createOpencodeClient()`, and exposes server APIs including sessions and event subscription. ([OpenCode][1]) Pi’s SDK centers on `createAgentSession()`, returns an `AgentSession`, supports default resource discovery, exposes `session.subscribe(...)`, and has a runtime abstraction for replacing or switching active sessions. ([Pi][2])

Muzen should borrow the shape, not the domain. The Muzen domain is not “chat with an agent”; it is “create a durable review session for a code source and observe the result.”

## Goals

1. Make the first Muzen example feel obvious and exciting.
2. Center the API around a live review/session object.
3. Preserve durable review workflow semantics.
4. Support embedded, server, and client-only usage with one mental model.
5. Support both programmatic review requests and webhook-driven review requests.
6. Make event subscription, replay, and final result retrieval first-class.
7. Keep production configuration explicit without making local setup noisy.
8. Let users or workspaces manage BYOK model profiles, provider credentials,
   custom base URLs, and model names at runtime.
9. Make scheduling many reviews natural while workers own concurrency, leases,
   retries, cancellation, and result writing.
10. Leave room for future non-review swarm workloads without making the initial public API generic.

## Non-goals

1. Do not expose runner paths, worker leases, changed file lists, or queue internals in the happy path.
2. Do not make artifacts the primary way to retrieve review output.
3. Do not make workspace configuration required for single-tenant setup.
4. Do not make fluent chains the primary API style.
5. Do not make the public vocabulary center on `run`.

## Design principles

### 1. Product nouns before infrastructure nouns

Developers ask Muzen to create a **review**.

Internally, the review is backed by a durable session. Publicly, examples should lead with:

```ts
const review = await muzen.review(...);
```

not:

```ts
const session = await muzen.submitReview(...);
```

The type may still be named `ReviewSession`, but the variable and docs should often say `review`.

### 2. A handle should be useful

The object returned from `muzen.review(...)` should not be a passive `{ id, status }` record. It should be a live handle:

```ts
review.subscribe(...)
review.events(...)
review.wait(...)
review.result(...)
review.cancel(...)
review.refresh(...)
```

This is the core DX improvement.

### 3. Default discovery first, explicit config second

Getting started should support:

```ts
const muzen = await createMuzen();
```

Muzen should discover config from conventional places:

```txt
muzen.config.ts
.env
process.env
```

Production users can still use explicit config.

### 4. User-owned configuration is first-class

Single-tenant apps may configure providers and models directly in
`createMuzen(...)`.

Multi-tenant and BYOK apps should configure infrastructure once and allow
users or workspaces to manage named provider and model profiles at runtime.
Reviews then reference those profiles by name.

A scheduled review must capture an effective configuration snapshot containing
profile ids, profile versions, non-secret routing metadata, and secret
references. Raw API keys and provider tokens must never be written to events,
logs, or review records.

### 5. Same mental model across local and remote modes

The same high-level API should work whether Muzen is embedded locally, running as a server, or accessed remotely as a client.

```ts
const muzen = await createMuzen();
const muzen = await createMuzen({ server: true });
const muzen = createMuzenClient({ baseUrl });
```

All should expose:

```ts
await muzen.review(...)
await muzen.resumeReview(...)
```

### 6. Scheduling is not orchestration

The SDK caller schedules review work and receives a durable handle. Workers own
execution concurrency, leases, retries, cancellation, and result writing.

This should be natural:

```ts
const reviews = await Promise.all([
  workspace.review("github:org/repo#101"),
  workspace.review("github:org/repo#102"),
  workspace.review("github:org/repo#103"),
]);
```

That `Promise.all` schedules reviews. It does not manually orchestrate swarm
execution.

### 7. Events are core, not debug output

Streaming and replay are part of the product contract. A UI should be able to rebuild itself from durable review events and resume from a cursor.

### 8. Results are first-class

The happy path should end with:

```ts
const result = await review.wait();
```

or:

```ts
const result = await review.result();
```

not with artifact lookup.

---

## Proposed API

### Package shape

Primary package:

```ts
import {
  createMuzen,
  createMuzenClient,
  createReviewSession,
  defineMuzenConfig,
  github,
  gitlab,
  openai,
  anthropic,
  postgres,
  vault,
} from "@muzen/sdk";
```

Potential future package split:

```ts
@muzen/sdk          // friendly SDK layer
@muzen/client       // generated API client
@muzen/server       // server/runtime primitives
@muzen/providers    // optional provider adapters
```

Initial recommendation: start with `@muzen/sdk` as the primary public entry point, even if implementation is internally split.

### Minimal script

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

const review = await muzen.review("github:maskdotdev/heimdaal#123");

const result = await review.wait();

console.log(result.conclusion);
console.log(result.summary);

await muzen.close();
```

### Minimal script with events

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

const review = await muzen.review("github:maskdotdev/heimdaal#123");

const unsubscribe = review.subscribe((event) => {
  switch (event.type) {
    case "session.started":
      console.log("Review started");
      break;

    case "finding.created":
      console.log(event.finding.title);
      break;

    case "session.completed":
      console.log("Review completed");
      break;
  }
});

const result = await review.wait();

unsubscribe();

console.log(result);
```

### Typed source builder

```ts
import { createMuzen, github } from "@muzen/sdk";

const muzen = await createMuzen();

const review = await muzen.review(
  github.pullRequest({
    owner: "maskdotdev",
    repo: "heimdaal",
    number: 123,
  }),
);

const result = await review.wait();
```

### Source string shorthand

Muzen should support source strings for demos, scripts, CLIs, and tests:

```ts
await muzen.review("github:maskdotdev/heimdaal#123");
await muzen.review("gitlab:maskdotdev/heimdaal!123");
```

These should parse into typed source descriptors internally.

Source strings should be documented as convenience syntax, not the only serious API.

### Full review options

```ts
const review = await muzen.review(
  github.pullRequest({
    owner,
    repo,
    number,
  }),
  {
    dedupe: "source-head",
    cancelSuperseded: true,

    model: "deep",

    scope: {
      include: ["packages/muzen/**"],
      exclude: ["**/*.snap"],
    },

    metadata: {
      requestedBy: "webhook",
      installationId,
    },
  },
);
```

### Workspace BYOK setup

For multi-tenant or BYOK deployments, the host configures infrastructure once
and workspaces configure their own model and provider profiles:

```ts
const muzen = await createMuzen({
  store: postgres({
    url: process.env.DATABASE_URL,
  }),

  secrets: vault({
    url: process.env.VAULT_URL,
  }),

  userConfig: {
    models: {
      allow: ["openai", "anthropic", "openai-compatible"],
    },

    providers: {
      allow: ["github", "gitlab"],
    },
  },
});

const workspace = muzen.workspace("acme");

await workspace.models.set("default", {
  provider: "openai-compatible",
  apiKey: userProvidedApiKey,
  baseUrl: userProvidedBaseUrl,
  model: userSelectedModel,
});

await workspace.providers.set("github", {
  provider: "github",
  token: userGithubToken,
  baseUrl: "https://api.github.com",
});

const review = await workspace.review(
  "github:maskdotdev/heimdaal#123",
  {
    model: "default",
  },
);
```

The stored review should reference the effective model and provider profile
versions used to schedule it. It should not store raw secrets.

### Review session handle

```ts
interface ReviewSession {
  readonly id: string;
  readonly status: ReviewStatus;
  readonly source: ReviewSource;

  subscribe(listener: (event: ReviewEvent) => void): () => void;

  events(options?: {
    after?: string | null;
    signal?: AbortSignal;
  }): AsyncIterable<ReviewEvent>;

  eventsResponse(options?: {
    after?: string | null;
    signal?: AbortSignal;
  }): Response;

  wait(options?: {
    timeout?: string | number;
    signal?: AbortSignal;
  }): Promise<ReviewResult>;

  result(): Promise<ReviewResult | undefined>;

  cancel(reason?: string | ReviewCancelOptions): Promise<void>;

  refresh(): Promise<ReviewSessionSnapshot>;
}
```

### Muzen runtime object

```ts
interface Muzen {
  review(
    source: ReviewSourceLike,
    options?: ReviewOptions,
  ): Promise<ReviewSession>;

  resumeReview(id: string): Promise<ReviewSession>;

  workspace(id: string): MuzenWorkspace;

  createReviewSession(
    input: CreateReviewSessionInput,
  ): Promise<ReviewSession>;

  reviews: {
    list(options?: ReviewListOptions): Promise<ReviewSessionSummary[]>;
    get(id: string): Promise<ReviewSession>;
    schedule(inputs: ScheduleReviewInput[]): Promise<ReviewSession[]>;
    cancelMany(input: CancelReviewsInput): Promise<CancelReviewsResult>;
  };

  webhooks: {
    github(
      request: Request,
      options?: WebhookReviewOptions,
    ): Promise<WebhookReviewDelivery>;

    gitlab(
      request: Request,
      options?: WebhookReviewOptions,
    ): Promise<WebhookReviewDelivery>;
  };

  workers: {
    start(options?: WorkerOptions): Promise<void>;
  };

  close(): Promise<void>;
}

interface MuzenWorkspace {
  readonly id: string;

  review(
    source: ReviewSourceLike,
    options?: ReviewOptions,
  ): Promise<ReviewSession>;

  resumeReview(id: string): Promise<ReviewSession>;

  reviews: {
    list(options?: ReviewListOptions): Promise<ReviewSessionSummary[]>;
    get(id: string): Promise<ReviewSession>;
    schedule(inputs: ScheduleReviewInput[]): Promise<ReviewSession[]>;
    cancelMany(input: CancelReviewsInput): Promise<CancelReviewsResult>;
  };

  models: {
    set(name: string, profile: ModelProfileInput): Promise<ModelProfile>;
    get(name: string): Promise<ModelProfile | undefined>;
    list(): Promise<ModelProfile[]>;
  };

  providers: {
    set(name: string, profile: ProviderProfileInput): Promise<ProviderProfile>;
    get(name: string): Promise<ProviderProfile | undefined>;
    list(): Promise<ProviderProfile[]>;
  };
}
```

### Standalone session factory

For scripts, tests, and tiny integrations:

```ts
import { createReviewSession } from "@muzen/sdk";

const { review, muzen } = await createReviewSession({
  source: "github:maskdotdev/heimdaal#123",
});

review.subscribe((event) => {
  console.log(event.type);
});

const result = await review.wait();

await muzen.close();
```

This should be convenience sugar over:

```ts
const muzen = await createMuzen();
const review = await muzen.review(source, options);
```

---

## Configuration

### Zero-config local discovery

This should work when environment variables and/or `muzen.config.ts` are present:

```ts
const muzen = await createMuzen();
```

Discovery order:

1. Explicit `createMuzen(...)` options
2. `muzen.config.ts`
3. environment variables
4. development defaults where safe

Example environment variables:

```txt
DATABASE_URL=
GITHUB_TOKEN=
GITHUB_WEBHOOK_SECRET=
OPENAI_API_KEY=
OPENAI_REVIEW_MODEL=
ANTHROPIC_API_KEY=
ANTHROPIC_REVIEW_MODEL=
```

### Configuration ownership layers

A Muzen deployment has three configuration layers:

1. Host config: database, workers, queues, secret storage, allowed providers,
   and global policy.
2. Workspace/user config: provider tokens, model API keys, base URLs, model
   names, and model profiles.
3. Review config: source, model profile, scope, dedupe policy, priority, and
   metadata.

Single-tenant apps may configure providers and models directly in
`createMuzen(...)` or `muzen.config.ts`. Multi-tenant or BYOK apps should
configure infrastructure once and allow users or workspaces to manage named
provider and model profiles at runtime.

Config changes affect future reviews by default. Running reviews use the
effective config snapshot captured when the review was scheduled.

### Explicit production config

```ts
import {
  defineMuzenConfig,
  github,
  gitlab,
  openai,
  anthropic,
  postgres,
  vault,
} from "@muzen/sdk";

export default defineMuzenConfig({
  store: postgres({
    url: process.env.DATABASE_URL,
  }),

  secrets: vault({
    url: process.env.VAULT_URL,
  }),

  userConfig: {
    models: {
      allow: ["openai", "anthropic", "openai-compatible"],
    },

    providers: {
      allow: ["github", "gitlab"],
    },
  },

  providers: [
    github({
      token: process.env.GITHUB_TOKEN,
      webhookSecret: process.env.GITHUB_WEBHOOK_SECRET,
      baseUrl: process.env.GITHUB_BASE_URL,
    }),

    gitlab({
      token: process.env.GITLAB_TOKEN,
      webhookSecret: process.env.GITLAB_WEBHOOK_SECRET,
      baseUrl: process.env.GITLAB_BASE_URL,
    }),
  ],

  models: {
    default: openai({
      apiKey: process.env.OPENAI_API_KEY,
      baseUrl: process.env.OPENAI_BASE_URL,
      model: process.env.OPENAI_REVIEW_MODEL ?? "gpt-4.1",
    }),

    deep: anthropic({
      apiKey: process.env.ANTHROPIC_API_KEY,
      baseUrl: process.env.ANTHROPIC_BASE_URL,
      model: process.env.ANTHROPIC_REVIEW_MODEL,
    }),
  },

  reviews: {
    model: "default",

    concurrency: {
      sessions: 50,
      agentsPerSession: 8,
    },

    dedupe: "source-head",
    cancelSuperseded: true,
  },
});
```

### Why array providers?

This:

```ts
providers: [
  github(...),
  gitlab(...),
]
```

is easier to scan than:

```ts
providers: {
  github: github(...),
  gitlab: gitlab(...),
}
```

It also leaves room for multiple configured instances of the same provider:

```ts
providers: [
  github({
    id: "github-cloud",
    token,
  }),

  github({
    id: "github-enterprise",
    baseUrl: "https://github.internal/api/v3",
    token,
  }),
]
```

Open question: whether model profiles should also be arrays or remain a named object.

---

## Runtime modes

### Embedded local mode

For scripts, tests, local tools, and early adoption:

```ts
const muzen = await createMuzen();

const review = await muzen.review("github:maskdotdev/heimdaal#123");

const result = await review.wait();

await muzen.close();
```

In development, embedded mode may run workers inline by default.

### Server mode

For apps that want to expose a Muzen server from the same process:

```ts
const muzen = await createMuzen({
  server: true,
});

console.log(muzen.server.url);
```

### Client-only mode

For apps talking to an already-running Muzen service:

```ts
import { createMuzenClient } from "@muzen/sdk";

const muzen = createMuzenClient({
  baseUrl: process.env.MUZEN_URL,
  token: process.env.MUZEN_TOKEN,
});

const review = await muzen.review("github:maskdotdev/heimdaal#123");

for await (const event of review.events()) {
  console.log(event.type);
}
```

### Worker mode

Production workers should be explicit:

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

await muzen.workers.start({
  queues: ["reviews"],
  concurrency: 50,
});
```

In development, `createMuzen()` may default to inline workers unless disabled:

```ts
const muzen = await createMuzen({
  workers: "inline",
});
```

Production recommendation:

```ts
const muzen = await createMuzen({
  workers: false,
});
```

Then run workers separately.

### Concurrent scheduling

Muzen must support scheduling and running many review sessions on the same
server or cluster simultaneously.

Calling:

```ts
const review = await workspace.review(source);
```

means:

```txt
Create or reuse a durable review session and schedule it for execution.
```

The returned `ReviewSession` is a handle to durable state.

Scheduling many reviews should be natural:

```ts
const reviews = await Promise.all([
  workspace.review("github:org/repo#101"),
  workspace.review("github:org/repo#102"),
  workspace.review("github:org/repo#103"),
]);
```

This `Promise.all` only schedules reviews. It does not manually orchestrate
swarm execution. Workers own execution, concurrency, leases, retries,
cancellation, and result writing.

A batch helper may also be provided:

```ts
const reviews = await workspace.reviews.schedule([
  { source: "github:org/repo#101" },
  { source: "github:org/repo#102" },
  { source: "github:org/repo#103" },
]);
```

Execution concurrency should be controlled by host and worker policy:

```ts
const muzen = await createMuzen({
  scheduling: {
    concurrency: {
      reviews: 100,
      agentsPerReview: 8,
      perWorkspace: 10,
      perUser: 3,
    },

    fairness: {
      strategy: "round-robin-by-workspace",
    },
  },
});
```

Workers should be able to claim multiple sessions concurrently using the
durable queue mechanism. Multiple API servers and multiple worker processes may
share the same store.

Explicit dedupe policy controls same-source behavior:

```ts
await workspace.review(source, {
  dedupe: "source-head",
  cancelSuperseded: true,
});
```

Other modes should include:

```ts
dedupe: "none";
dedupe: "source";
dedupe: { key: "application-defined-idempotency-key" };
```

This allows Muzen to support both "only one active review per PR head" and
"run multiple review passes over the same PR with different models/scopes."

---

## Webhook API

### Simple GitHub route

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

export async function POST(request: Request) {
  const delivery = await muzen.webhooks.github(request);

  if (delivery.type === "ignored") {
    return Response.json({
      ignored: true,
      reason: delivery.reason,
    });
  }

  return Response.json({
    reviewId: delivery.review.id,
    status: delivery.review.status,
  });
}
```

### Response helper

For the shortest possible framework integration:

```ts
export async function POST(request: Request) {
  return muzen.webhooks.github.response(request);
}
```

This helper should verify signatures, parse payloads, create or dedupe review sessions, and return an appropriate JSON response.

### Delivery result type

```ts
type WebhookReviewDelivery =
  | {
      type: "review_created";
      review: ReviewSession;
      deliveryId: string;
    }
  | {
      type: "review_deduped";
      review: ReviewSession;
      deliveryId: string;
    }
  | {
      type: "ignored";
      reason: string;
      deliveryId?: string;
    };
```

### Webhook options

```ts
await muzen.webhooks.github(request, {
  dedupe: "source-head",
  cancelSuperseded: true,

  map(payload) {
    return {
      model: payload.repository.private ? "deep" : "default",
      metadata: {
        installationId: payload.installation?.id,
      },
    };
  },
});
```

---

## Event API

### Callback subscription

```ts
const unsubscribe = review.subscribe((event) => {
  console.log(event.type);
});
```

This should replay from the latest known state or subscribe live depending on implementation defaults.

Open question: should `subscribe()` default to live-only or replay-and-follow?

Recommendation:

```ts
review.subscribe(listener, {
  replay: true,
});
```

with `replay: true` as the default for durable review sessions.

### Async iterable

```ts
for await (const event of review.events({
  after: request.headers.get("Last-Event-ID"),
})) {
  render(event);
}
```

### SSE response

```ts
export async function GET(
  request: Request,
  { params }: { params: { reviewId: string } },
) {
  const review = await muzen.resumeReview(params.reviewId);

  return review.eventsResponse({
    after: request.headers.get("Last-Event-ID"),
  });
}
```

### Event names

Durable events should include:

```ts
type ReviewEvent =
  | { type: "session.created"; id: string; createdAt: string }
  | { type: "session.queued"; id: string }
  | { type: "session.claimed"; id: string; attempt: number }
  | { type: "session.started"; id: string }
  | { type: "source.resolved"; source: ResolvedReviewSource }
  | { type: "scope.inferred"; scope: ReviewScope }
  | { type: "scope.overridden"; scope: ReviewScope }
  | { type: "repo.materialized"; repo: MaterializedRepo }
  | { type: "plan.created"; plan: ReviewPlan }
  | { type: "agent.started"; agent: ReviewAgent }
  | { type: "agent.completed"; agent: ReviewAgent }
  | { type: "tool.started"; tool: ReviewToolCall }
  | { type: "tool.completed"; tool: ReviewToolCall }
  | { type: "finding.created"; finding: ReviewFinding }
  | { type: "finding.updated"; finding: ReviewFinding }
  | { type: "review.result_created"; result: ReviewResult }
  | { type: "session.completed"; id: string }
  | { type: "session.failed"; id: string; error: ReviewError }
  | { type: "session.cancelled"; id: string; reason?: string };
```

High-volume logs, token streams, stdout/stderr tails, and heartbeats should be controlled by event policy:

```ts
const review = await muzen.review(source, {
  events: {
    persist: ["durable"],
    ephemeral: ["tool.logs", "agent.tokens"],
  },
});
```

---

## Result API

### Wait for final result

```ts
const result = await review.wait({
  timeout: "10m",
});
```

### Fetch existing result

```ts
const result = await review.result();

if (!result) {
  console.log("Review is still running");
}
```

### Result shape

```ts
type ReviewResult = {
  reviewId: string;
  sessionId: string;

  status: "completed" | "failed" | "cancelled";

  conclusion:
    | "approved"
    | "commented"
    | "changes_requested";

  summary: string;

  findings: ReviewFinding[];

  coverage: {
    filesConsidered: number;
    filesReviewed: number;
    filesSkipped: number;
  };

  metadata?: Record<string, unknown>;
};
```

### Finding shape

```ts
type ReviewFinding = {
  id: string;

  severity:
    | "info"
    | "warning"
    | "error";

  category:
    | "bug"
    | "security"
    | "performance"
    | "maintainability"
    | "style"
    | "test"
    | "docs"
    | "other";

  title: string;
  message: string;

  location?: {
    path: string;
    startLine?: number;
    endLine?: number;
    startColumn?: number;
    endColumn?: number;
  };

  suggestedFix?: {
    description?: string;
    patch?: string;
  };

  confidence?: number;
};
```

---

## Cancellation API

### Cancel one review

```ts
await review.cancel("superseded_by_new_push");
```

or:

```ts
await review.cancel({
  reason: "superseded_by_new_push",
});
```

### Cancel by source

```ts
await muzen.reviews.cancelMany({
  source: github.pullRequest({
    owner,
    repo,
    number,
  }),

  reason: "new_commit_pushed",
});
```

The public API should not expose worker lease or attempt internals.

---

## User-owned configuration and BYOK

Single-tenant setup should stay simple. A small deployment can configure
providers and models directly in `createMuzen(...)`.

Multi-tenant or BYOK deployments should configure infrastructure once, then let
users or workspaces manage named provider and model profiles at runtime.

The host controls:

- database and durable store,
- worker queues and global scheduling policy,
- secret storage,
- allowed provider families,
- allowed model provider families,
- global safety policy.

The workspace or user controls:

- provider credentials,
- model API keys,
- custom model API base URLs,
- model names,
- named model profiles,
- named provider profiles.

Example workspace profile setup:

```ts
const workspace = muzen.workspace(workspaceId);

await workspace.models.set("default", {
  provider: "openai-compatible",
  apiKey: userProvidedApiKey,
  baseUrl: userProvidedBaseUrl,
  model: userSelectedModel,
});

await workspace.providers.set("github", {
  provider: "github",
  token: userGithubToken,
  baseUrl: "https://api.github.com",
});
```

Reviews can reference those profiles by name:

```ts
const review = await workspace.review(
  "github:maskdotdev/heimdaal#123",
  {
    model: "default",
  },
);
```

A scheduled review must capture an effective config snapshot containing:

- model profile id and version,
- provider profile id and version,
- non-secret routing metadata,
- secret references,
- review source, scope, dedupe policy, priority, and metadata.

Raw API keys and provider tokens must never be written to events, logs, or
review records.

Config changes affect future reviews by default. Running reviews use the
config snapshot captured when the review was scheduled.

---

## Lower-level client API

The friendly SDK should be built on top of a lower-level generated or generated-like client.

Example:

```ts
await client.review.create({ body });
await client.review.get({ path: { id } });
await client.review.result({ path: { id } });
await client.review.cancel({ path: { id }, body });
await client.event.subscribe({ query: { reviewId: id } });
```

This lower-level layer is useful for:

1. generated API clients,
2. remote service access,
3. tests,
4. debugging,
5. non-JS clients,
6. backwards compatibility.

But the README should lead with the friendly SDK layer.

---

## Compatibility with current SDK

Current concepts should map as follows:

| Current concept                    | New concept                                              |
| ---------------------------------- | -------------------------------------------------------- |
| `startReview(...)`                 | `muzen.review(...)` plus `review.wait()`                 |
| `runId`                            | generated `review.id`; optional idempotency key          |
| `changedFiles`                     | inferred provider scope; optional `scope.files` override |
| `runnerPath`                       | advanced runtime config                                  |
| manual `Promise.all` orchestration | worker-managed concurrency                               |
| artifacts as result path           | `review.result()` / `review.wait()`                      |
| runner events                      | durable review events                                    |
| low-level run cancellation         | `review.cancel()`                                        |

Compatibility layer:

```ts
async function startReview(input: StartReviewInput) {
  const review = await muzen.review(convertStartReviewInput(input));
  return review.wait();
}
```

Deprecation path:

1. Introduce new SDK beside current API.
2. Rewrite docs and examples around `createMuzen()` and `muzen.review(...)`.
3. Implement `startReview` as compatibility sugar over the session API.
4. Deprecate `changedFiles` in favor of `scope`.
5. Deprecate `runId` in favor of `idempotencyKey` or generated `review.id`.
6. Move `runnerPath` into advanced runtime configuration.
7. Remove old API only after at least one stable migration window.

---

## Internal architecture requirements

The SDK-first API must still preserve the durable architecture:

1. A review is stored as a durable session row.
2. Scheduling may create a new session or reuse an existing session according
   to explicit dedupe policy.
3. The database row is the queue item.
4. A review captures an effective config snapshot with profile ids, profile
   versions, non-secret routing metadata, and secret references.
5. Raw API keys and provider tokens are never stored in review rows, events,
   logs, or final results.
6. Workers claim sessions with `FOR UPDATE SKIP LOCKED`.
7. Claimed sessions use leases, attempts, retry policy, and backoff.
8. Worker pools can run multiple sessions concurrently subject to global,
   workspace, user, model, and provider limits.
9. Events are persisted before being streamed.
10. Streams can replay from cursor and then follow live updates.
11. Notifications are wake-up hints, not the source of truth.
12. Polling remains as a fallback when notifications are missed.
13. The final structured result is written separately from raw artifacts.
14. Cancellation is durable and honored by workers.

These details should be invisible in the getting-started API.

---

## Proposed README flow

### 1. Install

```sh
npm install @muzen/sdk
```

### 2. Configure

Single-tenant:

```sh
DATABASE_URL=postgres://...
GITHUB_TOKEN=...
GITHUB_WEBHOOK_SECRET=...
OPENAI_API_KEY=...
```

BYOK or multi-tenant:

```ts
const workspace = muzen.workspace("acme");

await workspace.models.set("default", {
  provider: "openai-compatible",
  apiKey,
  baseUrl,
  model,
});
```

### 3. Run a review

Single-tenant:

```ts
const review = await muzen.review("github:maskdotdev/heimdaal#123");
```

Workspace-owned config:

```ts
const review = await workspace.review("github:maskdotdev/heimdaal#123");
```

Then wait for the result:

```ts
console.log(await review.wait());
```

### 4. Subscribe to progress

```ts
review.subscribe((event) => {
  console.log(event.type);
});
```

### 5. Handle GitHub webhooks

```ts
export async function POST(request: Request) {
  return muzen.webhooks.github.response(request);
}
```

### 6. Run workers in production

```ts
const muzen = await createMuzen();

await muzen.workers.start();
```

### 7. Connect to a remote Muzen service

```ts
import { createMuzenClient } from "@muzen/sdk";

const muzen = createMuzenClient({
  baseUrl: process.env.MUZEN_URL,
});

const review = await muzen.review("github:maskdotdev/heimdaal#123");

console.log(await review.wait());
```

### 8. Schedule many reviews

```ts
const reviews = await workspace.reviews.schedule(
  pullRequests.map((pr) => ({
    source: github.pullRequest(pr),
    model: "default",
    dedupe: "source-head",
    cancelSuperseded: true,
  })),
);
```

---

## Naming recommendations

### Preferred names

```ts
createMuzen()
createMuzenClient()
createReviewSession()
defineMuzenConfig()

muzen.review(...)
muzen.resumeReview(...)
muzen.workspace(...)
muzen.webhooks.github(...)
muzen.workers.start(...)

workspace.review(...)
workspace.reviews.schedule(...)
workspace.models.set(...)
workspace.providers.set(...)

review.subscribe(...)
review.events(...)
review.eventsResponse(...)
review.wait(...)
review.result(...)
review.cancel(...)
review.refresh(...)
```

### Avoid as primary DX

```ts
submitReview(...)
receiveReviewWebhook(...)
streamReviewEvents(...)
getReviewResult(...)
cancelReview(...)
startWorkers(...)
```

These names are clear but too command-oriented. They can exist as lower-level or compatibility APIs.

### Use sparingly

```ts
session
attempt
lease
run
artifact
runner
```

These are valid internal or advanced terms, but they should not dominate the happy path.

---

## Open questions

### 1. Should `muzen.review(...)` create immediately or return a builder?

Recommendation: create immediately.

Prefer:

```ts
const review = await muzen.review(source);
```

Avoid:

```ts
const review = await muzen.review(source).start();
```

The latter is more verbose and makes the review feel less like a durable product action.

### 2. Should `review.subscribe(...)` replay old events by default?

Recommendation: yes.

A durable review session should be rebuildable. Default subscription should probably replay from the beginning or from the last known cursor.

Potential API:

```ts
review.subscribe(listener, {
  after: cursor,
  replay: true,
});
```

### 3. Should source strings be official?

Recommendation: yes, but as convenience syntax.

They are excellent for examples and CLI usage:

```ts
"github:owner/repo#123"
```

But production docs should also show typed builders.

### 4. Should provider builders and provider config share names?

Recommendation: yes only if carefully namespaced.

Potential issue:

```ts
github(...)
github.pullRequest(...)
```

This may be cute but ambiguous.

Safer:

```ts
github.provider(...)
github.pullRequest(...)
```

or:

```ts
providers.github(...)
sources.github.pullRequest(...)
```

However, for beauty, this is attractive:

```ts
import { github } from "@muzen/sdk";

github({
  token,
});

github.pullRequest({
  owner,
  repo,
  number,
});
```

Prototype both.

### 5. Should the main package be `@muzen/sdk` or `@muzen/server`?

Recommendation: `@muzen/sdk` for the beautiful public API.

Use `@muzen/server` only if the package truly implies server-only runtime. The linked SDK inspirations both frame the developer experience as an SDK, not just server infrastructure.

### 6. Should generic swarm sessions be public now?

Recommendation: no.

Internally, build generic durable session machinery. Publicly, lead with reviews. A generic API can appear later if multiple Muzen workloads emerge.

### 7. Should embedded mode start workers inline?

Recommendation: in development, yes; in production, no by default.

Potential behavior:

```ts
createMuzen(); // dev: inline workers, prod: no inline workers
```

But environment-dependent behavior can be surprising.

Safer:

```ts
createMuzen({
  workers: "inline",
});
```

and quickstart can use:

```ts
createMuzen.dev();
```

Open for prototype.

---

## Risks

### Risk: magic config makes production behavior unclear

Mitigation: provide diagnostics.

```ts
const muzen = await createMuzen();

console.log(muzen.diagnostics.config);
```

or:

```ts
await muzen.doctor();
```

### Risk: source strings become a second DSL

Mitigation: document them as shorthand and always show the equivalent typed builder.

### Risk: object handle hides network boundaries

In client-only mode, `review.wait()` and `review.cancel()` are remote calls. This is acceptable, but docs should be clear that handles are lightweight proxies over durable server state.

### Risk: callback subscriptions are harder to compose than async iterables

Mitigation: support both.

```ts
review.subscribe(...)
review.events(...)
```

### Risk: session object becomes too large

Mitigation: keep the core handle small. Put advanced operations under explicit namespaces later if needed.

```ts
review.debug.artifacts()
review.comments.publish()
review.trace()
```

Do not add these to the initial happy path.

---

## Acceptance criteria

The new SDK direction is successful if the README can start with this and feel complete:

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

const review = await muzen.review("github:maskdotdev/heimdaal#123");

review.subscribe((event) => {
  console.log(event.type);
});

const result = await review.wait();

console.log(result.summary);
```

A production app should be able to add only these pieces:

```ts
export async function POST(request: Request) {
  return muzen.webhooks.github.response(request);
}
```

```ts
await muzen.workers.start();
```

A UI should be able to stream durable events from:

```ts
const review = await muzen.resumeReview(reviewId);

return review.eventsResponse({
  after: request.headers.get("Last-Event-ID"),
});
```

And a migration from the current SDK should not require users to understand worker leases, runner paths, manual run IDs, changed file lists, or artifact plumbing.

Additional acceptance criteria:

1. A user or workspace can configure BYOK model profiles at runtime.
2. A user or workspace can configure custom model API base URLs and model names.
3. A user or workspace can configure provider credentials, including
   self-hosted provider base URLs.
4. A review records the effective config profile and version used without
   storing raw secrets.
5. Changing a model or provider profile affects future reviews, not
   already-running reviews.
6. A single Muzen server can schedule many review sessions concurrently.
7. A worker pool can run multiple review sessions concurrently subject to
   global, workspace, user, model, and provider limits.
8. Multiple reviews for the same source are controlled by explicit dedupe
   policy.
9. The SDK supports both individual scheduling and batch scheduling.

---

## Recommended next steps

1. Prototype the `ReviewSession` handle in front of the existing review runner.
2. Implement `muzen.review(source, options)` as a compatibility layer over the current internals.
3. Add `review.subscribe`, `review.events`, `review.wait`, `review.result`, and `review.cancel`.
4. Add source string parsing for GitHub PRs.
5. Add typed `github.pullRequest(...)` source builder.
6. Add `createMuzen()` config discovery.
7. Add `createMuzenClient({ baseUrl })`.
8. Add workspace model/provider profile management for BYOK deployments.
9. Add effective config snapshot capture for scheduled reviews.
10. Add `workspace.reviews.schedule(...)` and dedupe policy behavior.
11. Rewrite the README around the SDK-first flow.
12. Only then revisit lower-level command names and package boundaries.

The core bet: **Muzen should feel like creating and controlling a live review object, while internally remaining a durable distributed workflow system.**

[1]: https://opencode.ai/docs/sdk/ "SDK | OpenCode"
[2]: https://pi.dev/docs/latest/sdk "SDK · Docs · Pi"
