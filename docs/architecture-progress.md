# Muzen Architecture Progress

Generated: 2026-06-05

## Objective

Raise Muzen's architecture quality as close to 10/10 as practical for
reviewability, maintainability, ease of flow, developer experience, and
production readiness.

This tracker is the source of truth for architecture score, deepening
opportunities, execution order, and verification.

## Current Rating

**Current score: 8.2/10.**

Muzen has a strong foundation: Rust owns the core, the runner protocol and HTTP
contract are explicit, the durable store seams have in-memory and Postgres
adapters, provider materialization lives in Rust, and the RFC tracker has
reviewable milestones with broad verification.

The score is not higher yet because several newer Review Session and SDK
modules are shallow at their current size: their interfaces expose many
concepts at once, while routing, mapping, persistence, execution, and language
adapter behavior are still concentrated in large files. The architecture works,
but a reviewer has to hold too much in memory to understand one change.

## Score Rubric

- **10.0**: Deep modules with small interfaces, clear seams, two or more real
  adapters at important seams, stable domain vocabulary, narrow test surfaces,
  and obvious file navigation for a new maintainer.
- **9.0**: Production-ready architecture with a few known local complexity
  clusters that are well documented and low-risk.
- **8.0**: Strong working architecture with real seams, but some large modules
  and mixed responsibilities reduce locality.
- **7.0**: Correct behavior, but reviewability depends on tribal knowledge or
  cross-file tracing.
- **6.0 or below**: Important behavior is implicit, duplicated, or difficult to
  test through stable interfaces.

## Strengths

- Rust is the core for review sessions, workers, webhooks, provider
  materialization, HTTP routing, and runner execution.
- Language SDKs are thin adapters over stable runner and remote contracts.
- The Review Session store seam has two real adapters:
  `InMemoryReviewSessionStore` and `PostgresReviewSessionStore`.
- Workspace profile persistence also has two real adapters.
- Provider materialization has a narrow Rust runner module with deterministic
  local Git tests.
- Remote HTTP routing is framework-neutral, with Axum as a concrete adapter.
- Verification is broad: Rust tests, SDK tests, runner-backed tests, service
  builds, and RFC example checks.

## Main Friction

1. **Review Session root module is too large.**
   [src/review_session.rs](/Users/e464543/code/muzen/src/review_session.rs)
   owns domain types, orchestration, conversions, helper functions, and a large
   test suite. The module is important, but it is currently too hard to review
   one behavior without scrolling through unrelated concepts.

2. **Store implementations share one large file.**
   [src/review_session/store.rs](/Users/e464543/code/muzen/src/review_session/store.rs)
   contains the store interface, in-memory adapter, Postgres adapter, schema,
   serialization helpers, claim logic, redaction, retry, and lease behavior.
   The seam is real, but the implementation locality inside the file is weak.

3. **TypeScript SDK orchestration is concentrated in one file.**
   [sdk/typescript/packages/muzen-sdk/src/muzen.ts](/Users/e464543/code/muzen/sdk/typescript/packages/muzen-sdk/src/muzen.ts)
   mixes local runner client behavior, remote client behavior, workers,
   webhooks, result mapping, validation, profile collections, and utility
   helpers. This makes SDK behavior harder to review than the public interface
   suggests.

4. **Python SDK mirrors the same concentration.**
   [sdk/python/muzen/client.py](/Users/e464543/code/muzen/sdk/python/muzen/client.py)
   has similar locality pressure, though at a smaller scale.

5. **Runner protocol schema is metadata-only.**
   The schema fixture guards method drift, but request/response payload shapes
   are still enforced mostly by Rust/SDK tests rather than a single wire-schema
   module.

6. **Architecture vocabulary was implicit.**
   There was no `CONTEXT.md`, so domain terms like Review Session, Review
   Source, Review Worker, Runner Protocol, and Provider Materialization were
   not centralized for future architecture reviews.

## Deepening Opportunities

### 1. Split Review Session Domain Types From Orchestration

Files:

- [src/review_session.rs](/Users/e464543/code/muzen/src/review_session.rs)
- new `src/review_session/types.rs`
- new `src/review_session/session.rs`
- new `src/review_session/options.rs`

Problem:

The root module interface is deep at the crate level, but the file itself is
shallow for maintainers: types, behavior, conversion helpers, and tests are all
interleaved.

Solution:

Move pure domain values and conversions into focused submodules while keeping
the public re-export surface stable. Preserve caller imports and tests while
improving locality.

Benefits:

- Reviewers can inspect source parsing, options/dedupe, session lifecycle, and
  result mapping independently.
- Tests can sit closer to the interface they validate.
- Future SDK/service changes touch fewer unrelated lines.

Expected score lift: **+0.4**.

### 2. Split Store Interface From Store Adapters

Files:

- [src/review_session/store.rs](/Users/e464543/code/muzen/src/review_session/store.rs)
- new `src/review_session/store/mod.rs`
- new `src/review_session/store/memory.rs`
- new `src/review_session/store/postgres.rs`
- new `src/review_session/store/schema.rs`

Problem:

The store seam is real, but all adapter details live together. This weakens
locality and makes Postgres changes harder to review without also scanning
in-memory behavior and trait definitions.

Solution:

Keep the trait and record types in `store/mod.rs`, move concrete adapters and
schema helpers into their own modules, and keep adapter tests scoped to the
adapter behavior.

Benefits:

- Clearer adapter ownership.
- Easier review of database migrations and claim semantics.
- Cleaner path toward live Postgres integration tests.

Expected score lift: **+0.5**.

### 3. Split TypeScript SDK Local And Remote Adapters

Files:

- [sdk/typescript/packages/muzen-sdk/src/muzen.ts](/Users/e464543/code/muzen/sdk/typescript/packages/muzen-sdk/src/muzen.ts)
- new `remote.ts`
- new `local.ts`
- new `mapping.ts`
- new `validation.ts`

Problem:

One SDK file currently hides several modules: local runner adapter, remote HTTP
adapter, review-session handles, workers, webhooks, wire mapping, and runtime
validation.

Solution:

Create local/remote adapter modules behind the existing public exports. Move
wire mapping and validators into shared helpers.

Benefits:

- Public API stays stable while review locality improves.
- Remote and local behavior can evolve independently.
- Tests can target mapping/validation without booting runner paths.

Expected score lift: **+0.4**.

### 4. Split Python SDK Along The Same Adapter Lines

Files:

- [sdk/python/muzen/client.py](/Users/e464543/code/muzen/sdk/python/muzen/client.py)
- new `local.py`
- new `remote.py`
- new `mapping.py`

Problem:

The Python SDK is smaller but mirrors the TypeScript concentration.

Solution:

Apply the same adapter split after TypeScript proves the shape.

Benefits:

- Cross-language architecture is easier to compare.
- Python tests become more focused.
- SDK parity gaps are easier to see.

Expected score lift: **+0.2**.

### 5. Add A Wire Contract Module

Files:

- [src/runner/types.rs](/Users/e464543/code/muzen/src/runner/types.rs)
- [src/runner/schema.rs](/Users/e464543/code/muzen/src/runner/schema.rs)
- [fixtures/runner-schema-v1.json](/Users/e464543/code/muzen/fixtures/runner-schema-v1.json)

Problem:

Runner methods are advertised, but payload shape documentation and schema drift
coverage are still distributed across Rust types, SDK mapping tests, and
fixtures.

Solution:

Make runner request/response payload schemas a first-class protocol contract
module and update fixtures to include shape metadata.

Benefits:

- SDK authors get one protocol source of truth.
- Reviewers can audit wire compatibility without reconstructing payloads from
  implementation code.
- Accidental wire drift becomes harder.

Expected score lift: **+0.3**.

## Execution Plan

1. **Baseline tracker and vocabulary.**
   Create this tracker and `CONTEXT.md`.

2. **Review Session split.**
   Move source/options/result/session domain groups into submodules without
   changing public exports.

3. **Store adapter split.**
   Separate store trait/records, in-memory adapter, Postgres adapter, and
   schema helpers.

4. **TypeScript SDK split.**
   Separate local runner and remote HTTP adapters, then extract mapping and
   validation helpers.

5. **Python SDK split.**
   Mirror the SDK adapter separation once TypeScript shape is stable.

6. **Wire contract deepening.**
   Add payload shape metadata to runner schema fixtures and ensure Rust/SDK
   tests use it.

## Progress Ledger

| Date | Commit | Score | Scope | Verification |
| ---- | ------ | ----- | ----- | ------------ |
| 2026-06-05 | e3fad31 | 8.2 | Architecture baseline tracker and domain vocabulary | Documentation-only commit |

## Current Target

Target score after the planned slices: **9.4/10**.

The remaining 0.6 depends on production integration evidence that should happen
in deployment CI: live Postgres, live provider materialization, and any future
host-specific model/tool callback ergonomics.
