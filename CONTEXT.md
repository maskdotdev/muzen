# Muzen Context

This file defines the project vocabulary used by architecture reviews and
implementation plans.

## Domain Language

- **Muzen**: Rust-first review automation runtime.
- **Review Session**: Durable product unit representing one requested review,
  including source, options, status, events, artifacts, result, retries,
  leases, cancellation, and workspace metadata.
- **Review Source**: User-facing source descriptor for local repositories,
  GitHub pull requests, or GitLab merge requests.
- **Provider Materialization**: Rust runner behavior that resolves provider
  review sources into temporary Git checkouts and changed-file lists.
- **Workspace**: Tenant or project scope that owns model profiles, provider
  profiles, review scheduling, and concurrency policy.
- **Model Profile**: Workspace-owned model routing record that references
  secrets by reference, not raw key material.
- **Provider Profile**: Workspace-owned source-provider routing record that
  references secrets by reference, not raw tokens.
- **Review Worker**: Rust core executor that claims queued review sessions,
  runs them, writes events/results/artifacts, and preserves durable state.
- **Runner Protocol**: Stable JSON-RPC contract between language SDKs and the
  Rust runner binary.
- **Remote HTTP Contract**: Framework-neutral service contract for review
  creation, event replay, SSE, result lookup, artifacts, webhooks, and workspace
  profile APIs.
- **Reviewer Kernel**: Core repository-review execution engine behind the
  runner and service adapters.

## Architecture Quality Target

Architecture work should make modules deeper: small, stable interfaces with
more behavior behind them, better locality for bugs and changes, and clearer
test surfaces.
