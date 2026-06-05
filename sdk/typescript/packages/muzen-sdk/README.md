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
DATABASE_URL=postgres://...
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
- production database persistence, provider materialization for local provider
  sources, a concrete Rust HTTP listener/framework adapter, and the high-level
  `muzen.workers.start()` facade remain future production work.

See the repository root README and
`docs/rfcs/0001-implementation-progress.md` for the full RFC implementation
ledger.
