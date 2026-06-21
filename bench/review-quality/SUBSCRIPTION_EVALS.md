# Subscription-Backed Review Evals

Use this loop when running live Muzen review-quality evals from this repo. The
goal is to test Muzen agent behavior with the owner's ChatGPT subscription via
the local Codex ChatGPT Responses proxy, while keeping that auth path out of
Muzen core product code.

## Setup

Build the runner:

```sh
cargo build --release --bin muzen-runner
```

Run the cheap local fake-model harness gate before live model calls:

```sh
node bench/review-quality/check-local.mjs \
  --runner-path target/release/muzen-runner
```

Before any subscription-backed runner-mode diagnostic, also run the proxy-shaped
fake gate. It starts the local Codex Responses proxy with a temporary fake auth
file, points that proxy at the fake Responses server, and verifies deterministic
retry behavior through the same `OPENAI_BASE_URL` path used by live evals:

```sh
node bench/review-quality/check-local.mjs \
  --runner-path target/release/muzen-runner \
  --include-codex-proxy true \
  --include-protocol-pressure true
```

The protocol pressure portion stays fake/local. It repeatedly exercises the
explicit protocol direct-session path under mixed heartbeat, status polling,
explicit cancellation, callback-tool budget exhaustion, and large synthetic
fixture pressure.

For broader fake runner-mode sweeps, `run-fake-runner-mode-sweep.mjs` exits
nonzero by default when parity, release, or isolation regressions appear. Keep
that default for pre-live gates. Use `--fail-on-regression false` only for
intentional chaos probes where the JSON report is the artifact to inspect.

Start the local proxy:

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs serve \
  --port 4141 \
  --reasoning-effort low
```

To run against a specific CodexBar-managed ChatGPT account, list accounts and
select one when starting the proxy:

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs accounts

node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs serve \
  --port 4141 \
  --reasoning-effort low \
  --codexbar-account maskdotdev@gmail.com
```

In the eval shell:

```sh
export OPENAI_BASE_URL=http://127.0.0.1:4141/v1
export OPENAI_API_KEY=muzen-codex-proxy
export MODEL=gpt-5.5
export ANTI_CHEAT_MODEL=gpt-5.4-mini
```

Use `MODEL` for recall-bearing positive runs and real PR regressions. Use
`ANTI_CHEAT_MODEL` for frequent live anti-cheat precision smoke runs so safe
controls stay cheap. A clean mini anti-cheat run is a tripwire, not final proof:
before calling a change promoted or merge-ready, rerun anti-cheat once with the
same target `MODEL` used for the positive eval.

If auth is missing or expired, run:

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs login
```

The proxy auth cache is local and ignored by git:
`experiments/codex-chatgpt-proxy/.auth.json`.

## Fast Smoke

Cheap live anti-cheat control:

```sh
node bench/review-quality/run-anti-cheat.mjs \
  --runner-path target/release/muzen-runner \
  --model "$ANTI_CHEAT_MODEL" \
  --output-dir bench/results-review-quality/gpt54mini-low-smoke-clean
```

Positive autonomous-review check:

```sh
node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com \
  --pr 14943 \
  --sessions 0 \
  --runner-path target/release/muzen-runner \
  --golden bench/review-quality/goldens/cal-pr-14943.json \
  --output bench/results-review-quality/gpt55-low-cal-pr-14943.json
```

Full current scored suite:

```sh
node bench/review-quality/run-martian-suite.mjs \
  --sessions 0 \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-martian-suite
```

Direct sessions are for protocol/session-output diagnostics. They return raw
`sessionOutputs` and completion diagnostics, but they do not publish scored
review findings.

## Official Martian Offline Judge

Use this when the review artifacts already exist in a local
`code-review-benchmark` checkout and the goal is to run official Martian step 3
without direct API billing. The official judge uses Chat Completions and reads
`MARTIAN_API_KEY`, `MARTIAN_BASE_URL`, and `MARTIAN_MODEL`; the wrapper starts a
temporary Chat Completions bridge that forwards those calls to the local Codex
ChatGPT Responses proxy.

Keep the proxy running:

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs serve \
  --port 4141 \
  --reasoning-effort low
```

Then run official step 3 for any tool that already exists in the benchmark
`benchmark_data.json` plus model-scoped `candidates.json`. For example, hagent:

```sh
OPENAI_BASE_URL=http://127.0.0.1:4141/v1 \
OPENAI_API_KEY=muzen-codex-proxy \
MARTIAN_JUDGE_MODEL=gpt-5.4-mini \
node bench/review-quality/tools/run-martian-official-step3.mjs \
  --benchmark-root /tmp/code-review-benchmark \
  --tool hagent \
  --force true
```

The wrapper stages the selected tool's `candidates.json` entries into
`results/<judge-model>/candidates.json`, sets placeholder-free Martian env vars
for the temporary bridge, and invokes:

```sh
uv run python -m code_review_benchmark.step3_judge_comments
```

The default hagent source candidates are under
`results/openai_gpt-5.2/candidates.json`; override with `--candidates-file` or
`--candidate-model` when judging a different prepared tool. By default,
evaluations are written to
`/tmp/code-review-benchmark/offline/results/gpt-5.4-mini/evaluations-hagent.json`.

To judge an existing Muzen concurrent run with official step 3, point the
wrapper at the concurrent `summary.json`; it materializes `muzen` review rows
and candidates before launching the judge:

```sh
OPENAI_BASE_URL=http://127.0.0.1:4141/v1 \
OPENAI_API_KEY=muzen-codex-proxy \
MARTIAN_JUDGE_MODEL=gpt-5.4-mini \
node bench/review-quality/tools/run-martian-official-step3.mjs \
  --benchmark-root /tmp/code-review-benchmark \
  --tool muzen \
  --summary-file /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-50-shared-c5/summary.json \
  --force true
```

For a fresh full-circle Muzen run, use `run-muzen-martian-concurrent.mjs` to
generate reviews, then pass that run's `summary.json` to
`run-martian-official-step3.mjs`:

```sh
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
EVAL_ROOT="/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-5-full-circle-${RUN_ID}"

OPENAI_BASE_URL=http://127.0.0.1:4141/v1 \
OPENAI_API_KEY=muzen-codex-proxy \
MODEL=gpt-5.5 \
node bench/review-quality/tools/run-muzen-martian-concurrent.mjs \
  --case-source /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-50-shared-c5/summary.json \
  --golden-dir /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-direct/goldens \
  --worktree-root /tmp/muzen-hagent-martian-worktrees \
  --runner-path target/release/muzen-runner \
  --runner-mode shared \
  --output-dir "$EVAL_ROOT/reviews" \
  --concurrency 2 \
  --limit 5 \
  --sessions 0 \
  --max-active 1 \
  --max-turns 60 \
  --max-tool-calls 50 \
  --model "$MODEL" \
  --skip-semantic true \
  --progress true

OPENAI_BASE_URL=http://127.0.0.1:4141/v1 \
OPENAI_API_KEY=muzen-codex-proxy \
MARTIAN_JUDGE_MODEL=gpt-5.4-mini \
node bench/review-quality/tools/run-martian-official-step3.mjs \
  --benchmark-root /tmp/code-review-benchmark \
  --tool muzen \
  --summary-file "$EVAL_ROOT/reviews/summary.json" \
  --force true \
  --evaluations-file "$EVAL_ROOT/evaluations-muzen.json"
```

## Bounded Real Reviewer

Use this when the goal is to test Muzen's autonomous reviewer while keeping one
PR bounded enough for live iteration:

```sh
node bench/review-quality/run-production-review.mjs \
  --repo /tmp/muzen-hagent-martian-worktrees/keycloak-keycloak-pr-37429 \
  --base-ref hagent-martian/pr-37429-base \
  --runner-path target/release/muzen-runner \
  --golden /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-direct/goldens/keycloak-keycloak-pull-37429.json \
  --mode review \
  --sessions 0 \
  --max-active 1 \
  --max-turns 60 \
  --max-tool-calls 50 \
  --model "$MODEL" \
  --output /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-bounded/keycloak-keycloak-pull-37429.json \
  --trace-output-dir /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-bounded/traces/keycloak-keycloak-pull-37429 \
  --progress true
```

`--mode review --sessions 0` creates the default orchestrator and is the scored
review-quality path. Wrapper-provided `--max-turns` and `--max-tool-calls` are
sent as a run-level budget on this path. Passing one or more sessions selects
direct-session mode, so model-bearing scored harnesses fail fast when explicit
sessions are requested. Use direct sessions only when the objective is raw
session/protocol diagnostics rather than published finding quality.

## Manager-Approved Runner-Mode Diagnostic Eval

Do not run this block without explicit manager approval. It uses the
subscription-backed proxy and real hosted model calls. Keep semantic scoring
disabled for the first runner-mode comparison so the only live spend is the
Muzen review pass.

```sh
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
EVAL_ROOT="/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-runner-mode-${RUN_ID}"
CASE_SOURCE="/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-50-retry/summary.json"
GOLDEN_DIR="/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-direct/goldens"
WORKTREE_ROOT="/tmp/muzen-hagent-martian-worktrees"

cargo build --release --bin muzen-runner

export OPENAI_BASE_URL=http://127.0.0.1:4141/v1
export OPENAI_API_KEY=muzen-codex-proxy
export MODEL=gpt-5.5

node bench/review-quality/tools/run-muzen-martian-concurrent.mjs \
  --case-source "$CASE_SOURCE" \
  --golden-dir "$GOLDEN_DIR" \
  --worktree-root "$WORKTREE_ROOT" \
  --runner-path target/release/muzen-runner \
  --runner-mode shared \
  --output-dir "$EVAL_ROOT/shared" \
  --concurrency 5 \
  --limit 50 \
  --sessions 0 \
  --max-active 1 \
  --max-turns 60 \
  --max-tool-calls 50 \
  --model "$MODEL" \
  --skip-semantic true \
  --progress true

node bench/review-quality/tools/run-muzen-martian-concurrent.mjs \
  --case-source "$CASE_SOURCE" \
  --golden-dir "$GOLDEN_DIR" \
  --worktree-root "$WORKTREE_ROOT" \
  --runner-path target/release/muzen-runner \
  --runner-mode process \
  --output-dir "$EVAL_ROOT/process" \
  --concurrency 5 \
  --limit 50 \
  --sessions 0 \
  --max-active 1 \
  --max-turns 60 \
  --max-tool-calls 50 \
  --model "$MODEL" \
  --skip-semantic true \
  --progress true

node bench/review-quality/tools/compare-muzen-runner-modes.mjs \
  --shared "$EVAL_ROOT/shared" \
  --process "$EVAL_ROOT/process" \
  --output "$EVAL_ROOT/runner-mode-compare.json"

node bench/review-quality/tools/forensic-compare-muzen-runner-modes.mjs \
  --shared "$EVAL_ROOT/shared" \
  --process "$EVAL_ROOT/process" \
  --format markdown \
  --output "$EVAL_ROOT/runner-mode-forensics.md"
```

When iterating on a recall change, keep the positive side on `MODEL` and run a
cheap mini anti-cheat sweep separately:

```sh
node bench/review-quality/run-anti-cheat.mjs \
  --runner-path target/release/muzen-runner \
  --model "$ANTI_CHEAT_MODEL" \
  --output-dir bench/results-review-quality/gpt54mini-low-anticheat
```

Use result directory names that identify the model, for example
`gpt55-low-positive-*`, `gpt54mini-low-anticheat-*`, and
`gpt55-low-anticheat-promotion-*`.

For full subscription-backed suites, run the current Martian suite:

```sh
node bench/review-quality/run-martian-suite.mjs \
  --sessions 0 \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-martian-suite
```

## Saved Results

Each `run-production-review.mjs` result records the temp repo, base ref, model,
and JSON-RPC frames path under `inputs` and `artifacts.frames`. Use those saved
artifacts to inspect harness, extraction, scoring, and reporting bugs before
spending another subscription-backed model call. The current harness does not
provide a no-model replay command, so prompt, policy, tool, context, model, or
runner behavior changes still require a fresh run through the proxy.

## Regression Run

After a real agent change, run the broader suite:

```sh
node bench/review-quality/run-martian-suite.mjs \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --sessions 0 \
  --output-dir bench/results-review-quality/gpt55-low-martian-suite
```

For promotion checkpoints, rerun anti-cheat with the target model as well as
the mini smoke. This is the precision evidence for the shipped review path:

```sh
node bench/review-quality/run-anti-cheat.mjs \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-anticheat-promotion
```

Summarize:

```sh
node bench/review-quality/summarize-results.mjs \
  bench/results-review-quality/gpt55-low-martian-suite/*.json
```

## Gates

Keep a change only when:

- every scored run has `reviewValid: true`
- cheap live anti-cheat controls have `falsePositiveCount: 0` during iteration
- target-model anti-cheat controls have `falsePositiveCount: 0` before
  promotion or merge-ready claims
- positive runs improve hit rate, or preserve hit rate with fewer false
  positives, fewer tokens, or clearer trace evidence
- direct-session protocol/session diagnostics have zero parse failures when
  that mode is explicitly under test
- traces show the intended mechanism actually happened: relevant reads,
  searches, tool calls, candidate decisions, or rejection reasons

Do not optimize prompts, policies, tools, or scoring against an individual PR,
golden, or fixture. Treat benchmark misses as evidence for general agent
behavior, then validate any change across positive and negative controls.

Useful per-run artifacts:

- result JSON files under the selected `--output-dir`
- JSON-RPC frames paths recorded in each result at `artifacts.frames`
- extracted trace files under `--trace-output-dir` when the specific command
  supports that option
