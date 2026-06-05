# RFC 0001 Implementation Progress

Generated: 2026-06-05
RFC: `docs/rfcs/0001-sdk-first-review-sessions.md`

## Objective

Complete the SDK-first review sessions RFC with production-quality architecture,
reviewable milestones, and a developer experience centered on:

```ts
const muzen = await createMuzen();
const review = await muzen.review("github:maskdotdev/heimdaal#123");
const result = await review.wait();
```

This tracker is the source of truth for implementation status, open design
pressure, and commit-sized milestones.

## Implementation Principles

- Preserve the Rust runner as the execution kernel and keep SDKs on stable
  protocol contracts.
- Put all core behavior in Rust. TypeScript and Python SDKs should provide
  ergonomic APIs, process supervision, validation, and language-native helpers
  over stable Rust-owned contracts.
- Keep happy-path SDK APIs centered on product nouns: `Muzen`,
  `ReviewSession`, `review`, `workspace`, `result`, and `events`.
- Hide runner paths, leases, attempts, and queue internals from first-contact
  APIs.
- Make event streaming, replay, cancellation, and final results first-class.
- Never store raw provider tokens or model API keys in review records, events,
  logs, or final results.
- Prefer narrow, reviewable commits that leave the repository buildable.

## Current Baseline

- [x] RFC exists at `docs/rfcs/0001-sdk-first-review-sessions.md`.
- [x] Rust runner binary exists as `muzen-runner`.
- [x] Runner exposes newline-delimited JSON-RPC over stdio.
- [x] Runner supports `runner.handshake`, `runner.check`,
  `runner.schema.export`, `run.start`, `run.cancel`, `run.status`,
  `run.result`, `artifact.read`, `artifact.export`, and `snapshot.readText`.
- [x] Runner emits `event.review`, `event.runtime`, `run.finished`, and
  `run.failed` notifications.
- [x] Protocol fixtures exist under `fixtures/`.
- [x] Rust core exposes a public SDK-facing review-session module.
- [x] Rust core models review sources, review sessions, events, results,
  cancellation, and session snapshots using product nouns.
- [x] Rust core maps review-session semantics onto runner execution without
  leaking runner internals into SDK contracts.
- [ ] SDK-first `@muzen/sdk` package exists.
- [ ] `createMuzen()` works end to end against `muzen-runner`.
- [ ] `ReviewSession` handle supports `subscribe`, `events`, `wait`,
  `result`, `cancel`, and `refresh`.
- [ ] Source string shorthand parses into typed source descriptors.
- [ ] Typed source builders exist for GitHub and GitLab.
- [ ] Workspace-owned profile APIs exist.
- [ ] Webhook helpers exist.
- [ ] Developer docs and examples run against the implementation.

## Phase 1: Progress Control And Contract Alignment

- [x] Create this progress tracker.
- [ ] Audit runner schema against RFC method needs.
- [ ] Document the temporary mapping from durable review-session API to current
  runner `run.*` protocol methods.
- [ ] Add protocol drift checks where missing.

Exit criteria:

- The repository has an explicit implementation ledger.
- Reviewers can see what is already implemented, what is stubbed, and what is
  intentionally deferred.

## Phase 2: Rust Core Review Session API

- [x] Add a Rust `review_session` module for SDK-facing core types and
  behavior.
- [x] Define Rust core types for:
  - [x] `ReviewSource`,
  - [x] `ReviewSourceLike` parsing inputs,
  - [x] `ReviewOptions`,
  - [x] `ReviewSessionId`,
  - [x] `ReviewStatus`,
  - [x] `ReviewSessionSnapshot`,
  - [x] `ReviewResult`,
  - [x] `ReviewFinding`,
  - [x] `ReviewEvent`.
- [x] Implement source string parsing:
  - [x] `github:owner/repo#123`,
  - [x] `gitlab:owner/repo!123`,
  - [x] local repository source descriptors for smoke tests.
- [x] Add Rust constructors/builders that express the RFC nouns without
  requiring TypeScript first.
- [x] Add conversion from Rust review-session request types into runner
  `RunStartParams`.
- [x] Add conversion from runner `RunnerRunResult` into Rust `ReviewResult`.
- [x] Add unit tests for source parsing, result conversion, and secret-safe
  serialization.

Exit criteria:

- Rust owns the review-session contract that SDKs will wrap.
- The core crate can represent the RFC happy path without TypeScript-specific
  assumptions.

## Phase 3: Rust Session Execution Facade

- [ ] Add a Rust facade that can create a review session against the existing
  runner execution path.
- [ ] Keep runner `run.*` protocol details out of public review-session types.
- [ ] Represent review event records in SDK-facing product vocabulary.
- [ ] Support `review.wait()` equivalent semantics in Rust core.
- [ ] Support `review.result()` equivalent semantics in Rust core.
- [ ] Support `review.cancel()` equivalent semantics where the current runner
  can honor it.
- [ ] Support `review.refresh()` equivalent semantics in Rust core.

Exit criteria:

- Rust can schedule or execute a local review through SDK-facing core types.
- The TypeScript and Python SDK work becomes transport/wrapper work, not core
  product modeling.

## Phase 4: TypeScript SDK Preview

- [ ] Add `sdk/typescript/packages/muzen-sdk` package scaffold.
- [ ] Define public SDK types for `Muzen`, `ReviewSession`, sources, options,
  events, results, errors, and runner configuration.
- [ ] Implement a runner stdio client with:
  - [ ] handshake negotiation,
  - [ ] request/response correlation,
  - [ ] notification dispatch,
  - [ ] typed protocol errors,
  - [ ] graceful process shutdown.
- [ ] Implement `createMuzen()`.
- [ ] Implement `createMuzenClient()` as an explicit unsupported preview or
  remote-client placeholder until HTTP transport exists.
- [ ] Implement `createReviewSession()` as sugar over `createMuzen().review()`.
- [ ] Implement `muzen.review(...)`.
- [ ] Implement `muzen.resumeReview(...)` for locally known runner sessions.
- [ ] Implement `ReviewSession.subscribe(...)`.
- [ ] Implement `ReviewSession.events(...)`.
- [ ] Implement `ReviewSession.wait(...)`.
- [ ] Implement `ReviewSession.result(...)`.
- [ ] Implement `ReviewSession.cancel(...)`.
- [ ] Implement `ReviewSession.refresh(...)`.

Exit criteria:

- A TypeScript script can create a review against a local repo, receive events,
  wait for a result, and close the runner.

## Phase 5: Source And Review API Ergonomics

- [ ] Implement `github.pullRequest(...)`.
- [ ] Implement `gitlab.mergeRequest(...)`.
- [ ] Implement source string parsing:
  - [ ] `github:owner/repo#123`,
  - [ ] `gitlab:owner/repo!123`.
- [ ] Implement local repository source support for SDK smoke tests.
- [ ] Map friendly `ReviewOptions` into runner `RunStartParams`.
- [ ] Normalize review result shape from runner result shape.
- [ ] Provide compatibility helpers for artifact read/export after final result.

Exit criteria:

- The public SDK example in the RFC compiles.
- Local source and provider source examples are represented by typed
  descriptors.

## Phase 6: Python SDK Preview

- [ ] Add `sdk/python/muzen` package scaffold.
- [ ] Wrap the Rust-owned runner/review-session contracts with Pydantic models.
- [ ] Implement Python process supervision for `muzen-runner`.
- [ ] Implement Python `Client.create()`.
- [ ] Implement Python review session event iteration and final result access.
- [ ] Add a notebook-friendly basic review example.

Exit criteria:

- Python can run the same basic review flow as TypeScript through the same
  Rust-owned protocol contracts.

## Phase 7: Examples And Docs

- [ ] Add `examples/typescript/basic-review`.
- [ ] Add `examples/typescript/events`.
- [ ] Add `examples/python/basic-review`.
- [ ] Add `examples/python/notebook-review`.
- [ ] Update `Readme.md` so examples are accurate for implemented preview
  behavior.
- [ ] Document unsupported preview areas without exposing internals as the main
  mental model.

Exit criteria:

- A fresh contributor can run the basic TypeScript example from the repository.

## Phase 8: Durable Session Store And Worker Semantics

- [ ] Design durable session store boundary separate from runner stdio protocol.
- [ ] Implement review session records.
- [ ] Implement event persistence and replay cursors.
- [ ] Implement result persistence separate from artifacts.
- [ ] Implement dedupe policies:
  - [ ] `none`,
  - [ ] `source`,
  - [ ] `source-head`,
  - [ ] application-defined key.
- [ ] Implement durable cancellation.
- [ ] Implement worker claim and lease semantics.
- [ ] Implement retry policy and backoff.
- [ ] Implement worker concurrency limits.

Exit criteria:

- The SDK-first API is backed by durable sessions instead of only local runner
  process state.

## Phase 9: Workspace-Owned Configuration And BYOK

- [ ] Define host configuration model.
- [ ] Define workspace profile records.
- [ ] Implement model profile set/get/list.
- [ ] Implement provider profile set/get/list.
- [ ] Capture effective config snapshots when scheduling reviews.
- [ ] Store only secret references and non-secret routing metadata.
- [ ] Add redaction tests for records, events, logs, and results.

Exit criteria:

- Multi-tenant and BYOK deployments can schedule reviews without persisting raw
  credentials.

## Phase 10: Webhooks And Remote Mode

- [ ] Implement GitHub webhook verification and source mapping.
- [ ] Implement GitLab webhook verification and source mapping.
- [ ] Implement webhook response helpers.
- [ ] Define remote HTTP API contract.
- [ ] Implement `createMuzenClient({ baseUrl })`.
- [ ] Add SSE event response support.

Exit criteria:

- Apps can receive webhook events, schedule durable reviews, and stream review
  events to clients.

## Verification Ledger

Record every milestone with the commands that were run.

| Date | Commit | Scope | Verification |
| ---- | ------ | ----- | ------------ |
| 2026-06-05 | 8f98757 | Progress tracker baseline | Documentation-only commit |
| 2026-06-05 | core-contracts | Rust core review-session contracts | `cargo test review_session --lib`; `cargo test` |

## Open Decisions

- Should `ReviewSession.subscribe()` default to replay-and-follow or live-only?
  RFC recommendation is replay-and-follow.
- Should model profiles use named objects, arrays, or both?
- Should `createMuzenClient()` ship as a placeholder before HTTP transport, or
  wait until remote mode exists?
- How much local inline-worker behavior should `createMuzen()` own before the
  durable store exists?
- Provider sources currently parse into stable Rust descriptors but return an
  explicit unsupported error when converted to the local runner start params;
  provider materialization belongs in the durable source-resolution phase.

## Notes For Reviewers

The first implementation bridge will map Rust-owned review-session semantics
onto the current runner `run.*` execution path. That gives the desired
developer-facing shape early while keeping durable store, worker leases, remote
mode, and language SDKs as explicit later phases instead of hidden
half-implementations.
