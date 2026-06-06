# SDK Memory Benchmarks

These benchmarks exercise the local SDK path: a TypeScript or Python client process
starts `muzen-runner stdio`, submits one deterministic local review with many
agent sessions, and records RSS for the client process, the runner child process,
and their combined footprint.

Run both SDK benchmarks:

```sh
bench/sdk-memory/run-all.sh
```

Run one SDK/language directly:

```sh
cargo build --release --bin muzen-runner
npm --prefix sdk/typescript/packages/muzen-sdk run build

node bench/sdk-memory/typescript-local.mjs \
  --repo . \
  --sessions 100 \
  --runner-path target/release/muzen-runner \
  --output bench/results-sdk-memory/typescript-local-100.json

PYTHONPATH=sdk/python python3 bench/sdk-memory/python_local.py \
  --repo . \
  --sessions 100 \
  --runner-path target/release/muzen-runner \
  --output bench/results-sdk-memory/python-local-100.json
```

The JSON report schema is `muzen.sdk-memory-benchmark.v1`. The default workload
uses deterministic local runner behavior, not a live model provider, so it
measures SDK process overhead plus runner/runtime memory without API cost.

## Live Model Callback Benchmarks

The real-model benchmark uses the runner callback path: the SDK-language process
answers runner `model.complete` callbacks by calling an OpenAI-compatible Chat
Completions endpoint. Defaults are intentionally small because this makes live
LLM calls.

```sh
ENV_FILE=~/.envs/work.zsh MODEL=gpt-4o-mini SESSIONS=2 \
  bench/sdk-memory/run-real.sh
```

The live report uses `mode: local-runner-stdio-real-model-callback` and records
model callback counts/tokens alongside client, runner, and combined RSS. It is a
runner-callback benchmark, not the default deterministic `createMuzen()` path.
