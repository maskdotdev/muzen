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

When `--muzen-bin` is omitted, the runner builds `muzen` once with
`cargo build --bin muzen` and reuses `target/debug/muzen` for every case.

For iteration, build once and run focused diagnostic cases:

```sh
cargo build --bin muzen
python3 bench/context-engine/run.py \
  --muzen-bin target/debug/muzen \
  --jobs 6 \
  --case-id mined-1ca728244ab4-pack \
  --output /tmp/context-case.json

python3 bench/context-engine/run.py \
  --muzen-bin target/debug/muzen \
  --jobs 6 \
  --case-glob '*-pack' \
  --output /tmp/context-pack-cases.json
```

Filtered runs are diagnostic only: they skip the committed regression gate and
cannot write `baseline.json`. Use the full unfiltered suite before accepting
or committing quality changes.

Compare two summary artifacts after an experiment:

```sh
python3 bench/context-engine/compare.py \
  bench/results-context-engine/context-engine-summary.json \
  /tmp/context-engine-summary-candidate.json
```

Use artifacts with the same case selection for summary-level metric deltas.
When selections differ, the case-delta rows are still useful for diagnostics.

The runner drives the public `muzen context query` or `muzen context pack` CLI
for each case in `bench/context-engine/cases/`, computes recall, precision,
token efficiency, omissions, redaction correctness, prompt-injection trust
checks, expected range coverage, and latency, then writes
`bench/results-context-engine/context-engine-summary.json`.
Weak-case diagnostics include candidate-present missed omissions and the
selected tail candidates with score, rank index, representation, and token
estimate, so pack-selection tradeoffs can be inspected directly.

Case files may set `localSemantic: true` or `hostedSemantic: true` and
`maxEmbeddingInputs` to exercise semantic indexing. Hosted cases can also set
`hostedEmbeddingBaseUrl`, `hostedEmbeddingModel`, and
`hostedEmbeddingCredentialRef`; the deterministic default suite avoids live
provider calls. Cases may also set `expectedRanges` entries with `path`,
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

cargo run --bin muzen -- context query \
  --repo fixtures/context-engine/simple-auth \
  --changed-file src/auth/token.rs \
  --local-semantic \
  --kind search-text \
  --query "user id token"
```
