# TanStack Durable Review Example

This example shows the production-style Muzen flow with a small TanStack
frontend and a tiny host service:

```text
browser
  -> POST /api/reviews
  -> receives durable review id
  -> subscribes to /api/reviews/:id/events/stream
  -> fetches /api/reviews/:id/result when terminal

host service
  -> persists queued review
  -> worker claims review
  -> runs one Muzen review
  -> persists ordered events
  -> persists final result
```

The service uses an in-memory store so the example stays small. The important
part is the boundary: the browser never owns the review lifecycle, and the
worker emits events that can be replayed by cursor. For real production, replace
`src/server/store.ts` with Postgres or the Muzen durable service store.

## Run

Build the runner first:

```bash
cargo build --bin muzen-runner
```

Install this example:

```bash
cd examples/tanstack-durable-review
npm install
```

Start the host service in one terminal:

```bash
MUZEN_RUNNER_PATH=../../target/debug/muzen-runner npm run service
```

The service script prepares the local TypeScript SDK package before starting,
so it works from a fresh checkout.

Start the TanStack/Vite frontend in another terminal:

```bash
npm run dev
```

Open the Vite URL, then choose one of the targets:

- Local repo: submit a path such as `../..` and use `Cargo.toml` as the
  changed file.
- GitHub PR: submit a URL such as
  `https://github.com/owner/repo/pull/123`, `github:owner/repo#123`, or
  `owner/repo#123`.

For GitHub PR targets, leave changed files empty to let Muzen fetch the PR ref
and infer the changed files. Public PRs work over HTTPS. Private PRs require
`GITHUB_TOKEN` in the service environment:

```bash
GITHUB_TOKEN=... MUZEN_RUNNER_PATH=../../target/debug/muzen-runner npm run service
```

## What To Look At

- `src/server.ts` exposes the durable HTTP/SSE shape.
- `src/server/github.ts` parses real GitHub PR targets into provider-neutral
  Muzen sources.
- `src/server/store.ts` is the replaceable durable store boundary.
- `src/server/worker.ts` is the worker claim/execute/result projection path.
- `src/App.tsx` shows the browser flow: create one run, attach SSE, fetch
  result.
- `FLOW.md` walks through the sequence for one large merge request.

## Why This Is Not `Promise.all`

This example creates one durable review run and streams its events by id. A
large MR should become one Muzen run with multiple sessions inside it, not many
JavaScript promises that own execution.
