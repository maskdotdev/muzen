# Muzen Context

This file defines the project vocabulary used by architecture reviews and
implementation plans.

## Domain Language

- **Muzen**: Rust-first review automation runtime.
- **Review Session**: Durable product unit representing one requested review,
  including source, options, status, events, artifacts, result, retries,
  leases, cancellation, and project metadata.
- **Review Source**: User-facing source descriptor for local repositories,
  GitHub pull requests, or GitLab merge requests.
- **Provider Materialization**: Review Source behavior that resolves provider
  review sources into Workspace checkouts and changed-file lists.
- **Project**: Tenant or customer scope that owns model profiles, provider
  profiles, review scheduling, and concurrency policy.
- **Workspace**: Local materialized review state: checkout, changed files,
  diffs, snapshots, file inventory, and file classification.
- **Model Profile**: Project-owned model routing record that references
  secrets by reference, not raw key material.
- **Provider Profile**: Project-owned source-provider routing record that
  references secrets by reference, not raw tokens.
- **Review Worker**: Rust core executor that claims queued review sessions,
  runs them, writes events/results/artifacts, and preserves durable state.
- **Runner Protocol**: Stable JSON-RPC contract between language SDKs and the
  Rust runner binary.
- **Remote HTTP Contract**: Framework-neutral service contract for review
  creation, event replay, SSE, result lookup, artifacts, webhooks, and project
  profile APIs.
- **Reviewer Kernel**: Core repository-review execution engine behind the
  runner and service adapters.
- **Agent Loop**: Private Reviewer Kernel module that runs one bounded
  model/tool conversation over a SessionScope: transcript preparation, prompt
  budget enforcement, model turns, tool batches, tool-result transcript
  effects, accounting, cancellation, and terminal loop status. It is reusable
  implementation, not a public product interface.
- **Chat Session**: Future durable product unit for conversational repository,
  issue, and workspace interaction. It owns chat-specific message history,
  attachments, streaming events, continuation, and result semantics, and may
  reuse Agent Loop internally. It is not the same thing as a direct execution
  mode for Review Sessions.
- **Context Engine**: Core evidence-compilation module that turns Workspace
  state, changed-file manifests, repository guidance, host metadata, tool
  results, and feedback into ranked, cited, permission-aware context packs and
  context query results.
- **Context Evidence**: Typed, provenance-carrying review evidence such as a
  diff hunk, file span, rule, test, host issue, historical finding, or tool
  output. Evidence records carry trust, sensitivity, source, and content hash
  metadata.
- **Context Graph**: Deterministic, bounded, explainable graph of
  review-relevant relationships between repository artifacts. It connects
  files, chunks, symbols, tests, configuration, contracts, and other context
  nodes with typed, weighted, provenance-carrying edges. It is not a perfect
  semantic model of program behavior.
- **Context Pack**: Durable, session-specific compiled artifact containing the
  evidence selected for a review purpose, the evidence relationships, the
  omitted candidates, budget usage, and a sufficiency assessment.

## Architecture Quality Target

Architecture work should make modules deeper: small, stable interfaces with
more behavior behind them, better locality for bugs and changes, and clearer
test surfaces.
