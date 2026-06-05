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
| provider `muzen.review(...)` | none yet | Pending | GitHub/GitLab sources parse into descriptors, but provider materialization is not implemented. |
| `ReviewSession.subscribe(...)` | `event.review` | Implemented | Current SDK previews replay events recorded during the local runner execution. |
| `ReviewSession.events(...)` | `event.review` | Implemented | Current previews replay recorded events; durable replay cursors are now modeled in the Rust store boundary. |
| `ReviewSession.wait(...)` | `run.start` result / `run.result` | Implemented | Local runner execution is synchronous today; durable scheduling will make this wait on persisted terminal state. |
| `ReviewSession.result(...)` | `run.result` | Implemented | Results are normalized into SDK-facing `ReviewResult`. |
| `ReviewSession.cancel(...)` | `run.cancel` | Preview | The current runner can report terminal-state cancellation responses; durable active cancellation is pending. |
| `ReviewSession.refresh(...)` | `run.status` | Implemented | Current SDK previews refresh runner-local sessions. |
| `ReviewSession.readArtifact(...)` | `artifact.read` | Implemented | Defaults to redacted artifact view. |
| `ReviewSession.exportArtifacts(...)` | `artifact.export` | Implemented | Defaults to redacted artifact view and supports export limits. |
| source text reads | `snapshot.readText` | Runner implemented | Not yet exposed as a happy-path SDK helper. |
| local `muzen.webhooks.github.response(request)` | `webhook.github.handle` | Implemented | Verifies GitHub signatures and schedules a durable queued review through Rust core. |
| local `muzen.webhooks.gitlab.response(request)` | `webhook.gitlab.handle` | Implemented | Verifies GitLab tokens and schedules a durable queued review through Rust core. |
| custom model callbacks | `model.complete` | Runner implemented | SDK callback ergonomics remain future work. |
| custom tool callbacks | `tool.execute` | Runner implemented | SDK callback ergonomics remain future work. |

## Review Session Store Mapping

The Rust `review_session` module now includes a `ReviewSessionStore` boundary
and `InMemoryReviewSessionStore` implementation.

The store owns:

- review session records,
- event replay by cursor,
- final result persistence,
- redacted/raw artifact references,
- dedupe lookup keys.

This is the contract a durable database-backed store should implement next.
The in-memory store is not a production durability substitute.

## Known Gaps

- Provider source materialization is not mapped to runner inputs yet.
- `source-head` dedupe currently uses the stable source key until source
  resolution can capture provider head revisions.
- Active cancellation, leases, retry policy, and backoff require worker-owned
  durable sessions.
- A bound Rust HTTP server/router is still needed around the core remote HTTP
  contract.
- Workspace-owned BYOK profile APIs need production persistent profile records
  and secret references.
