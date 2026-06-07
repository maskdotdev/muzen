# Context Engine Benchmarks

This directory contains deterministic Context Engine retrieval evaluation.

The benchmark target is not end-to-end review quality. It is whether the
Context Engine retrieves the evidence needed for a review session before a
model reasons over it.

Initial metrics to track:

- Context recall: required evidence found for seeded changes.
- Context precision: selected evidence that is actually relevant.
- Evidence coverage: findings with primary evidence.
- Token efficiency: selected useful evidence per 1k estimated tokens.
- Latency: index, query, and pack build time.
- Secret redaction correctness: known secret-like strings are not emitted in
  context query payloads.
- Prompt-injection resistance: hostile repository guidance remains structurally
  untrusted evidence.
- Symbol range coverage: symbol evidence includes expected line spans.
- Optional local semantic retrieval: cases can opt into the local hashed vector
  index through the public CLI.

Run the evaluation suite:

```sh
python3 bench/context-engine/run.py
```

The runner drives `cargo run --bin muzen -- context query` or
`cargo run --bin muzen -- context pack` for each case in
`bench/context-engine/cases/`, computes recall, precision, token efficiency,
omissions, redaction correctness, prompt-injection trust checks, expected range
coverage, and latency, then writes
`bench/results-context-engine/context-engine-summary.json`.

Case files may set `localSemantic: true` and `maxEmbeddingInputs` to exercise
local semantic indexing. They may also set `expectedRanges` entries with `path`,
`startLine`, `endLine`, and optional `kind`; the run fails when no returned
evidence item matches each expected range.

Local smoke commands:

```sh
cargo run --bin muzen -- context index \
  --repo fixtures/context-engine/simple-auth \
  --changed-file src/auth/token.rs

cargo run --bin muzen -- context pack \
  --repo fixtures/context-engine/simple-auth \
  --changed-file src/auth/token.rs \
  --purpose security

cargo run --bin muzen -- context query \
  --repo fixtures/context-engine/simple-auth \
  --changed-file src/auth/token.rs \
  --kind related-tests \
  --path src/auth/token.rs
```
