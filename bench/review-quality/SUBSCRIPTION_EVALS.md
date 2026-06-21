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
  --include-codex-proxy true
```

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
  --fixture safe-message-retry-cleanup \
  --mode direct_sessions \
  --sessions 1 \
  --runner-path target/release/muzen-runner \
  --model "$ANTI_CHEAT_MODEL" \
  --output-dir bench/results-review-quality/gpt54mini-low-smoke-clean \
  --trace-output-dir bench/results-review-quality/traces/gpt54mini-low-smoke-clean
```

Positive direct-session check:

```sh
node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com \
  --pr 14943 \
  --mode direct_sessions \
  --sessions 1 \
  --runner-path target/release/muzen-runner \
  --golden bench/review-quality/goldens/cal-pr-14943.json \
  --output bench/results-review-quality/gpt55-low-cal-pr-14943.json \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-cal-pr-14943
```

Domain-neutral positive/negative pair for collection-subset propagation:

```sh
node bench/review-quality/run-synthetic-suite.mjs \
  --positive-fixture collection-subset-return-original \
  --mode direct_sessions \
  --sessions 1 \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/gpt55-low-synthetic-suite \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-synthetic-suite
```

Omit `--positive-fixture` to run every registered domain-neutral positive and
its paired anti-cheat control.

## Bounded Real Reviewer

Use this when the goal is to test Muzen's autonomous reviewer, not direct-session
extraction, but keep one PR bounded enough for live iteration:

```sh
node bench/review-quality/run-production-review.mjs \
  --repo /tmp/muzen-hagent-martian-worktrees/keycloak-keycloak-pr-37429 \
  --base-ref hagent-martian/pr-37429-base \
  --runner-path target/release/muzen-runner \
  --golden /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-direct/goldens/keycloak-keycloak-pull-37429.json \
  --mode review \
  --sessions 1 \
  --max-active 1 \
  --max-turns 60 \
  --max-tool-calls 50 \
  --model "$MODEL" \
  --output /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-bounded/keycloak-keycloak-pull-37429.json \
  --trace-output-dir /tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-bounded/traces/keycloak-keycloak-pull-37429 \
  --progress true
```

`--mode review --sessions 1` still runs the autonomous `review-orchestrator`,
but the explicit session lets the harness pass a caller hard-cap budget. Keep
`--max-turns` at or above the tool budget for this path; the orchestrator may
need extra finalization turns after exploration.
`--mode review --sessions 0` creates the default adaptive orchestrator and can
spend much longer before producing the final result.

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
  --sessions 1 \
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
  --sessions 1 \
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
  --mode review \
  --runner-path target/release/muzen-runner \
  --model "$ANTI_CHEAT_MODEL" \
  --output-dir bench/results-review-quality/gpt54mini-low-anticheat \
  --trace-output-dir bench/results-review-quality/traces/gpt54mini-low-anticheat
```

Use result directory names that identify the model, for example
`gpt55-low-positive-*`, `gpt54mini-low-anticheat-*`, and
`gpt55-low-anticheat-promotion-*`.

For full subscription-backed suites, prefer checkpointing so completed pairs
are not lost if the terminal session is interrupted:

```sh
node bench/review-quality/run-synthetic-suite.mjs \
  --mode review \
  --sessions 0 \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-synthetic-suite \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-synthetic-suite \
  --checkpoint
```

Resume the same output directory without rerunning already-passed pairs:

```sh
node bench/review-quality/run-synthetic-suite.mjs \
  --mode review \
  --sessions 0 \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-synthetic-suite \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-synthetic-suite \
  --resume
```

If a full live suite is run fixture-by-fixture or rerun in patches, aggregate
the checkpointed suite summaries without another model call:

```sh
node bench/review-quality/run-synthetic-suite.mjs \
  --mode review \
  --sessions 0 \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-synthetic-suite-aggregate \
  --aggregate-from bench/results-review-quality/gpt55-low-synthetic-suite,bench/results-review-quality/gpt55-low-synthetic-reruns
```

Later `--aggregate-from` directories override earlier entries for the same
fixture, and each pair records its `aggregateSource`.

Domain-neutral positive/negative pair for removed authorization or scope
guards:

```sh
node bench/review-quality/run-synthetic-suite.mjs \
  --positive-fixture removed-decision-guard \
  --mode direct_sessions \
  --sessions 1 \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/gpt55-low-synthetic-removed-guard \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-synthetic-removed-guard
```

## Replay Harness Changes

If a live run exposes a harness, extraction, scoring, or reporting bug, fix the
harness and replay the saved JSON-RPC frames before spending another
subscription-backed model call. The original result records the temp repo,
base ref, model, and frames under `inputs` and `artifacts.frames`.

```sh
node bench/review-quality/run-production-review.mjs \
  --frames /path/to/frames.jsonl \
  --repo /path/to/materialized-fixture-repo \
  --base-ref HEAD~1 \
  --mode direct_sessions \
  --model gpt-5.5 \
  --golden bench/review-quality/goldens/synthetic-removed-decision-guard.json \
  --output bench/results-review-quality/replayed.json \
  --trace-output-dir bench/results-review-quality/traces/replayed
```

Replay is valid evidence for harness-only changes because it reuses the exact
model output and reruns local parsing, extraction, scoring, and trace
generation. Replay is not evidence for prompt, policy, tool, context, or model
behavior changes; those require a fresh live run through the proxy.

## Regression Run

After a real agent change, run the broader suite:

```sh
node bench/review-quality/run-martian-suite.mjs \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --anti-cheat-model "$ANTI_CHEAT_MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-martian-suite \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-martian-suite
```

For promotion checkpoints, rerun anti-cheat with the target model as well as
the mini smoke. This is the precision evidence for the shipped review path:

```sh
node bench/review-quality/run-anti-cheat.mjs \
  --mode review \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-anticheat-promotion \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-anticheat-promotion
```

Summarize:

Fresh Martian diagnostics and semantic scorer outputs include
`summary.rankedSlices` for `top1`, `top3`, `primary`, and
`primaryOrSecondary` findings when review results include `reviewPriority`
metadata. These ranked slices are reporting-only: they do not suppress findings,
alter full semantic hit/miss/false-positive counts, or give Muzen extra golden
credit. Use them to inspect whether primary ranking separates core issues from
plausible adjacent findings before changing product presentation.

Backfill or aggregate ranked slices from a saved fresh diagnostic run without
another model call:

```sh
node bench/review-quality/tools/summarize-ranked-slices.mjs \
  --summary bench/results-review-quality/gpt55-low-fresh-martian-diagnostics/summary-gpt-5.5.json \
  --output bench/results-review-quality/gpt55-low-fresh-martian-diagnostics/ranked-slices.json
```

Compare a candidate experiment against a baseline without another model call.
Gate expressions are deltas from baseline to candidate, so `full.hits>=0`
means the candidate must not lose full semantic hits:

```sh
node bench/review-quality/tools/compare-ranked-slices.mjs \
  --baseline bench/results-review-quality/gpt55-low-fresh-martian-baseline/ranked-slices.json \
  --candidate bench/results-review-quality/gpt55-low-fresh-martian-candidate/ranked-slices.json \
  --output bench/results-review-quality/gpt55-low-fresh-martian-candidate/ranked-slice-comparison.json \
  --gate 'full.hits>=0' \
  --gate 'full.falsePositives<=0' \
  --gate 'top3.precision>=0' \
  --gate 'primaryOrSecondary.precision>=0'
```

Record promotion or rejection decisions in
`bench/review-quality/promotion-gates.json`, then verify the records without
another model call:

```sh
node bench/review-quality/tools/check-promotion-gates.mjs
```

The local no-targeting guard scans `src/reviewer_kernel` and the active
direct-session review harness for exact scored-corpus case IDs, golden IDs,
golden titles, and golden file paths. Synthetic control goldens are excluded
from that term set because they are neutral harness fixtures. The same guard
also rejects failed broad-count prompt patterns such as "return every distinct
supported bug", "single best bug" counterprompts, and fixed finding-count
language:

```sh
node bench/review-quality/tools/check-no-targeted-review-logic.mjs
```

Verify that every required review lens has product guidance, direct-session
harness guidance, a domain-neutral positive, and paired anti-cheat control:

```sh
node bench/review-quality/tools/check-lens-coverage.mjs
```

```sh
node bench/review-quality/summarize-results.mjs \
  bench/results-review-quality/gpt55-low-martian-suite/*.json
```

For Martian semantic scoring, keep unmatched-candidate adjudication separate
from the benchmark precision/recall numbers. This does not change hit, miss, or
false-positive counts; it only classifies unmatched candidates as
`plausible_actionable`, `insufficient_evidence`, or `unclear` so benchmark
precision regressions are not confused with proof that every unmatched finding
is bad:

```sh
node bench/review-quality/tools/score-martian-semantic.mjs \
  --result bench/results-review-quality/gpt55-low-martian-cal-pr-14740.json \
  --golden bench/review-quality/goldens/martian-cal-pr-14740.json \
  --model "$ANTI_CHEAT_MODEL" \
  --adjudicate-unmatched \
  --output bench/results-review-quality/gpt55-low-martian-cal-pr-14740.semantic-adjudicated-gpt54mini.json
```

For fresh Martian-style diagnostics outside the fixed regression suite, use the
mixed runner. It runs selected imported Martian goldens, then immediately runs
semantic adjudication and a cheap anti-cheat sweep. This is diagnostic evidence
only: do not promote prompt or product changes from a single PR without a
general positive/negative control.

```sh
node bench/review-quality/run-fresh-martian-diagnostics.mjs \
  --runner-path target/release/muzen-runner \
  --model "$MODEL" \
  --semantic-model "$ANTI_CHEAT_MODEL" \
  --anti-cheat-model "$ANTI_CHEAT_MODEL" \
  --output-dir bench/results-review-quality/gpt55-low-fresh-martian-diagnostics \
  --trace-output-dir bench/results-review-quality/traces/gpt55-low-fresh-martian-diagnostics
```

Pass `--case martian-cal-pr-10967` or `--pr 10967,14740` to run a smaller
subset. The default set is the imported Martian Cal.com cases that are not part
of `run-martian-suite.mjs`.

## Gates

Keep a change only when:

- every scored run has `reviewValid: true`
- cheap live anti-cheat controls have `falsePositiveCount: 0` during iteration
- target-model anti-cheat controls have `falsePositiveCount: 0` before
  promotion or merge-ready claims
- positive runs improve hit rate, or preserve hit rate with fewer false
  positives, fewer tokens, or clearer trace evidence
- direct-session extraction has zero parse failures
- traces show the intended mechanism actually happened: relevant reads,
  searches, tool calls, candidate decisions, or rejection reasons

Do not optimize prompts, policies, tools, or scoring against an individual PR,
golden, or fixture. Treat benchmark misses as evidence for general agent
behavior, then validate any change across positive and negative controls.

Useful trace files live under each `--trace-output-dir`:

- `agent-trace.json`
- `event-coverage.json`
- `audit-diagnostics.json`
- `all-events.jsonl`
