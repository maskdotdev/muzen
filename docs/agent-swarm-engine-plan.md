# Agent Swarm Engine Plan

Status: proposed (2026-06-12)

## Goal

Make Muzen a high-performance **agent swarm engine**: spawn many agents
concurrently, give each agent tools, let users bring their own model
(base URL + model name + key) per agent, with extreme efficiency. Code
review becomes one adapter built on the swarm core, not the core itself.

## Where we are (audit summary, 2026-06-12)

Verified against `main` (13cb4ad), `demo`, and `feat/context-engine`:

- **Execution is sequential.** Shards run one at a time
  (`src/reviewer/run.rs:221`) and review units run one at a time
  (`src/runtime/planned_units.rs:72`). Only intra-turn tool batches are
  concurrent (`src/runtime/tools/engine.rs:155`).
- **A real swarm scheduler existed and was deleted.** Commit `9f56abe`
  (2026-06-06) removed `src/runtime/job_runtime.rs` (JoinSet + semaphore
  bounded by `max_active_sessions`, child cancellation per session) and
  `src/runtime/session_loop.rs` (generic per-session runner), replacing
  them with the sequential planned-units pipeline. Benchmarked speedups
  before removal: 11x at 50 sessions, 20.9x at 100
  (`docs/reviewer-primitive-assessment.md:27`). Recover with:
  `git show 9f56abe^:src/runtime/job_runtime.rs` and
  `git show 9f56abe^:src/runtime/session_loop.rs`.
- **One provider is wired.** Runtime `ProviderKind` has only
  `OpenaiCompatible` (`src/contracts.rs:130`). Anthropic exists in the
  profiles layer enum but is unreachable. All profiles in a run must
  share one base URL (`src/runner/wiring.rs:170`). SDKs expose only an
  `openai()` model factory.
- **No retries, no streaming.** 429s are marked retryable but the loop
  breaks on any model error; `model.complete()` awaits full responses.
- **Strong foundations to keep.** Tool system (custom tool traits,
  JSON-RPC tools, SDK `tool.execute` callbacks, moka caching,
  singleflight dedup, capability authorization), hierarchical
  `ModelLimiter` (global/provider/profile/key/session semaphores),
  cancellation tokens everywhere, lock-free metrics, durable session
  store + leases, generic runner protocol and thin SDKs.

## Design stance

- Sessions (agents) are the unit of execution. Review-unit planning and
  findings synthesis become one consumer of the swarm scheduler.
- Keep the secret-reference discipline (`env:`, `secret:`); no inline
  keys in SDK payloads.
- Resurrect and modernize the deleted executor rather than rebuilding.
- No distributed multi-node scheduling in this plan; single-process
  swarms first, proven by benchmarks.

## Phases

### Phase 0 — Baseline and guardrails (small)

1. Restore the concurrent-vs-sync benchmark harness that produced the
   `results-concurrent-compare` table (results dir was never committed);
   wire it to the deterministic mock model so it runs in CI without API
   cost.
2. Record baselines on `main`: sessions/sec, wall time, peak RSS, tool
   cache hit rates at 10/50/100 sessions.
3. Decide coordination with `feat/context-engine` (it rewrites ~1,700
   lines of `planned_units.rs`). Recommendation: land or rebase that
   branch first; the scheduler work below touches the same file.

Exit: benchmark harness runs in CI; baseline numbers committed under
`bench/`.

### Phase 1 — Resurrect the swarm scheduler (core)

1. Port `JobRuntime::run_sessions_with_cancel` (JoinSet + Semaphore on
   `max_active_sessions`, child cancel tokens) and `SessionRunner` from
   `9f56abe^` onto current contracts (event dispatcher, policy, runtime
   contracts drifted in the planned-units commit — this is a port, not a
   cherry-pick; port the old tests too).
2. Make `PlannedReviewRuntime` a consumer: unit planning produces
   session scopes, the scheduler runs them concurrently, synthesis
   remains a barrier after all units complete. Make synthesis input
   ordering deterministic (sort by unit id) so results don't depend on
   completion order.
3. Run shards through the same scheduler (cross-shard concurrency) or
   document why shards stay sequential.
4. Verify under concurrency: metrics accuracy, event ordering/context,
   artifact-store merging, cancellation propagation mid-flight.

Exit: concurrent-compare bench shows ≥10x at 50 mock sessions; full test
suite green; cancellation test kills a 50-session swarm promptly.

### Phase 2 — Generic agent primitive (sessions-first API)

1. Add a run mode that executes user-supplied sessions directly —
   no `build_review_unit_plan`, no evidence obligations, no findings
   parsing. Input: session scopes (objective, instructions, tool grants,
   budget, model profile). Output: per-session report (transcript,
   structured result as opaque JSON, token/tool metrics).
2. Findings/evidence/synthesis move behind the review adapter: review
   runs request the review policy layer; swarm runs skip it.
3. Expose through the existing runner protocol — `RunSessionParams` is
   already generic; add the mode flag and per-session result retrieval.
4. SDK surface: `runSwarm`/`run_swarm` (or equivalent) in TypeScript and
   Python taking agents + tools + models; review API unchanged on top.

Exit: a Rust example and one SDK example spawn 20 heterogeneous agents
with custom tools and collect structured results, no review concepts
involved.

### Phase 3 — BYO providers (models, base URLs, keys)

1. Per-profile base URLs: drop the single-base-URL-per-run constraint
   (`src/runner/wiring.rs:170`); each model profile carries its own
   endpoint. Smallest, highest-leverage change — unlocks vLLM/Ollama/
   proxy mixes with the existing OpenAI-compatible client.
2. Native Anthropic Messages client behind `ConcurrentModelClient`; add
   `ProviderKind::Anthropic` and wire profile-layer Anthropic through.
   Hard error (not silent fallback) for any unwired provider.
3. SDK model factories: `anthropic()`, `openaiCompatible({ baseUrl,
   model, credential })`; keep credential = `{env}` | `{secretRef}`.
4. Test matrix: one run, three agents, three providers/endpoints;
   per-key limiter semaphores verified under load.
5. Stretch: fallback chains (profile A → profile B on retryable
   exhaustion) and cheap/expensive tier hints in routing metadata.

Exit: one swarm mixes Anthropic + hosted OpenAI-compatible + local
endpoint, keys via env refs, all from a single SDK call.

### Phase 4 — Model-loop efficiency and resilience

1. Retries: exponential backoff + jitter on retryable errors (429/5xx/
   timeouts), capped per turn and per budget; surface retry counts in
   metrics. Today any model error silently kills the session
   (`src/runtime/planned_units.rs:292`).
2. Streaming: SSE for chat completions; stream tool-call deltas so tool
   batches can start as soon as calls are complete; enables early
   cancellation and cuts time-to-first-token.
3. Transcript management: enforce `max_prompt_tokens` mid-loop
   (truncate oldest tool results first, keep system + objective stable);
   keep a stable message prefix for provider prompt caching (Anthropic
   `cache_control`, OpenAI automatic caching); stop re-serializing the
   full transcript every turn (incremental message assembly).

Exit: bench shows reduced per-turn overhead at 7-turn depth; chaos test
(injected 429s/timeouts) completes a 50-session swarm with retries
instead of dropped sessions.

### Phase 5 — Packaging and DX

1. Module re-exports first: public `muzen::swarm` facade (Run, RunSpec,
   session specs, model router, tool registry) independent of review
   naming. Crate split (`muzen-core` / `muzen-review`) only after the
   API settles — renames are cheap, published crates are not.
2. Quickstarts: Rust, TypeScript, Python swarm examples under
   `examples/`; document the tool callback protocol for SDK-side tools.
3. Update CONTEXT.md vocabulary: swarm engine terms (Agent Session,
   Swarm Run, Model Profile) with review terms scoped to the adapter.

### Phase 6 — Proof at scale

1. Bench ladder: 100/500 mock sessions (wall time, RSS, limiter
   contention); real-provider canary at small N.
2. Backpressure validation: saturate per-key and per-provider semaphores
   and confirm queueing, not collapse; measure DashMap/semaphore
   contention at high N.
3. Publish results under `bench/` (commit them this time) and add a CI
   regression gate on the mock-model swarm bench.

## Risks

- **`feat/context-engine` collision** — biggest sequencing risk; both
  rewrite `planned_units.rs`. Decide ordering in Phase 0.
- **Port drift** — the old executor predates current event/policy
  contracts; mitigate by porting its tests and running the full suite
  plus benches behind a feature flag until parity.
- **Review-quality regression** — concurrency changes finding order;
  deterministic synthesis input ordering plus the existing
  review-quality bench (`bench/review-quality/`) guard this.
- **Provider matrix creep** — keep Phase 3 to OpenAI-compatible +
  Anthropic; everything else rides the OpenAI-compatible client via
  per-profile base URLs.

## Sequencing

Phases 0–1 are the unlock and should land together (scheduler without
benchmarks is unverifiable; benchmarks without the scheduler are
baseline-only). Phase 3.1 (per-profile base URLs) is small enough to
land alongside Phase 1. Phases 2 and 3.2+ follow in either order;
Phase 4 builds on both. Phases 5–6 trail continuously.
