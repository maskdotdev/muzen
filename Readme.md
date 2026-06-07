# Muzen

Code review automation that runs as a real service.

Most review automation is glue code that breaks the moment you need retries,
cancellation, streaming progress, or durable state. Muzen handles all of that so
you can treat a code review like an API call:

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();
const review = await muzen.review("github:maskdotdev/heimdaal#123");

review.subscribe((event) => {
  console.log(event.type);
});

const result = await review.wait();
console.log(result.conclusion);
console.log(result.summary);
```

A Rust runtime manages sessions, workers, event replay, webhooks, provider
checkouts, artifacts, and scheduling. TypeScript and Python SDKs give you a
clean interface over it -- you never have to think about the Rust layer unless
you want to.

## Current Status

Muzen is a preview implementation of RFC 0001. The repo builds and the APIs
work, but the surface is still settling.

**Working today:**

- Local reviews via the `muzen-runner` binary
- TypeScript and Python SDK previews over a shared runner protocol
- Durable review sessions with events, results, artifacts, logs, cancellation,
  retries, leases, and worker claims
- In-memory stores for local dev, Postgres-backed stores for real deployments
- Workspace-scoped model and provider profiles with secret references (no raw
  credentials)
- GitHub and GitLab webhook verification, source mapping, and queued scheduling
- Full HTTP API: review creation, SSE streaming, results, artifacts,
  cancellation, webhooks, and workspace profiles
- An Axum-backed `muzen-service` binary wrapping the core HTTP router
- Pull request and merge request materialization with temporary Git checkouts,
  token-safe auth, provider base URL routing, and changed-file inference

**Still hardening:**

- Postgres integration CI for environments with `DATABASE_URL`
- Live GitHub/GitLab provider smoke tests where tokens and network access are
  available
- Migration from local inline execution to durable service-bound SDK execution

Full implementation ledger:
[`docs/rfcs/0001-implementation-progress.md`](docs/rfcs/0001-implementation-progress.md)

## Try a Local Review

Build the runner and point an SDK at it:

```sh
cargo build --bin muzen-runner
export MUZEN_RUNNER_PATH="$PWD/target/debug/muzen-runner"
```

Then run a review:

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

More examples:

- `examples/typescript/basic-review`
- `examples/typescript/events`
- `examples/python/basic_review.py`
- `examples/python/notebook-review/notebook_review.ipynb`

## Run the Service

`muzen-service` exposes the full HTTP API from RFC 0001. Point it at Postgres
for durable storage, or omit `DATABASE_URL` to run with in-memory stores.
Production deployments should read
[`docs/production-operations.md`](docs/production-operations.md), especially
the notes on external HTTP API authentication and preview schema resets.

```sh
DATABASE_URL=postgres://...
GITHUB_WEBHOOK_SECRET=...
GITLAB_WEBHOOK_TOKEN=...
cargo run --bin muzen-service -- --bind 127.0.0.1:7341
```

Once the service is up, connect a remote client:

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

console.log(await review.wait());
```

## Webhooks

Webhook verification and event mapping live in Rust. Your framework route just
delegates:

```ts
export async function POST(request: Request) {
  return muzen.webhooks.github.response(request);
}
```

The helpers verify signatures (GitHub) or tokens (GitLab), map pull request and
merge request events to review sources, and queue workspace reviews
automatically.

## Python

Python shares the same runner protocol as TypeScript:

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

        async for event in review.events():
            print(event.type)

        result = await review.wait()
        print(result.conclusion)
        print(result.summary)
    finally:
        await client.close()


asyncio.run(main())
```

## Architecture

```text
TypeScript SDK       Python SDK
      |                  |
      +------ runner protocol ------+
                                    |
                              muzen-runner
                                    |
                              Rust review core
                                    |
        +-----------+-------------+-------------+
        |           |             |             |
     sessions    workers      webhooks      artifacts
        |           |             |             |
        +-----------+-------------+-------------+
                                    |
                      in-memory stores or Postgres

Remote clients use HTTP instead:

SDK -> muzen-service -> Rust review core -> stores/workers/events/results
```

The Rust boundary is an operational choice: SDKs stay small and ergonomic while
protocol validation, provider materialization, durable records, and worker
behavior share a single implementation.

## Verify the Repo

Run the full verification gate:

```sh
scripts/verify-rfc-0001-examples.sh
```

This builds `muzen-runner`, runs TypeScript and Python SDK tests, typechecks the
TypeScript examples, executes the Python basic review example, and validates the
notebook JSON.

## Docs

- [RFC 0001 -- SDK-First Review Sessions](docs/rfcs/0001-sdk-first-review-sessions.md)
- [Remote HTTP API Contract](docs/rfcs/0001-remote-http-api-contract.md)
- [Runner Protocol Mapping](docs/rfcs/0001-runner-protocol-mapping.md)
- [Implementation Progress](docs/rfcs/0001-implementation-progress.md)
