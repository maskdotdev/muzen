# RFC 0001 Runner Protocol Mapping

Generated: 2026-06-05
RFC: `docs/rfcs/0001-sdk-first-review-sessions.md`

## Purpose

This document records the temporary bridge between the SDK-first
`ReviewSession` API and the current Rust runner protocol.

The public product model is:

```txt
Muzen.review(...)
ReviewSession.subscribe(...)
ReviewSession.events(...)
ReviewSession.wait(...)
ReviewSession.result(...)
ReviewSession.cancel(...)
ReviewSession.refresh(...)
ReviewSession.readArtifact(...)
ReviewSession.exportArtifacts(...)
```

The execution boundary is still:

```txt
muzen-runner stdio
newline-delimited JSON-RPC 2.0
muzen.runner.v1
```

This bridge is intentional. The TypeScript and Python SDKs provide ergonomic
language bindings; Rust owns the runner protocol, review-session contracts, and
execution semantics.

## Drift Checks

The runner schema is exported by `runner.schema.export` and mirrored in
`fixtures/runner-schema-v1.json`.

Current Rust tests verify:

- `runner::tests::schema_fixture_matches_current_schema`
- `runner::tests::handshake_fixture_matches_current_response`
- `runner::tests::schema_marks_wired_run_methods_and_callbacks_implemented`

These tests are the current protocol drift gate. Any incompatible runner method
change must update the fixture and the SDK mapping intentionally.

## Current Mapping

| SDK/core API | Runner protocol | Status | Notes |
| --- | --- | --- | --- |
| `createMuzen()` / `Client.create()` | `runner.handshake` | Implemented | Negotiates `muzen.runner.v1` and validates runner availability. |
| local `muzen.review(...)` | `run.start` | Implemented | Local sources map to `repo`, `changedFiles`, `sessions`, and `limits`. |
| provider `muzen.review(...)` | `run.start` | Implemented | GitHub/GitLab sources map to a Rust-owned `source` descriptor. The runner materializes pull/merge request refs into temporary Git checkouts, uses `GITHUB_TOKEN`/`GITLAB_TOKEN` auth headers when present, supports provider base URL routing, and infers changed files from the provider ref. |
| `ReviewSession.subscribe(...)` | `event.review` | Implemented | Current SDK previews replay events recorded during the local runner execution. |
| `ReviewSession.events(...)` | `event.review` | Implemented | Current previews replay recorded events; durable replay cursors are now modeled in the Rust store boundary. |
| `ReviewSession.wait(...)` | `run.start` result / `run.result` | Implemented | Local runner execution is synchronous today; durable scheduling will make this wait on persisted terminal state. |
| `ReviewSession.result(...)` | `run.result` | Implemented | Results are normalized into SDK-facing `ReviewResult`. |
| `ReviewSession.cancel(...)` | `run.cancel` / durable store cancellation | Partial | Durable review records preserve cancellation and reject late worker result overwrites. The local synchronous runner can still only report terminal-state cancellation responses. |
| `ReviewSession.refresh(...)` | `run.status` | Implemented | Current SDK previews refresh runner-local sessions. |
| `ReviewSession.readArtifact(...)` | `artifact.read` | Implemented | Defaults to redacted artifact view. |
| `ReviewSession.exportArtifacts(...)` | `artifact.export` | Implemented | Defaults to redacted artifact view and supports export limits. |
| source text reads | `snapshot.readText` | Runner implemented | Not yet exposed as a happy-path SDK helper. |
| local `muzen.webhooks.github.response(request)` | `webhook.github.handle` | Implemented | Verifies GitHub signatures and schedules a durable queued review through Rust core. |
| local `muzen.webhooks.gitlab.response(request)` | `webhook.gitlab.handle` | Implemented | Verifies GitLab tokens and schedules a durable queued review through Rust core. |
| local `muzen.workers.runOnce()` / `muzen.workers.start()` | `worker.runOnce` | Implemented | TypeScript worker ergonomics call Rust `ReviewWorker`; `start()` loops over Rust-owned claim/execute cycles. |
| custom model callbacks | `model.complete` | Runner implemented | SDK callback ergonomics remain future work. |
| custom tool callbacks | `tool.execute` | Runner implemented | SDK callback ergonomics remain future work. |

## Review Session Store Mapping

The Rust `review_session` module now includes a `ReviewSessionStore` boundary,
`InMemoryReviewSessionStore`, `LibsqlReviewSessionStore`, and
`PostgresReviewSessionStore`.

The store owns:

- review session records,
- event replay by cursor,
- final result persistence,
- redacted/raw artifact references,
- dedupe lookup keys.

The libSQL implementation is the default durable local SQLite store. It stores
JSON payloads as text with scalar queue fields for claiming and indexing. The
Postgres implementation stores durable session records as JSONB and uses
transactional `FOR UPDATE SKIP LOCKED` worker claims. The in-memory store
remains an explicit preview/test implementation.

## Known Gaps

- Webhook-scheduled `source-head` dedupe captures provider head revisions from
  GitHub/GitLab payloads. Direct provider reviews still fall back to the stable
  source key unless the caller supplies known head metadata.
- Interrupting an already executing local synchronous `run.start` remains a
  preview limitation; durable cancellation is preserved at the store boundary.
- A framework-neutral Rust HTTP router and Axum-backed `muzen-service` listener
  now exist around the core remote HTTP contract. `muzen-service` uses durable
  local SQLite by default, with explicit Postgres and memory store URL modes.
- A live provider materialization smoke test should run in deployment CI with
  real provider credentials and network access.
