# Runner Stdio Robustness Findings

Status: open (found 2026-06-12 while verifying the agent swarm engine
branch; both behaviors predate that branch — the stdio framing code in
`src/runner/protocol.rs`, `src/runner/session.rs`, and
`src/runner/transport.rs` was untouched by it).

Both were observed by driving `muzen-runner stdio` directly with
newline-delimited JSON-RPC, the same way the SDKs do.

## 1. A malformed line kills the stdio session silently

Repro: send a non-JSON line (e.g. `{"this is not json`) followed by a
valid `runner.handshake` request.

Observed: the runner produces no output at all — no JSON-RPC `-32700`
parse-error response, no stderr diagnostic, and the subsequent valid
handshake never gets a response. The process appears alive but the
session is dead.

Impact: any SDK bug, log line, or shell banner that contaminates stdout
of the host process takes down the whole run with zero diagnostics.
This class of bug is painful to debug from the SDK side because there
is nothing to observe.

Suggested fix: respond to unparseable lines with a JSON-RPC error
(`-32700 Parse error`, id null) and continue reading the stream, or at
minimum write a structured diagnostic to stderr before bailing.

## 2. Stdin EOF abandons in-flight runs without any terminal frame

Repro: write `runner.handshake` + `run.start` to stdin, then close
stdin immediately (a natural one-shot client pattern:
`echo "$requests" | muzen-runner stdio`).

Observed: the runner exits when stdin closes, abandoning the in-flight
run. The client receives no `run.start` response, no `run.failed`
notification, and no events — the run simply vanishes. A client must
know to hold stdin open until the response arrives, which is not
documented anywhere (including the handshake fixtures).

Impact: one-shot integrations silently lose runs; SDK authors discover
the hold-stdin-open requirement by trial and error.

Suggested fix (either or both):

- Drain in-flight runs before exiting on EOF: stop accepting new
  requests, finish (or cancel-with-`run.failed`) what is running, emit
  the terminal frames, then exit.
- Document the contract: stdin must remain open for the lifetime of
  the session; closing it is a hard shutdown.

## Related observation

Against a connection-refused provider endpoint the runner emits
`modelFailed { attempt: 1, retrying: false }` and ends the session
"partial" on the first error — live confirmation of the no-retry gap
tracked as Phase 4 of `docs/agent-swarm-engine-plan.md`.
