# Muzen Context Engine

This directory holds the implementation plan for making the Context Engine a
first-class Muzen primitive.

Current artifact:

- `implementation-plan.md`: detailed design, module shape, rollout phases,
  tests, event/artifact contracts, and review checklist.

The short decision: build one deep core module inside Muzen that is used by
review runs first, then expose standalone CLI, SDK, and HTTP adapters once the
review path proves the interface.

## Implementation Status

Implemented on `feat/context-engine`:

- Core context contracts, config, no-op engine, snapshot engine, in-memory index
  store, pack compiler, query surface, and context tool registrations.
- Review-run integration for context indexing, pack construction, events,
  artifacts, context tool grants, and evidence policy checks.
- Standalone CLI, HTTP workspace routes, runner stdio RPCs, and TypeScript/Python
  local and remote SDK adapters.
- Deterministic snapshot retrieval for text search, exact spans, related tests,
  related symbols/importers, ticket requirements, sufficiency checks, and pack
  explanations.
- Opt-in host/ticket context from provider-neutral instructions and metadata,
  preserving trust labels so untrusted ticket text cannot override kernel
  evidence.
- Feedback records can create inspectable proposed repository/workspace/org
  learnings; approval/rejection and expiry are enforced before
  `history_similar` retrieval returns a learning, with in-memory and JSON-file
  stores available for restart-safe local persistence.
- Workspace HTTP hosts can opt into per-workspace durable learning stores with
  sanitized store paths; `muzen-service` wires this through
  `MUZEN_CONTEXT_LEARNING_STORE_ROOT`.
- Durable context learning stores prune expired records on open so retention is
  enforced in persisted JSON, not only at query time.
- HTTP, runner stdio, and TypeScript/Python SDK context adapters expose feedback
  recording and learning approval flows.
- Cross-repository contract context has a provider-neutral query surface,
  host-provided evidence indexing, and explicit capability omission reporting
  when no cross-repo evidence or network/provider grant is available.
- Provider-materialized cross-repository contract candidates are capability
  scoped by resource id across Rust, HTTP, runner, and SDK adapters; ungranted
  candidates are denied and reported instead of becoming evidence.
- Optional semantic retrieval scaffolding is present: no-vector mode is the
  default, embedding provider/vector-index traits are defined, a deterministic
  local hashed embedding provider, in-memory vector index, local-mode semantic
  index lifecycle, hosted OpenAI-compatible embedding provider, and provider-
  matched search merge are available, semantic score is an additive ranking
  signal, and hosted embedding inputs reject restricted evidence unless
  explicitly allowed.
- Parser-backed Rust, TypeScript/TSX, JavaScript/JSX, and Python definition and
  import extraction feeds a per-index symbol graph used by `related_symbols`,
  including common re-export, alias, nested import, method-definition forms, and
  line ranges on emitted symbol evidence.
- A runnable context-engine evaluation harness with fixture-backed recall,
  precision, token efficiency, omission, latency, secret-redaction, and
  prompt-injection trust summaries, plus expected symbol-range checks and
  local semantic-mode cases driven through the public CLI.
- The context CLI can opt into local semantic indexing with `--local-semantic`
  or hosted semantic indexing with `--hosted-semantic`, and bound vector inputs
  with `--max-embedding-inputs`.

Future hardening:

- Broader parser coverage for more deeply nested module/class shapes.
- Live hosted semantic ranking benchmark results with provider credentials.
- Broader replay/evaluation coverage for sufficiency calibration, cache hit
  rates, hosted semantic ranking, and larger multi-fixture regressions.
