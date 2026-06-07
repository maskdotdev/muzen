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
  learnings; in-memory approval/rejection and expiry are enforced before
  `history_similar` retrieval returns a learning.
- HTTP, runner stdio, and TypeScript/Python SDK context adapters expose feedback
  recording and learning approval flows.
- Cross-repository contract context has a provider-neutral query surface,
  host-provided evidence indexing, and explicit capability omission reporting
  when no cross-repo evidence or network/provider grant is available.
- Fixture and benchmark scaffolding for context-engine evaluation.

Still pending from the full plan:

- Parser-backed symbol/import/export graphs for Rust, TypeScript/TSX, and Python.
- Persistent feedback/learnings across process restarts and durable workspace
  storage.
- Capability-scoped network/provider retrieval for cross-repository contracts.
- Optional embedding providers/vector indexes and semantic ranking.
- Full replay/evaluation suite with precision, recall, latency, redaction, and
  prompt-injection metrics.
