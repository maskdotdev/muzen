# Muzen Eval UI

A zero-dependency local web UI for launching review-quality evals and inspecting
the **full trace** of a run — sessions, model turns, tool calls, candidate
decisions, golden coverage, event coverage, and the raw event stream. Styled to
match the Muzen "Hive Engine" site (void blacks + electric cyan).

It is eval tooling only: it never touches Muzen core, never reads `.env` files,
never echoes secret values, and binds to loopback.

## Run it

```sh
node bench/review-quality/eval-ui/server.mjs            # → http://127.0.0.1:7777
node bench/review-quality/eval-ui/server.mjs --port 8080
```

No `npm install`, no build step. Requires only Node (24+).

## What you can do

- **Browse runs** — every trace directory and top-level result JSON under
  `bench/results-review-quality/` is listed, newest first. Filter by text or by
  kind (trace vs result).
- **Inspect a run**:
  - **Summary** — hit rate, golden hits/misses, false positives, model turns,
    tool calls, sessions, peak RSS, elapsed, tokens, plus the run inputs.
  - **Findings** — each model finding with location, claim, severity,
    confidence, challenge/validation status, and expandable evidence.
  - **Trace** — sessions → turns → entries timeline (model turns, tool-call
    requests, candidate decisions, synthesis summaries, resource samples…),
    color-coded by trace kind.
  - **Coverage** — event-coverage LEDs + counters + trace kinds + audit
    diagnostics.
  - **Raw events** — paged, filterable view of `all-events.jsonl`.
- **Launch a run** — the **New run** button runs a curated preset:
  - *Local gate (fake model)* — `check-local.mjs`; needs a built
    `muzen-runner`, but does not make live model calls. It includes the
    fake Codex-proxy-shaped path and fake protocol pressure sweep.
  - *Martian suite / Anti-cheat suite* — need a built `muzen-runner` and a
    model. They run autonomous review (`--sessions 0`) so review-quality
    scoring uses published findings instead of raw direct-session outputs.
    Output lands under `bench/results-review-quality/eval-ui-runs/…` and is
    picked up by the run browser when the run finishes.

  Live stdout/stderr streams into the console while the run executes.

## Model auth

Launching model-bearing presets reuses the server process's environment. For
direct API billing, export `OPENAI_API_KEY` before starting the server.

To run live evals through a ChatGPT subscription (see
[`../SUBSCRIPTION_EVALS.md`](../SUBSCRIPTION_EVALS.md)), check **Route model
calls through local Codex proxy** and pick a **Codex account**:

- **Already-running proxy (:4141)** — use a proxy you started yourself; the run
  points at `http://127.0.0.1:4141/v1`.
- **A specific CodexBar account** — muzen starts (and reuses) a managed Codex
  proxy bound to that account on its own free port and points the run at it. The
  account fixes at proxy-startup, so each account gets its own proxy process;
  they are torn down when the eval-ui server stops. The accounts list comes from
  CodexBar's `managed-codex-accounts.json` (only the email + workspace label are
  surfaced — never auth files or home paths).

If an account has no cached auth, the run will say so; refresh it with
`node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs login`.

## Data sources

The UI only reads artifacts that already exist on disk:

| File | Used for |
| --- | --- |
| `<run>.json` | summary, findings, benchmark, inputs |
| `agent-trace.json` | session/turn/entry timeline |
| `event-coverage.json` | coverage LEDs + counters |
| `audit-diagnostics.json` | audit panel |
| `all-events.jsonl` | raw event viewer |

These are produced by the review-quality harness when `--trace-output-dir` is
passed (see [`../README.md`](../README.md)).
