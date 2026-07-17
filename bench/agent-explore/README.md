# Exploring-agent facade benchmark

This benchmark measures agents that genuinely explore a repository through a multi-turn `list → read × N → grep → summarize` loop. It runs the public Python, TypeScript, and Rust `Agent` facades on both supported deployment shapes:

- `local`: facade-managed loopback MCP tools, with the Python and TypeScript facades using `muzen-agent-runner` and the Rust facade using its in-process runtime.
- `http`: client-executed facade tools through one shared `muzen-agent-service` process. The default five concurrent agents preserve the concurrency class that previously exposed runtime deadlocks.

The benchmark deliberately uses the facades instead of a hand-rolled MCP client. That keeps session creation, provider requests, tool grants, loopback MCP, HTTP client-tool events, run waiting, and cleanup on the same APIs users call.

## Prerequisites

Build the release binaries and TypeScript SDK first. Node 22 or newer is required.

```sh
cargo build --release
npm --prefix sdk/typescript/packages/muzen-sdk run build
```

The orchestrator uses `python3` with `PYTHONPATH=sdk/python`. Override the interpreter with `PYTHON=/path/to/python` if needed. It locates `muzen-agent-service` and `muzen-agent-runner` through `MUZEN_AGENT_SERVICE_BIN` and `MUZEN_AGENT_RUNNER_BIN`, then falls back to `target/release`. Debug binaries are rejected unless `--allow-debug` is explicit.

For this checkout, Python can also be run with `/Users/mask/.local/bin/uv run --python 3.14`; the orchestrated acceptance path uses plain `python3` plus `PYTHONPATH`.

## Run the matrix

```sh
PATH=/Users/mask/.nvm/versions/node/v24.12.0/bin:$PATH \
  node bench/agent-explore/run.mjs --waves 2 --concurrency 3
```

The permanent deadlock-class probe can be isolated to one HTTP cell:

```sh
PATH=/Users/mask/.nvm/versions/node/v24.12.0/bin:$PATH \
  node bench/agent-explore/run.mjs \
  --languages rust --transports http --concurrency 5 --waves 3 --store sqlite
```

Useful flags are `--languages python,ts,rust`, `--transports local,http`, `--repo-root`, `--latency-ms`, `--read-files`, `--store sqlite|memory`, `--runstamp`, and `--output`. Results default to `bench/results-agent-explore/<runstamp>.json`.

## Run layers independently

Start the deterministic OpenAI-compatible model server:

```sh
node bench/agent-explore/mock-model-server.mjs --port 8088 --latency-ms 300 --read-files 5
```

Then run one local facade driver against it:

```sh
PYTHONPATH=sdk/python MUZEN_AGENT_RUNNER_BIN=target/release/muzen-agent-runner \
  python3 bench/agent-explore/driver.py \
  --transport local_runner --root "$PWD" --model-base-url http://127.0.0.1:8088/v1 --read-files 5

MUZEN_AGENT_RUNNER_BIN=target/release/muzen-agent-runner \
  node bench/agent-explore/driver.mjs \
  --transport local_runner --root "$PWD" --model-base-url http://127.0.0.1:8088/v1 --read-files 5

target/release/muzen-agent-explore-bench \
  --transport local --root "$PWD" --model-base-url http://127.0.0.1:8088/v1 --read-files 5
```

For HTTP, start one service and change each driver to `--transport http --base-url http://127.0.0.1:8090`:

```sh
target/release/muzen-agent-service \
  --listen 127.0.0.1:8090 --store sqlite --db /tmp/muzen-agent-explore.sqlite --allow-loopback-http
```

Each driver prints exactly one JSON line with `turns`, `toolCalls`, `durationMs`, and `summaryText`. It exits non-zero unless it observes exactly one list, the configured number of reads, one grep, and a non-empty terminal summary.

## Report schema and interpretation

Reports use schema id `muzen.agent-explore-benchmark.v2`. Top-level `config` records the workload, `binaries` records the exact executables, and each `cells[]` entry contains per-wave driver results and timing/RSS samples.

RSS sampling uses the benchmark Rust binary's native macOS/Linux process sampler, so it does not depend on shelling out to `ps`.

- `servicePeakRssBytes` is the highest runtime-side RSS during the wave: the shared service for HTTP, runner processes for Python/TypeScript local, and the in-process Rust driver for Rust local.
- `serviceSettledRssBytes` is sampled after the wave has completed. Local processes have exited, so settled local RSS is normally zero.
- `retainedPerRunBytes` is `(final settled - initial settled) / total runs` for the persistent HTTP service. Small negative values are possible because RSS is sampled, not an allocator accounting ledger.
- `theoreticalSequentialMs` follows the probe's requested `(readFiles + 2) × latencyMs × concurrency` estimate for list, individual reads, and grep. `modelTurnsPerRun` is one higher because it also records the terminal summary turn.
- `speedup` compares that sequential estimate with observed wave wall time. Near-concurrency speedup shows requests overlap; a collapse toward `1x` exposes serialization.

Any non-zero driver, malformed JSON line, count mismatch, or timeout fails the full run. A timeout is reported as a `HANG FINDING`; the orchestrator does not retry it and terminates every child process on exit.
