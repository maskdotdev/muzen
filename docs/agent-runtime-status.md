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

## Verification

- Rust: 484 tests passing (72 in agent_runtime, both store backends under the
  shared conformance suite). Python SDK: 26. TypeScript SDK: 49.
- Live-verified against the real OpenAI API through the Python SDK and the
  `muzen-agent-runner` binary: plain completion, and a model-driven swarm
  where a gpt-4o-mini root agent spawned a child via the `agent_spawn` tool
  and both completed with gap-free event sequences.

## Known v1 limitations (deliberate)

- Secrets are process-ephemeral; a reopened runtime fails old refs with
  `secret_unavailable` by design.
- No automatic retries on retryable provider errors (classification only).
- Input-side token limits are delegated to the provider APIs.
- Artifacts, workspaces, and MCP HTTP tool execution are contract-defined but
  not implemented (`unsupported`).
- Crash recovery replays durable state, but an interrupted tool batch does
  not resume mid-batch.
- Single-connection SQLite store; a shared-database adapter is required
  before multi-host workers (per the Adapter Seams section).

## Remaining roadmap

- Removal of the old review-specific product surface — deferred: that
  surface is under active concurrent development; sequence the removal after
  that workstream lands.
