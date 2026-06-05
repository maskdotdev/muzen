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
- [x] Rust core exposes workspace-owned model and provider profile records with
  secret-reference-only config snapshots.
- [x] SDK-first `@muzen/sdk` package exists.
- [x] `createMuzen()` works end to end against `muzen-runner`.
- [x] `ReviewSession` handle supports `subscribe`, `events`, `wait`,
  `result`, `cancel`, and `refresh`.
- [x] Source string shorthand parses into typed source descriptors.
- [x] Typed source builders exist for GitHub and GitLab.
- [x] Workspace-owned profile APIs exist.
- [x] Webhook helpers exist.
- [x] Rust core exposes framework-agnostic HTTP/SSE response helpers for review
  events and webhook deliveries.
- [x] Developer docs and examples run against the implementation.

## Phase 1: Progress Control And Contract Alignment

- [x] Create this progress tracker.
- [x] Audit runner schema against RFC method needs.
- [x] Document the temporary mapping from durable review-session API to current
  runner `run.*` protocol methods.
- [x] Add protocol drift checks where missing.

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

- [x] Add a Rust facade that can create a review session against the existing
  runner execution path.
- [x] Keep runner `run.*` protocol details out of public review-session types.
- [x] Represent review event records in SDK-facing product vocabulary.
- [x] Support `review.wait()` equivalent semantics in Rust core.
- [x] Support `review.result()` equivalent semantics in Rust core.
- [x] Support `review.cancel()` equivalent semantics where the current runner
  can honor it.
- [x] Support `review.refresh()` equivalent semantics in Rust core.

Exit criteria:

- Rust can schedule or execute a local review through SDK-facing core types.
- The TypeScript and Python SDK work becomes transport/wrapper work, not core
  product modeling.

## Phase 4: TypeScript SDK Preview

- [x] Add `sdk/typescript/packages/muzen-sdk` package scaffold.
- [x] Define public SDK types for `Muzen`, `ReviewSession`, sources, options,
  events, results, errors, and runner configuration.
- [x] Implement a runner stdio client with:
  - [x] handshake negotiation,
  - [x] request/response correlation,
  - [x] notification dispatch,
  - [x] typed protocol errors,
  - [x] graceful process shutdown.
- [x] Implement `createMuzen()`.
- [x] Implement `createMuzenClient()` as an explicit unsupported preview or
  remote-client placeholder until HTTP transport exists.
- [x] Implement `createReviewSession()` as sugar over `createMuzen().review()`.
- [x] Implement `muzen.review(...)`.
- [x] Implement `muzen.resumeReview(...)` for locally known runner sessions.
- [x] Implement `ReviewSession.subscribe(...)`.
- [x] Implement `ReviewSession.events(...)`.
- [x] Implement `ReviewSession.wait(...)`.
- [x] Implement `ReviewSession.result(...)`.
- [x] Implement `ReviewSession.cancel(...)`.
- [x] Implement `ReviewSession.refresh(...)`.

Exit criteria:

- A TypeScript script can create a review against a local repo, receive events,
  wait for a result, and close the runner.

## Phase 5: Source And Review API Ergonomics

- [x] Implement `github.pullRequest(...)`.
- [x] Implement `gitlab.mergeRequest(...)`.
- [x] Implement source string parsing:
  - [x] `github:owner/repo#123`,
  - [x] `gitlab:owner/repo!123`.
- [x] Implement local repository source support for SDK smoke tests.
- [x] Map friendly `ReviewOptions` into runner `RunStartParams`.
- [x] Normalize review result shape from runner result shape.
- [x] Provide compatibility helpers for artifact read/export after final result.

Exit criteria:

- The public SDK example in the RFC compiles.
- Local source and provider source examples are represented by typed
  descriptors.

## Phase 6: Python SDK Preview

- [x] Add `sdk/python/muzen` package scaffold.
- [x] Wrap the Rust-owned runner/review-session contracts with Python
  dataclass models.
- [x] Implement Python process supervision for `muzen-runner`.
- [x] Implement Python `Client.create()`.
- [x] Implement Python review session event iteration and final result access.
- [x] Add a notebook-friendly basic review example.
- [x] Add Python remote/workspace profile wrappers.

Exit criteria:

- Python can run the same basic review flow as TypeScript through the same
  Rust-owned protocol contracts.

## Phase 7: Examples And Docs

- [x] Add `examples/typescript/basic-review`.
- [x] Add `examples/typescript/events`.
- [x] Add `examples/python/basic-review`.
- [x] Add `examples/python/notebook-review`.
- [x] Update `Readme.md` so examples are accurate for implemented preview
  behavior.
- [x] Document unsupported preview areas without exposing internals as the main
  mental model.

Exit criteria:

- A fresh contributor can run the basic TypeScript example from the repository.

## Phase 8: Durable Session Store And Worker Semantics

- [x] Design durable session store boundary separate from runner stdio protocol.
- [x] Implement review session records.
- [x] Implement event persistence and replay cursors.
- [x] Implement result persistence separate from artifacts.
- [x] Implement dedupe policies:
  - [x] `none`,
  - [x] `source`,
  - [x] `source-head`,
  - [x] application-defined key.
- [x] Implement durable cancellation.
- [x] Implement worker claim and lease semantics.
- [x] Implement retry policy and backoff.
- [x] Implement worker concurrency limits:
  - [x] claim batch and global running limits,
  - [x] workspace limits,
  - [x] user limits,
  - [x] model profile limits,
  - [x] provider profile limits.
- [x] Wire Rust workspace scheduling to queued durable records and a worker
  execution loop.
- [ ] Decide when `workspace.review(...)` and language SDK happy paths should
  switch from inline preview execution to queued durable execution.

Exit criteria:

- The SDK-first API is backed by durable sessions instead of only local runner
  process state.

## Phase 9: Workspace-Owned Configuration And BYOK

- [x] Define host configuration model.
- [x] Define workspace profile records.
- [x] Implement model profile set/get/list.
- [x] Implement provider profile set/get/list.
- [x] Capture effective config snapshots when scheduling workspace reviews.
- [x] Store only secret references and non-secret routing metadata in review
  session records.
- [x] Add redaction tests for durable records, events, and results.
- [ ] Add log redaction tests when review-session log persistence exists.

Exit criteria:

- Multi-tenant and BYOK deployments can schedule reviews without persisting raw
  credentials.

## Phase 10: Webhooks And Remote Mode

- [x] Implement GitHub webhook verification and source mapping.
- [x] Implement GitLab webhook verification and source mapping.
- [x] Implement webhook response helpers.
  - [x] Add Rust delivery JSON response body helper.
  - [x] Add Rust framework-agnostic HTTP response helper.
  - [x] Add framework-facing SDK response helpers.
- [ ] Implement high-level SDK webhook request handlers.
  - [x] Add TypeScript remote `muzen.webhooks.github.response(request)` facade.
  - [ ] Add local/host TypeScript `createMuzen().webhooks.github.response(request)`
    facade once the Rust host/router exists.
- [x] Define remote HTTP API contract.
- [x] Implement `createMuzenClient({ baseUrl })`.
- [x] Add SSE event response support.
  - [x] Define SSE route contract and TypeScript remote `eventsResponse`.
  - [x] Implement service-side SSE route response helper in Rust.

Exit criteria:

- Apps can receive webhook events, schedule durable reviews, and stream review
  events to clients.

## Verification Ledger

Record every milestone with the commands that were run.

| Date | Commit | Scope | Verification |
| ---- | ------ | ----- | ------------ |
| 2026-06-05 | 8f98757 | Progress tracker baseline | Documentation-only commit |
| 2026-06-05 | 6be8151 | Rust core review-session contracts | `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | 5fd89a5 | Rust local review-session facade | `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | efaa992 | TypeScript SDK preview over `muzen-runner` | `cargo build --bin muzen-runner`; `npm run build`; `MUZEN_RUNNER_PATH=/Users/e464543/code/muzen/target/debug/muzen-runner npm test` |
| 2026-06-05 | 79d1ce6 | Python SDK preview over `muzen-runner` | `PYTHONPATH=/Users/e464543/code/muzen/sdk/python MUZEN_RUNNER_PATH=/Users/e464543/code/muzen/target/debug/muzen-runner python3 -m unittest discover -s tests` |
| 2026-06-05 | f12ea0b | Preview README and basic examples | `PYTHONPATH=/Users/e464543/code/muzen/sdk/python MUZEN_RUNNER_PATH=/Users/e464543/code/muzen/target/debug/muzen-runner python3 examples/python/basic_review.py . Cargo.toml` |
| 2026-06-05 | 4643441 | Redacted artifact read/export helpers in Rust, TypeScript, and Python | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test`; `MUZEN_RUNNER_PATH=/Users/e464543/code/muzen/target/debug/muzen-runner npm test`; `PYTHONPATH=/Users/e464543/code/muzen/sdk/python MUZEN_RUNNER_PATH=/Users/e464543/code/muzen/target/debug/muzen-runner python3 -m unittest discover -s tests` |
| 2026-06-05 | c9fbbc6 | Rust review-session store boundary with in-memory records, replay, results, and dedupe | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | 387629c | Runner protocol mapping and drift gate audit | `cargo test runner::tests --lib` |
| 2026-06-05 | e939764 | Rust workspace profile records, model/provider profile APIs, and effective config snapshots | `cargo fmt --check`; `cargo test` |
| 2026-06-05 | 20f556f | Rust durable cancellation, worker claims, leases, retry backoff, and workspace claim limits | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | b7f1abc | Rust host scheduling config and global/workspace/user/model/provider worker concurrency limits | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | cc1c12c | Rust queued workspace scheduling and worker execution loop over durable session records | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | f524c73 | Serializable durable review records and redaction coverage for records, events, and results | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | eec90c9 | Rust GitHub/GitLab webhook verification, source mapping, queued scheduling, dedupe, and delivery JSON bodies | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | 56f1af7 | TypeScript event example and Python notebook review example | `node -e "JSON.parse(...)"`; `npm test`; `PYTHONPATH=/Users/e464543/code/muzen/sdk/python python3 -m unittest discover -s sdk/python/tests`; `cargo build --bin muzen-runner`; `PYTHONPATH=/Users/e464543/code/muzen/sdk/python MUZEN_RUNNER_PATH=/Users/e464543/code/muzen/target/debug/muzen-runner python3 examples/python/basic_review.py . Cargo.toml` |
| 2026-06-05 | a3384a6 | Remote HTTP API contract and TypeScript `createMuzenClient({ baseUrl })` client | `npm test` |
| 2026-06-05 | f42dbb6 | TypeScript remote workspace review/model/provider profile APIs and HTTP contract endpoints | `npm test` |
| 2026-06-05 | 71ea9e9 | Python remote client and workspace review/model/provider profile APIs | `PYTHONPATH=/Users/e464543/code/muzen/sdk/python python3 -m unittest discover -s sdk/python/tests` |
| 2026-06-05 | 6b51700 | Docs/examples verification harness for RFC 0001 | `scripts/verify-rfc-0001-examples.sh` |
| 2026-06-05 | d90e6f7 | Rust review HTTP/SSE response core for webhook deliveries and event replay/stream routes | `cargo fmt --check`; `cargo test review_session --lib`; `cargo test` |
| 2026-06-05 | 5378435 | TypeScript and Python webhook delivery response helpers | `npm test`; `PYTHONPATH=/Users/e464543/code/muzen/sdk/python python3 -m unittest discover -s sdk/python/tests`; `scripts/verify-rfc-0001-examples.sh` |
| 2026-06-05 | 4b3ce54 | README aligned to the intended production SDK flow while labeling preview gaps | Documentation-only commit |
| 2026-06-05 | pending | TypeScript remote webhook request facade and HTTP contract routes | `npm test` |

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
- The Rust `MuzenWorkspace` facade now has explicit queued scheduling and a
  worker execution loop. The default `review(...)` happy path still executes
  local reviews synchronously for preview compatibility.
- The store boundary has an in-memory implementation for local execution,
  tests, durable cancellation, worker claims, leases, retries, and workspace
  claim limits. A production database implementation with transactional
  `SKIP LOCKED`-style claiming remains open.
- Workspace model/provider profiles have Rust core APIs and in-memory storage.
  Persistent profile storage and SDK workspace wrappers remain open.
- Host scheduling configuration now defines lease defaults, retry defaults,
  fairness strategy, and global/workspace/user/model/provider concurrency
  limits. Production persistence still needs to enforce the same contract
  transactionally.
- Durable records, events, and results have redaction coverage around profile
  secret references. Log-specific redaction coverage should be added with the
  log persistence boundary.
- Rust webhook helpers verify GitHub HMAC-SHA256 signatures and GitLab webhook
  tokens, map supported pull/merge request events to review sources, schedule
  queued workspace reviews, and return delivery JSON bodies plus
  framework-agnostic HTTP responses. TypeScript and Python SDK response helpers
  now turn webhook deliveries into framework-facing HTTP responses. The
  TypeScript remote client forwards
  `muzen.webhooks.github.response(request)` calls to the remote service. The
  local `createMuzen().webhooks.github.response(request)` facade still depends
  on a bound Rust host/router.
- TypeScript event and Python notebook examples are present. The
  `scripts/verify-rfc-0001-examples.sh` gate builds the runner, runs the
  TypeScript SDK tests, typechecks TypeScript examples, runs Python SDK tests,
  executes the Python basic review example, and validates the notebook JSON.
- The remote HTTP contract is documented in
  `docs/rfcs/0001-remote-http-api-contract.md`; the TypeScript SDK can create,
  resume, wait for, cancel, replay events for, stream events for, and read/export
  artifacts from remote reviews through `createMuzenClient({ baseUrl })`.
  Rust core now exposes replay and SSE event responses; a bound web server/router
  remains open.
- Workspace-owned profile APIs exist in Rust core, the TypeScript remote SDK,
  and the Python remote SDK.

## Notes For Reviewers

The first implementation bridge will map Rust-owned review-session semantics
onto the current runner `run.*` execution path. That gives the desired
developer-facing shape early while keeping durable store, worker leases, remote
mode, and language SDKs as explicit later phases instead of hidden
half-implementations.
