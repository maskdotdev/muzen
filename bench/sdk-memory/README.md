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

## Shared Runner Callback Benchmarks

Use the shared-runner benchmark to model a production-style long-lived runner:
one `muzen-runner stdio` process stays alive while several `run.start` requests
execute concurrently over the same JSON-RPC connection.

Create a workload JSON array:

```json
[
  {
    "id": "cal-pr-8330",
    "repo": "/tmp/cal-pr-8330-worktree",
    "baseRef": "aci-martian/pr-8330-base",
    "sessions": 1,
    "changedFiles": [
      "apps/web/test/lib/getSchedule.test.ts",
      "packages/lib/slots.ts"
    ]
  },
  {
    "id": "cal-pr-11059",
    "repo": "/tmp/cal-pr-11059-worktree",
    "baseRef": "aci-martian/pr-11059-base",
    "sessions": 11,
    "changedFiles": ["packages/app-store/_utils/oauth/refreshOAuthTokens.ts"]
  }
]
```

Run it:

```sh
source ~/.zshrc
cargo build --release --bin muzen-runner

node bench/sdk-memory/typescript-shared-runner-callback.mjs \
  --workloads /tmp/muzen-shared-workloads.json \
  --runner-path target/release/muzen-runner \
  --model gpt-5.4-mini \
  --api-key-env OPENAI_API_KEY \
  --output bench/results-sdk-memory-real/shared-runner.json
```

The report schema is `muzen.shared-runner-benchmark.v1`. Key memory fields:

- `memory.peakRunnerRssBytes`: RSS for the single shared `muzen-runner`.
- `memory.peakClientRssBytes`: RSS for the Node benchmark/model-callback host.
- `memory.peakCombinedRssBytes`: client plus shared runner RSS.

This benchmark is the right comparison point for production-style concurrency.
The older PR callback script starts one runner per invocation, so concurrent
shell invocations have additive runner memory.
