# Muzen Source Architecture

Muzen source is organized around product concepts, not implementation buckets.

Top-level `src/` modules:

- `review_sessions`: durable lifecycle, store, worker, Project profiles, retries, leases, webhooks, and session outcome persistence.
- `review_sources`: source descriptors and provider materialization into local review state.
- `workspace`: local materialized review state: checkout, changed files, diffs, snapshots, file inventory, and file classification.
- `context_engine`: Context Graph, Context Evidence, and Context Pack construction from workspace state.
- `reviewer_kernel`: autonomous review loop, model clients, tools, policy, transcript, artifacts, semantic reports, and canary evidence formats.
- `runner_protocol`: JSON-RPC contract, schema, stdio transport, and protocol dispatch.
- `remote_http`: HTTP request/response contract, routes, SSE/event replay mapping, and Axum adapter.
- `canary`: named operational proof flows.
- `cli`: command-line adapter only.

Forbidden top-level buckets are `runtime`, `runner`, `reviewer`, `review_session`, `diagnostics`, `contracts.rs`, `repo.rs`, `util.rs`, and `service.rs`.

Dependency direction:

- `runner_protocol` translates JSON-RPC into product module calls.
- `remote_http` translates HTTP into product module calls.
- `review_sessions` owns durable orchestration and delegates source materialization to `review_sources`.
- `review_sources` produces workspace state; it does not own review lifecycle.
- `workspace` has no knowledge of HTTP, JSON-RPC, profile stores, or worker scheduling.
- `context_engine` reads workspace state and emits context artifacts.
- `reviewer_kernel` executes the review loop; callers do not reach into its private support modules.
