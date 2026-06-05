# RFC 0001 Durable Happy Path Decision

Generated: 2026-06-05
RFC: `docs/rfcs/0001-sdk-first-review-sessions.md`

## Decision

Production Muzen happy paths are durable and queued. A call to
`muzen.review(...)` or `workspace.review(...)` in a hosted or remote Muzen
service schedules a durable review session, returns a live `ReviewSession`
handle, and lets workers own execution, leases, retries, cancellation, event
persistence, and final results.

The local `createMuzen()` runner preview remains an inline compatibility bridge
until the durable SDK service-boundary switch is complete. This exception is
scoped to the preview runner path and must not shape the production API
vocabulary.

## API Contract

- `createMuzenClient({ baseUrl }).review(...)` is the production client-only
  happy path and expects the remote service to schedule durable work.
- `createMuzenClient({ baseUrl }).workspace(id).review(...)` is the
  workspace-scoped durable scheduling path.
- Rust `MuzenWorkspace::schedule_review(...)` is the canonical core operation
  for durable workspace scheduling.
- Rust `MuzenWorkspace::review(...)` and SDK local `createMuzen().review(...)`
  remain preview conveniences for inline local repository execution.
- Provider-backed local `createMuzen().review("github:...")` is resolved by
  Rust runner provider materialization before inline preview execution.
- Webhook paths schedule durable reviews through Rust core, including the local
  TypeScript preview facade through `webhook.github.handle` and
  `webhook.gitlab.handle`.

## Switch Criteria

Embedded `createMuzen()` should switch its default happy path from inline runner
execution to queued durable execution when all of these are true:

1. A Rust host/router boundary exists for review creation, webhook handling,
   event replay, SSE streaming, worker control, and artifacts. Implemented by
   `ReviewHttpRouter`, `muzen-service`, and the runner worker/webhook protocol.
2. A production `ReviewSessionStore` implementation exists with transactional
   claiming and lease semantics equivalent to the in-memory store contract.
   Implemented by `PostgresReviewSessionStore`.
3. Workspace profile persistence exists for model/provider records and
   secret-reference snapshots. Implemented by `PostgresWorkspaceProfileStore`.
4. Provider source materialization exists for GitHub and GitLab pull/merge
   requests. Implemented by Rust runner provider materialization.
5. SDK `review.wait()`, `events()`, `eventsResponse()`, `cancel()`,
   `refresh()`, `readArtifact()`, and `exportArtifacts()` all operate through
   the durable service boundary.

Until those criteria are met, local inline execution is acceptable only as a
preview/testing bridge. The README should keep the production flow visible while
labeling preview gaps directly.

## Rationale

Switching the local preview immediately would produce queued reviews that cannot
complete without a production service boundary. That would make the
first-contact SDK API look durable while leaving `review.wait()` weak or
misleading.

Keeping the production path durable-first and the local path explicitly
preview-only preserves the intended developer experience without hiding current
implementation gaps.
