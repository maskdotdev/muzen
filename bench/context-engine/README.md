# Context Engine Benchmarks

This directory is the starting point for Context Engine retrieval evaluation.

The benchmark target is not end-to-end review quality. It is whether the
Context Engine retrieves the evidence needed for a review session before a
model reasons over it.

Initial metrics to track:

- Context recall: required evidence found for seeded changes.
- Context precision: selected evidence that is actually relevant.
- Evidence coverage: findings with primary evidence.
- Token efficiency: selected useful evidence per 1k estimated tokens.
- Latency: index, query, and pack build time.

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
