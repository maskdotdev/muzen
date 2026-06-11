# @muzen/sdk

Muzen is a Rust-first review automation runtime. The TypeScript SDK provides
the application-facing API for starting reviews, subscribing to progress,
handling provider webhooks, running workers, and connecting to a remote Muzen
service.

## 1. Install

```sh
npm install @muzen/sdk
```

## 2. Configure

```sh
MUZEN_STORE_URL=sqlite://.muzen/muzen.db
GITHUB_TOKEN=...
GITHUB_WEBHOOK_SECRET=...
OPENAI_API_KEY=...
```

## 3. Run A Review

```ts
import { createMuzen } from "@muzen/sdk";

const muzen = await createMuzen();

const review = await muzen.review("github:maskdotdev/heimdaal#123");

console.log(await review.wait());
```

## 4. Subscribe To Progress

```ts
review.subscribe((event) => {
  console.log(event.type);
});
```

## 5. Handle GitHub Webhooks

```ts
export async function POST(request: Request) {
  return muzen.webhooks.github.response(request);
}
```

## 6. Run Workers In Production

```ts
const muzen = await createMuzen();

await muzen.workers.start();
```

## 7. Connect To A Remote Muzen Service

```ts
import { createMuzenClient } from "@muzen/sdk";

const muzen = createMuzenClient({
  baseUrl: process.env.MUZEN_URL,
});

const review = await muzen.review("github:maskdotdev/heimdaal#123");

console.log(await review.wait());
```

## Current Preview Status

This README leads with the intended production developer experience. The
repository preview currently implements the Rust core contracts and SDK
transports in committed slices, while a few production pieces are still being
completed:

- local preview reviews run through the Rust `muzen-runner` process;
- remote review, event, result, artifact, webhook, and workspace profile client
  APIs are implemented against the documented HTTP contract;
- local and remote webhook response facades are implemented for GitHub and
  GitLab;
- local worker execution is exposed through `muzen.workers.runOnce()` and
  `muzen.workers.start()`, backed by Rust `ReviewWorker` core;
- `muzen-service` uses durable local SQLite by default and can be pointed at
  explicit SQLite, Postgres, or non-durable memory stores with `MUZEN_STORE_URL`
  or `--store-url`;
- provider-backed review sources are forwarded to Rust runner core, which
  materializes GitHub/GitLab pull/merge request refs into temporary Git
  checkouts with token-safe auth headers and changed-file inference.

See the repository root README and
`docs/rfcs/0001-implementation-progress.md` for the full RFC implementation
ledger.
