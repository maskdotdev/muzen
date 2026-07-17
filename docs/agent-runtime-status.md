# Agent Runtime — Implementation Status

Status as of 2026-07-16 on `experiment/agent-runtime`. The contract is
`docs/agent-first-interface.md` (approved); this page tracks what is built
against it.

## Built and committed

| Layer | Commits | Notes |
| --- | --- | --- |
| Public contract (Rust/TS/Python) | `f0f57006` | `Muzen` / `AgentSession` / `Run`, swarms as root+child runs |
| Durable state core | `4bee010f`, `cb9610e8` | Memory + SQLite (libsql) adapters behind one `AgentStore` trait; shared conformance suite enforces identical semantics; SQLite survives reopen |
| Execution engine | `7cca282f`, `387c77e3`, `e551f4f6` | Multi-turn agent loop, `run.send` (steer/follow_up with waiting states), `run.spawn`, tool batches with grant/effect/budget enforcement, built-in `agent.spawn`/`agent.message` sharing the command scheduler |
| Credentials + model providers | `3b710a8e` | Ephemeral local secret store behind a CredentialResolver seam; Anthropic messages, OpenAI chat_completions and responses adapters; HTTPS-only egress with explicit loopback opt-in |
| JSON-RPC runner transport | `de1a5b3d` | stdio-jsonl server + Rust client + `muzen-agent-runner` binary |
| HTTP/SSE service transport | `84821030` | All 16 contract routes, SSE events, Range artifacts, Idempotency-Key, bearer auth + `muzen-agent-service` binary |
| Python SDK transports | `4d4a8601` | `connect_local_runner` / `connect_http`, stdlib-only, asyncio |
| TypeScript SDK transports | `fcb5fc84` | `connectLocalRunner` / `connectHttp`, no runtime deps, Node 22 |
| Live-run hardening | `14829e21` | Fixes from the first real-model run (argument normalization, grant-cap semantics, honest budget errors, envelope hygiene, IPv6 loopback) |
| Ergonomic Agent facades | `cc900b95` (Py), `ca90cc5d` (TS) | `Agent(instructions=..., model=..., output=...)` front door; wire spec demoted to escape hatch; env-key model synthesis, session continuity, swarm sugar |
| Structured outputs | `50b86ab8` | OutputContract enforced: strict schema formats on the wire + engine-side validation; parsed JSON in outputs |
| MCP HTTP tools + @tool | `78ef4ab0`, `f3872ec3` | Streamable-HTTP MCP client (schemas to models, tools/call, egress policy); Python `@tool` functions hosted on a per-Agent loopback MCP shim |
| Concurrency fixes | `636dbe10`, `544b81f5` | Two unpolled-waiter deadlocks (select-starved agents → JoinSet; backpressured SSE reserving the fair mutex → SQLite connection actor) found by live/concurrency probing |

## Verification

- Rust: 503 tests passing (91 in agent_runtime, both store backends under the
  shared conformance suite). Python SDK: 44. TypeScript SDK: 57.
- Live-verified against the real OpenAI API through the Python SDK and the
  `muzen-agent-runner` binary: plain completion; a model-driven swarm where a
  gpt-4o-mini root agent spawned a child via the `agent_spawn` tool; and the
  facade trio in one script — custom instructions, a decorated `@tool`
  function executed through the loopback MCP shim, and a TypedDict output
  returned as validated parsed JSON.
- Benchmarked: 5 concurrent agents on one `muzen-agent-service` process
  (SQLite store) complete each wave in 0.37s against a 0.3s-latency model;
  RSS 15MB idle, ~19.9MB after 5 waves (~25KB retained per run, peak equals
  settled).

## Known v1 limitations (deliberate)

- Secrets are process-ephemeral; a reopened runtime fails old refs with
  `secret_unavailable` by design.
- No automatic retries on retryable provider errors (classification only).
- Input-side token limits are delegated to the provider APIs.
- Artifacts and workspaces are contract-defined but not implemented
  (`unsupported`).
- Crash recovery replays durable state, but an interrupted tool batch does
  not resume mid-batch.
- Single-connection SQLite store (behind an always-polled connection actor);
  a shared-database adapter is required before multi-host workers (per the
  Adapter Seams section).

## Remaining roadmap

- TypeScript `tool()` parity (the loopback MCP shim exists in Python only).
- Automatic retries on retryable provider errors.
- Removal of the old review-specific product surface — deferred: that
  surface is under active concurrent development; sequence the removal after
  that workstream lands.
