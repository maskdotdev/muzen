> This is a very early idea. The SDK and API shown below are not working or implemented yet.

# Muzen

## Install

```sh
npm install @muzen/sdk
```

## Configure

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

## Run A Review

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

## Subscribe To Progress

```ts
review.subscribe((event) => {
  console.log(event.type);
});
```

## Handle GitHub Webhooks

```ts
export async function POST(request: Request) {
  return muzen.webhooks.github.response(request);
}
```

## Run Workers In Production

```ts
const muzen = await createMuzen();

await muzen.workers.start();
```

## Connect To A Remote Muzen Service

```ts
import { createMuzenClient } from "@muzen/sdk";

const muzen = createMuzenClient({
  baseUrl: process.env.MUZEN_URL,
});

const review = await muzen.review("github:maskdotdev/heimdaal#123");

console.log(await review.wait());
```

## Schedule Many Reviews

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
