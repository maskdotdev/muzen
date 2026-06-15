# RFC 0002: Agent Loop and Single Review Path

## Status

Accepted.

## Context

Muzen previously had a second generic session execution experiment beside the
autonomous review product path. That experiment added useful implementation
proof, but it was not a product interface. It also duplicated loop mechanics
now present in the autonomous review path: transcript setup, prompt-budget
enforcement, model turns, tool batches, tool-result transcript effects,
accounting, cancellation, and terminal session status.

Muzen is unreleased, so this decision does not preserve compatibility for
obsolete execution modes, runner fields, aliases, or migration shims.

## Decision

Review execution has one product path.

The reusable implementation will be a private `reviewer_kernel::agent_loop`
Module. Its Interface is internal to the Reviewer Kernel and represents one
bounded model/tool conversation over a `SessionScope`.

The retired generic session experiment is not the future chat product
Interface. A future Chat Session must be its own durable product unit with
chat-specific message history, attachments, streaming events, continuation
semantics, and result contract. Chat may reuse Agent Loop internally, but it
must not depend on review-session runner outputs.

## Implementation Order

1. Extract `reviewer_kernel::agent_loop` from the autonomous review loop,
   pulling only reusable mechanics from the direct-session implementation.
2. Rewire `AutonomousReviewRuntime` to use Agent Loop while keeping review
   policy local to autonomous review.
3. Delete the public direct-session execution mode end to end.
4. Collapse runner/review contract fields made redundant by the deletion.
5. Reassess broad Rust adapter surfaces, proof tooling, and tool-call
   tolerance once the main runtime path is singular.

## Agent Loop Owns

- transcript initialization and append mechanics
- prompt-budget enforcement
- model turn execution and retry integration
- tool batch execution
- tool-result effect application
- token, model, and tool accounting
- cancellation checks
- generic final-turn forcing mechanics
- terminal loop status

## Autonomous Review Owns

- orchestrator and delegate session roles
- review-specific response formats
- final review instruction text
- output validation and repair policy
- finding and file-review synthesis
- mandatory validation runs
- publication gates
- Review Session, Runner Protocol, and Review Result mapping

## Consequences

The Agent Loop becomes a deeper Module: callers get reusable model/tool loop
behavior through a small private Interface, while review-specific decisions
retain locality in Autonomous Review.

Deleting the generic session experiment removes a shallow public execution
seam. It also removes runner protocol mode mapping, per-session text outputs,
mode-specific tests, and demo coupling that exist only to support the obsolete
path.
