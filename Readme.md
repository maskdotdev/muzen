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
- Durable libSQL-backed SQLite stores by default, with an explicit in-memory
  store mode
- Project-scoped model and provider profiles with secret references (no raw
  credentials)
- GitHub and GitLab webhook verification, source mapping, and queued scheduling
- Full HTTP API: review creation, SSE streaming, results, artifacts,
  cancellation, webhooks, and project profiles
- An Axum-backed `muzen-service` binary wrapping the core HTTP router
- Pull request and merge request materialization with temporary Git checkouts,
  token-safe auth, provider base URL routing, and changed-file inference

**Still hardening:**

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
    local("."),
    {
      scope: { files: ["Cargo.toml"] },
    },
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

`muzen-service` exposes the full HTTP API from RFC 0001. It uses
`sqlite://.muzen/muzen.db` for durable local storage by default. Override that
with `--store-url` or `MUZEN_STORE_URL` to use `sqlite://` or explicit
non-durable `memory://` storage.
Production deployments should read
[`docs/production-operations.md`](docs/production-operations.md), especially
the notes on external HTTP API authentication and preview schema resets.

```sh
# Optional; this is the default when omitted.
MUZEN_STORE_URL=sqlite://.muzen/muzen.db
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

const project = muzen.project("acme");

await project.models.set("default", {
  provider: "openai_compatible",
  model: "gpt-5",
  secretRef: "vault://projects/acme/models/default",
});

await project.providers.set("github", {
  provider: "github",
  secretRef: "vault://projects/acme/providers/github",
});

const review = await project.review("github:maskdotdev/heimdaal#123", {
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
merge request events to review sources, and queue project reviews
automatically.

## Python

Python shares the same runner protocol as TypeScript:

```py
import asyncio
import os

from muzen import Client, ReviewOptions, local


async def main() -> None:
    client = await Client.create(
        runner_path=os.environ.get("MUZEN_RUNNER_PATH"),
    )
    try:
        review = await client.review(
            local("."),
            ReviewOptions(scope_files=["Cargo.toml"]),
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
                         SQLite or memory stores

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
