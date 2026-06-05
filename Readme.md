# Muzen

Muzen is a Rust-first review automation runtime. The durable core owns review
sessions, workspace configuration, scheduling, workers, webhooks, and event
streams in Rust; TypeScript and Python SDKs provide the high-level developer
experience on top.

## Production SDK Flow

This is the public API shape RFC 0001 is driving toward.

### 1. Install

```sh
npm install @muzen/sdk
```

### 2. Configure

```sh
DATABASE_URL=postgres://...
GITHUB_TOKEN=...
GITHUB_WEBHOOK_SECRET=...
OPENAI_API_KEY=...
```

### 3. Run A Review

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

const review = await muzen.review("github:maskdotdev/heimdaal#123");

console.log(await review.wait());
```

### 4. Subscribe To Progress

```ts
review.subscribe((event) => {
  console.log(event.type);
});
```

### 5. Handle GitHub Webhooks

```ts
export async function POST(request: Request) {
  return muzen.webhooks.github.response(request);
}
```

### 6. Run Workers In Production

```ts
const muzen = await createMuzen();

await muzen.workers.start();
```

### 7. Connect To A Remote Muzen Service

```ts
import { createMuzenClient } from "@muzen/sdk";

const muzen = createMuzenClient({
  baseUrl: process.env.MUZEN_URL,
});

const review = await muzen.review("github:maskdotdev/heimdaal#123");

console.log(await review.wait());
```

## Current Implementation Status

The flow above is the target developer experience. The implementation is being
built in committed RFC slices, and the current preview intentionally avoids
documenting unimplemented APIs as if they are ready.

Implemented now:

- Rust core review-session contracts, local execution, durable records,
  cancellation, worker claims, leases, retries, concurrency limits, and queued
  worker execution.
- Rust Postgres-backed review-session and workspace-profile stores, including
  transactional worker claims with `FOR UPDATE SKIP LOCKED`.
- Rust workspace model/provider profiles and effective config snapshots with
  secret references instead of raw credentials.
- Rust GitHub/GitLab webhook verification, source mapping, queued scheduling,
  delivery JSON, and framework-agnostic HTTP/SSE response helpers.
- Rust framework-neutral remote HTTP router for review creation, events,
  results, artifacts, workspace profiles, and provider webhooks.
- Rust Axum HTTP service adapter and `muzen-service` binary around the core
  router.
- TypeScript SDK local preview over `muzen-runner`.
- TypeScript local webhook request facade:
  `createMuzen(...).webhooks.github.response(request)`.
- TypeScript local worker facade:
  `createMuzen(...).workers.runOnce()` and `createMuzen(...).workers.start()`,
  backed by the Rust worker protocol.
- TypeScript remote client preview with review, workspace profile, event,
  result, cancellation, and artifact APIs.
- TypeScript framework-facing webhook delivery response helpers.
- TypeScript remote webhook request facade:
  `createMuzenClient(...).webhooks.github.response(request)`.
- Python SDK local preview over `muzen-runner`.
- Python remote client preview with review and workspace profile APIs.
- Python framework-neutral webhook delivery response helpers.

Still in progress:

- GitHub/GitLab provider materialization for local `createMuzen().review(...)`.

## Local Preview

Build the runner:

```sh
cargo build --bin muzen-runner
export MUZEN_RUNNER_PATH="$PWD/target/debug/muzen-runner"
```

Run a local repository review through the TypeScript SDK:

```ts
import { createMuzen, local } from "@muzen/sdk";

const muzen = await createMuzen({
  runnerPath: process.env.MUZEN_RUNNER_PATH,
});

try {
  const review = await muzen.review(
    local(".", {
      changedFiles: ["Cargo.toml"],
    }),
  );

  review.subscribe((event) => {
    console.log(event.type);
  });

  const result = await review.wait();
  console.log(result.conclusion);
  console.log(result.summary);
} finally {
  await muzen.close();
}
```

Runnable examples:

- `examples/typescript/basic-review`
- `examples/typescript/events`
- `examples/python/basic_review.py`
- `examples/python/notebook-review/notebook_review.ipynb`

## Rust Service

Run the RFC 0001 HTTP service:

```sh
DATABASE_URL=postgres://...
GITHUB_WEBHOOK_SECRET=...
GITLAB_WEBHOOK_TOKEN=...
cargo run --bin muzen-service -- --bind 127.0.0.1:7341
```

When `DATABASE_URL` is set, `muzen-service` uses Postgres-backed durable review
session and workspace profile stores. Without it, the service uses in-memory
stores for local preview.

## Remote Client Preview

```ts
import { createMuzenClient } from "@muzen/sdk";

const muzen = createMuzenClient({
  baseUrl: "https://muzen.example",
  token: process.env.MUZEN_TOKEN,
});

const workspace = muzen.workspace("acme");

await workspace.models.set("default", {
  provider: "openai_compatible",
  model: "gpt-5",
  secretRef: "vault://workspaces/acme/models/default",
});

await workspace.providers.set("github", {
  provider: "github",
  secretRef: "vault://workspaces/acme/providers/github",
});

const review = await workspace.review("github:maskdotdev/heimdaal#123", {
  model: "default",
});
```

## Python Preview

```py
import asyncio
import os

from muzen import Client, local


async def main() -> None:
    client = await Client.create(
        runner_path=os.environ.get("MUZEN_RUNNER_PATH"),
    )
    try:
        review = await client.review(
            local(".", changed_files=["Cargo.toml"]),
        )

        for event in [event async for event in review.events()]:
            print(event.type)

        result = await review.wait()
        print(result.conclusion)
        print(result.summary)
    finally:
        await client.close()


asyncio.run(main())
```

## Verification

```sh
scripts/verify-rfc-0001-examples.sh
```

The implementation ledger lives at
`docs/rfcs/0001-implementation-progress.md`.
