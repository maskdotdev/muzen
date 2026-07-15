# Muzen Context

This file defines the project vocabulary used by architecture reviews and
implementation plans.

## Domain Language

- **Muzen**: Rust-first agent runtime and central swarm host, connected through
  an embedded, local-runner, or remote Adapter.
- **Agent Definition**: Immutable value describing one Agent's instructions,
  model reference, Tool Grants, budgets, output contract, and metadata.
- **Agent Session**: Durable Agent identity, transcript, configuration, and
  private copy-on-write Workspace overlay. It may be active in at most one Run.
- **Run**: Bounded execution tree containing one or more root Agent Sessions
  and every tracked child Agent Session. A swarm is a Run with multiple roots
  or children; it is not a separate public type.
- **Agent Path**: Durable ordered child-ordinal path used for deterministic
  Run tree and result ordering. IDs are opaque and never parsed for hierarchy.
- **Model Profile**: Session-local provider, protocol, model, endpoint, token
  limits, and Secret Reference used for model execution.
- **Secret Reference**: Opaque handle to credential material stored behind the
  Credential Resolver Seam. Session and Run records never contain raw secrets.
- **Tool Provider**: Built-in or MCP HTTP Implementation that supplies tools.
- **Tool Grant**: Explicit tool authority, effects, and call limit available to
  an Agent. Child authority can only be reduced through intersection.
- **Workspace**: Immutable content-addressed base plus one private
  copy-on-write overlay per Agent Session. Child Sessions receive point-in-time
  overlay forks; changes leave Muzen only as explicit artifacts.
- **Agent Event**: Durable, versioned Run observation with one Run-scoped,
  gap-free sequence used for replay and live tailing.
- **Runner Protocol**: Stable JSON-RPC Adapter contract between language SDKs
  and the Rust local runner.
- **Remote HTTP Contract**: HTTP/SSE Adapter contract with the same lifecycle,
  durability, event, cancellation, result, secret, and artifact semantics as
  the Runner Protocol.
- **Agent Loop**: Private Implementation of one bounded model/tool conversation:
  transcript preparation, prompt budgeting, model turns, tool batches,
  accounting, cancellation, and terminal Agent status.
- **Context Engine**: Private evidence-compilation Implementation that turns
  Workspace state, guidance, metadata, tool results, and feedback into ranked,
  permission-aware context packs for an Agent Loop.
- **Context Evidence**: Typed, provenance-carrying evidence such as a file
  span, rule, issue, historical change, or tool output, with trust,
  sensitivity, source, and content-hash metadata.
- **Context Graph**: Deterministic, bounded graph of relationships between
  Workspace artifacts, symbols, tests, configuration, and contracts.
- **Context Pack**: Bounded compiled artifact containing selected evidence,
  relationships, omissions, budget usage, and sufficiency assessment.
## Architecture Quality Target

Architecture work should make modules deeper: small, stable interfaces with
more behavior behind them, better locality for bugs and changes, and clearer
test surfaces.
