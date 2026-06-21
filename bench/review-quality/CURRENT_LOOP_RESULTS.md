# Current Loop Benchmark Results

## Shared/process runner fake-proxy investigation

Generated on 2026-06-21 with `target/release/muzen-runner` while diagnosing
shared/concurrent runner behavior before any further subscription-backed evals.

The current fake-first gate sequence is:

```sh
node bench/review-quality/check-local.mjs \
  --runner-path target/release/muzen-runner \
  --include-codex-proxy true \
  --include-protocol-pressure true
```

That opt-in gate runs the standard fake local probes plus a Codex-proxy-shaped
retry probe. The proxy probe starts the real local Codex Responses proxy with a
temporary fake auth file, points it at the fake Responses server, and exercises
the same `OPENAI_BASE_URL` path used by live subscription evals without making
live model calls.

Latest fake/local proxy-plus-protocol integrated result:

```text
bench/results-review-quality/check-local-autonomous-hardcap-proxy-protocol-20260621T072057Z
```

This passed all 9 local probes (`finalize-after-one-tool`,
`symmetric-tool-budget-exhaustion`, `candidate-publication`,
`schema-repair-per-conversation`, `provider-queue-saturation`,
`caller-hard-cap-budget`, `autonomous-hard-cap-budget`,
`autonomous-hard-cap-exhaustion`,
`codex-proxy-deterministic-retry`) plus one fake protocol mixed-pressure sweep
with no regressions. The autonomous hard-cap exhaustion probe now forces a fake
model to keep requesting tools under `--sessions 0 --max-tool-calls 4`; both
shared and process mode stop at exactly 5 model calls, 4 tool calls, one
exhausted run, and zero shared-only exhaustions. The proxy-shaped fake probe
published 5 findings in both shared and process mode with 36 model calls and 10
tool calls per mode. The protocol sweep retained shared/process status,
heartbeat, status-poll, and completed-run budget-accounting parity.

Direct-session scoring finding:

```text
bench/results-review-quality/min-direct-session-candidate-20260621T064921Z
bench/results-review-quality/min-orchestrator-candidate-20260621T064921Z
```

With the same fake candidate model response, direct-session mode accepted the
final output but published 0 findings and 0 candidate lifecycle events.
Autonomous review (`--sessions 0`) published 1 finding with validation and
candidate lifecycle events. This confirms direct sessions are a raw protocol /
session-output path, not a scored review-quality path. Scored live evals and
eval UI launch presets should use autonomous review (`--sessions 0`).

Scored live-wrapper guard:

```text
bench/results-review-quality/check-local-session-guard-20260621T070217Z
```

`run-github-pr-review.mjs --sessions 1` and
`run-muzen-martian-concurrent.mjs --sessions 1 --model gpt-5.5` now fail before
network, runner, or model work with an explicit message that scored
review-quality runs must use `--sessions 0`. The fake local gate still passes,
so direct-session diagnostics remain available for deterministic fake/protocol
harnesses.

Autonomous budget hard-cap finding:

```text
bench/results-review-quality/min-autonomous-budget-surface-20260621T071255Z
bench/results-review-quality/min-autonomous-budget-hardcap-20260621T071718Z
```

Before this fix, the scored/autonomous wrapper sent `--max-tool-calls` and
`--max-turns` only inside explicit `sessions[]`. Under `--sessions 0`, the
run-start had no session budget, so the autonomous planner correctly treated the
orchestrator as adaptive and raised the tool budget to the 48-call minimum. A
single fake case requested `--max-tool-calls 3 --max-turns 8` but ran 49 model
turns and 48 tool calls in both shared and process mode. The protocol now
accepts a run-level `budget`, and scored wrappers send it when `sessions === 0`.
The same minimized fake case now stops at 4 model calls and 3 tool calls in both
modes. This explains the apparent budget overrun as a wrapper/protocol contract
gap, not a shared-runner-only concurrency failure.

Latest `check-local --include-codex-proxy true` result after removing fake
validation repair noise:

| Probe | Shared findings | Process findings | Shared model calls | Process model calls | Shared tool calls | Process tool calls | Shared provider errors | Process provider errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| candidate-publication | 5 | 5 | 20 | 20 | 10 | 10 | 0 | 0 | passed |
| codex-proxy-deterministic-retry | 5 | 5 | 36 | 36 | 10 | 10 | 16 | 16 | passed |
| schema-repair-per-conversation | 0 | 0 | 15 | 15 | 10 | 10 | 0 | 0 | passed |

Corrected autonomous candidate publication stress sweep:

```sh
node bench/review-quality/tools/run-fake-runner-mode-sweep.mjs \
  --runner-path target/release/muzen-runner \
  --concurrency 1,4,8,12 \
  --cases 16 \
  --sessions 0 \
  --max-tool-calls 8 \
  --max-turns 12 \
  --tools-before-final 1 \
  --http-error-attempts-per-request 1 \
  --final-mode candidate \
  --latency-ms 25 \
  --jitter-ms 0 \
  --max-concurrent 1 \
  --via-codex-proxy true
```

Result file:

```text
bench/results-review-quality/fake-autonomous-hardcap-candidate-sweep-20260621T072438Z/sweep-summary.json
```

| Concurrency | Findings shared/process | Model calls shared/process | Tool calls shared/process | Tokens shared/process | Fake 500s shared/process | Accepted candidates per case | Final outputs per case | Result |
| ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 16 / 16 | 113 / 113 | 32 / 32 | 1152 / 1152 | 49 / 49 | 1 / 1 | 2 / 2 | passed |
| 4 | 16 / 16 | 113 / 113 | 32 / 32 | 1152 / 1152 | 49 / 49 | 1 / 1 | 2 / 2 | passed |
| 8 | 16 / 16 | 113 / 113 | 32 / 32 | 1152 / 1152 | 49 / 49 | 1 / 1 | 2 / 2 | passed |
| 12 | 16 / 16 | 113 / 113 | 32 / 32 | 1152 / 1152 | 49 / 49 | 1 / 1 | 2 / 2 | passed |

Corrected autonomous budget exhaustion stress sweep:

```sh
node bench/review-quality/tools/run-fake-runner-mode-sweep.mjs \
  --runner-path target/release/muzen-runner \
  --concurrency 1,4,8,12 \
  --cases 16 \
  --sessions 0 \
  --max-tool-calls 4 \
  --max-turns 8 \
  --tools-before-final infinite \
  --http-error-attempts-per-request 1 \
  --final-mode clean \
  --latency-ms 25 \
  --jitter-ms 0 \
  --max-concurrent 1 \
  --via-codex-proxy true
```

Result file:

```text
bench/results-review-quality/fake-autonomous-hardcap-budget-sweep-20260621T072733Z/sweep-summary.json
```

| Concurrency | Exhausted shared/process | Max tool calls shared/process | Tool calls used per case | Shared-only exhaustions | Model calls shared/process | Tool calls shared/process | Fake 500s shared/process | Result |
| ---: | --- | --- | --- | ---: | --- | --- | --- | --- |
| 1 | 16 / 16 | 4 / 4 | 4 / 4 | 0 | 160 / 160 | 64 / 64 | 80 / 80 | passed |
| 4 | 16 / 16 | 4 / 4 | 4 / 4 | 0 | 160 / 160 | 64 / 64 | 80 / 80 | passed |
| 8 | 16 / 16 | 4 / 4 | 4 / 4 | 0 | 160 / 160 | 64 / 64 | 80 / 80 | passed |
| 12 | 16 / 16 | 4 / 4 | 4 / 4 | 0 | 160 / 160 | 64 / 64 | 80 / 80 | passed |

Corrected autonomous large-fixture stress sweep:

```sh
node bench/review-quality/tools/run-fake-runner-mode-sweep.mjs \
  --runner-path target/release/muzen-runner \
  --concurrency 4,8,12 \
  --cases 8 \
  --sessions 0 \
  --fixture-extra-lines 400 \
  --fixture-line-bytes 160 \
  --max-tool-calls 8 \
  --max-turns 12 \
  --tools-before-final 1 \
  --http-error-attempts-per-request 1 \
  --final-mode candidate \
  --latency-ms 25 \
  --jitter-ms 0 \
  --max-concurrent 1 \
  --via-codex-proxy true
```

Result file:

```text
bench/results-review-quality/fake-autonomous-hardcap-large-fixture-20260621T073245Z/sweep-summary.json
```

| Concurrency | Findings shared/process | Model calls shared/process | Tool calls shared/process | Tokens shared/process | Fake 500s shared/process | Accepted candidates per case | Failed finishes shared/process | Result |
| ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 | 8 / 8 | 57 / 57 | 16 / 16 | 576 / 576 | 25 / 25 | 1 / 1 | 0 / 0 | passed |
| 8 | 8 / 8 | 57 / 57 | 16 / 16 | 576 / 576 | 25 / 25 | 1 / 1 | 0 / 0 | passed |
| 12 | 8 / 8 | 57 / 57 | 16 / 16 | 576 / 576 | 25 / 25 | 1 / 1 | 0 / 0 | passed |

Protocol explicit-session and tool-accounting stress sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-tool-accounting-20260621T055951Z \
  --cases 6 \
  --concurrency 3 \
  --sessions 4 \
  --max-active-sessions 2 \
  --max-tool-calls 4 \
  --max-turns 8 \
  --tools-per-session 2 \
  --tool-calls-per-turn 1 \
  --tool-delay-ms 100 \
  --artifact-bytes 4096
```

Result file:

```text
bench/results-review-quality/fake-protocol-tool-accounting-20260621T055951Z/protocol-session-stress-summary.json
```

| Mode | Runs | Sessions per run | Completed per run | Session outputs per run | Tool calls per run | Diagnostic tool calls used | Diagnostic custom tool calls | Model callbacks | Tool callbacks | Callback ownership errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 6 | 4 | 4 | 4 | 8 | 8 | 8 | 72 | 48 | 0 | passed |
| process | 6 | 4 | 4 | 4 | 8 | 8 | 8 | 72 | 48 | 0 | passed |

Protocol direct-session budget-exhaustion sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-budget-exhaustion-20260621T060523Z \
  --cases 6 \
  --concurrency 3 \
  --sessions 4 \
  --max-active-sessions 2 \
  --max-tool-calls 1 \
  --max-turns 5 \
  --tools-per-session 2 \
  --tool-calls-per-turn 2 \
  --tool-delay-ms 100 \
  --artifact-bytes 4096
```

Result file:

```text
bench/results-review-quality/fake-protocol-budget-exhaustion-20260621T060523Z/protocol-session-stress-summary.json
```

| Mode | Runs | Sessions per run | Completed per run | Tool calls per run | Exhausted sessions per run | Budget-rejected tool calls per run | Model callbacks | Tool callbacks | Callback ownership errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 6 | 4 | 4 | 4 | 4 | 4 | 48 | 24 | 0 | passed |
| process | 6 | 4 | 4 | 4 | 4 | 4 | 48 | 24 | 0 | passed |

Protocol heartbeat pressure sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-heartbeat-continue-20260621T062012Z \
  --cases 4 \
  --concurrency 2 \
  --sessions 3 \
  --max-active-sessions 2 \
  --max-tool-calls 4 \
  --max-turns 8 \
  --tools-per-session 2 \
  --tool-calls-per-turn 1 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --heartbeat-mode continue \
  --heartbeat-interval-ms 25 \
  --heartbeat-lease-seconds 1 \
  --artifact-bytes 2048
```

Result file:

```text
bench/results-review-quality/fake-protocol-heartbeat-continue-20260621T062012Z/protocol-session-stress-summary.json
```

| Mode | Runs | Completed runs | Sessions per run | Tool calls per run | Heartbeat callbacks | Model callbacks | Tool callbacks | Callback ownership errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 4 | 4 | 3 | 6 | 116 | 36 | 24 | 0 | passed |
| process | 4 | 4 | 3 | 6 | 116 | 36 | 24 | 0 | passed |

Protocol heartbeat cancellation sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-heartbeat-cancel-result-20260621T062331Z \
  --cases 4 \
  --concurrency 2 \
  --sessions 3 \
  --max-active-sessions 2 \
  --max-tool-calls 4 \
  --max-turns 8 \
  --tools-per-session 2 \
  --tool-calls-per-turn 1 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --heartbeat-mode cancel-first \
  --heartbeat-interval-ms 25 \
  --heartbeat-lease-seconds 1 \
  --artifact-bytes 2048
```

Result file:

```text
bench/results-review-quality/fake-protocol-heartbeat-cancel-result-20260621T062331Z/protocol-session-stress-summary.json
```

| Mode | Runs | Completed runs | Cancelled runs | Stored cancelled results | Stored cancelled session outputs | Heartbeat callbacks | Run-failed notifications | Run-finished notifications | Callback ownership errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 4 | 3 | 1 | 1 | 3 | 88 | 1 | 3 | 0 | passed |
| process | 4 | 3 | 1 | 1 | 3 | 88 | 1 | 3 | 0 | passed |

Protocol in-flight status pressure sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-status-pressure-20260621T062012Z \
  --cases 4 \
  --concurrency 2 \
  --sessions 3 \
  --max-active-sessions 2 \
  --max-tool-calls 4 \
  --max-turns 8 \
  --tools-per-session 2 \
  --tool-calls-per-turn 1 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --status-poll-interval-ms 25 \
  --artifact-bytes 2048
```

Result file:

```text
bench/results-review-quality/fake-protocol-status-pressure-20260621T062012Z/protocol-session-stress-summary.json
```

| Mode | Runs | Completed runs | Running status polls per run | Status poll errors | Model callbacks | Tool callbacks | Callback ownership errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 4 | 4 | 34 | 0 | 36 | 24 | 0 | passed |
| process | 4 | 4 | 35 | 0 | 36 | 24 | 0 | passed |

Protocol explicit cancel request sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-request-cancel-result-20260621T062331Z \
  --cases 4 \
  --concurrency 2 \
  --sessions 3 \
  --max-active-sessions 2 \
  --max-tool-calls 4 \
  --max-turns 8 \
  --tools-per-session 2 \
  --tool-calls-per-turn 1 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --status-poll-interval-ms 25 \
  --request-cancel-mode cancel-first \
  --request-cancel-after-status 1 \
  --artifact-bytes 2048
```

Result file:

```text
bench/results-review-quality/fake-protocol-request-cancel-result-20260621T062331Z/protocol-session-stress-summary.json
```

| Mode | Runs | Completed runs | Cancelled runs | Stored cancelled results | Stored cancelled session outputs | Accepted cancel requests | Running status polls on cancelled run | Run-failed notifications | Callback ownership errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 4 | 3 | 1 | 1 | 3 | 1 | 6 | 1 | 0 | passed |
| process | 4 | 3 | 1 | 1 | 3 | 1 | 6 | 1 | 0 | passed |

Protocol mixed pressure gate:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-mixed-pressure-20260621T062610Z \
  --cases 6 \
  --concurrency 3 \
  --sessions 4 \
  --max-active-sessions 2 \
  --max-tool-calls 1 \
  --max-turns 5 \
  --tools-per-session 2 \
  --tool-calls-per-turn 2 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --heartbeat-mode continue \
  --heartbeat-interval-ms 25 \
  --heartbeat-lease-seconds 1 \
  --status-poll-interval-ms 25 \
  --request-cancel-mode cancel-first \
  --request-cancel-after-status 1 \
  --artifact-bytes 4096
```

Result file:

```text
bench/results-review-quality/fake-protocol-mixed-pressure-20260621T062610Z/protocol-session-stress-summary.json
```

| Mode | Runs | Completed runs | Cancelled runs | Stored cancelled results | Accepted cancel requests | Heartbeat callbacks | Running status polls per run min | Completed-run tool calls | Completed-run exhausted sessions | Completed-run budget rejections | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 6 | 5 | 1 | 1 | 1 | 104 | 6 | 4 | 4 | 4 | passed |
| process | 6 | 5 | 1 | 1 | 1 | 105 | 6 | 4 | 4 | 4 | passed |

Protocol mixed pressure repeat sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-mixed-pressure-sweep.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-mixed-pressure-sweep-20260621T062934Z \
  --iterations 3 \
  --cases 6 \
  --concurrency 3 \
  --sessions 4 \
  --max-active-sessions 2 \
  --max-tool-calls 1 \
  --max-turns 5 \
  --tools-per-session 2 \
  --tool-calls-per-turn 2 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --heartbeat-interval-ms 25 \
  --status-poll-interval-ms 25 \
  --request-cancel-after-status 1 \
  --artifact-bytes 4096
```

Result file:

```text
bench/results-review-quality/fake-protocol-mixed-pressure-sweep-20260621T062934Z/mixed-pressure-sweep-summary.json
```

| Iterations | Shared completed/cancelled | Process completed/cancelled | Heartbeat callbacks shared/process | Status polls min shared/process | Completed-run tool calls min shared/process | Completed-run budget rejections min shared/process | Result |
| ---: | --- | --- | --- | --- | --- | --- | --- |
| 3 | 15 / 3 | 15 / 3 | 103-104 / 105-106 | 6 / 6 | 4 / 4 | 4 / 4 | passed |

Protocol mixed large-fixture gate:

```sh
node bench/review-quality/tools/run-fake-protocol-session-stress.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-mixed-large-fixture-20260621T063315Z \
  --cases 6 \
  --concurrency 3 \
  --sessions 4 \
  --max-active-sessions 2 \
  --max-tool-calls 1 \
  --max-turns 5 \
  --tools-per-session 2 \
  --tool-calls-per-turn 2 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --fixture-extra-lines 400 \
  --fixture-line-bytes 160 \
  --heartbeat-mode continue \
  --heartbeat-interval-ms 25 \
  --heartbeat-lease-seconds 1 \
  --status-poll-interval-ms 25 \
  --request-cancel-mode cancel-first \
  --request-cancel-after-status 1 \
  --artifact-bytes 4096
```

Result file:

```text
bench/results-review-quality/fake-protocol-mixed-large-fixture-20260621T063315Z/protocol-session-stress-summary.json
```

| Mode | Runs | Completed runs | Cancelled runs | Fixture extra lines | Fixture line bytes | Heartbeat callbacks | Completed-run tool calls | Completed-run exhausted sessions | Completed-run budget rejections | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| shared | 6 | 5 | 1 | 400 | 160 | 106 | 4 | 4 | 4 | passed |
| process | 6 | 5 | 1 | 400 | 160 | 104 | 4 | 4 | 4 | passed |

Protocol mixed large-fixture repeat sweep:

```sh
node bench/review-quality/tools/run-fake-protocol-mixed-pressure-sweep.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/fake-protocol-mixed-large-fixture-sweep-20260621T063333Z \
  --iterations 2 \
  --cases 6 \
  --concurrency 3 \
  --sessions 4 \
  --max-active-sessions 2 \
  --max-tool-calls 1 \
  --max-turns 5 \
  --tools-per-session 2 \
  --tool-calls-per-turn 2 \
  --tool-delay-ms 120 \
  --model-delay-ms 20 \
  --fixture-extra-lines 400 \
  --fixture-line-bytes 160 \
  --heartbeat-interval-ms 25 \
  --status-poll-interval-ms 25 \
  --request-cancel-after-status 1 \
  --artifact-bytes 4096
```

Result file:

```text
bench/results-review-quality/fake-protocol-mixed-large-fixture-sweep-20260621T063333Z/mixed-pressure-sweep-summary.json
```

| Iterations | Shared completed/cancelled | Process completed/cancelled | Fixture extra lines | Fixture line bytes | Heartbeat callbacks shared/process | Status polls min shared/process | Completed-run tool calls min shared/process | Completed-run budget rejections min shared/process | Result |
| ---: | --- | --- | ---: | ---: | --- | --- | --- | --- | --- |
| 2 | 10 / 2 | 10 / 2 | 400 | 160 | 103-105 / 104-105 | 6 / 1 | 4 / 4 | 4 / 4 | passed |

`run-fake-runner-mode-sweep.mjs` exits nonzero by default if it reports
parity, release, or isolation regressions. Use `--fail-on-regression false`
only for exploratory chaos probes where a failing JSON report is the desired
artifact.

Interpretation:

- The deterministic fake/proxy path does not reproduce a shared-only
  concurrency regression. Shared and process modes match on findings, model
  calls, tool calls, token totals, retry counts, and terminal completion across
  queue pressure, candidate publication, schema repair, proxy-shaped retry, and
  forced max-tool-call exhaustion up to concurrency 16. They also match under a
  larger synthetic diff path with 130312 artifact bytes per case.
- The forced exhaustion probe answers the budget-scope concern for this harness:
  `maxToolCalls` is enforced per case in both modes. Every case exhausts exactly
  once when the fake model keeps requesting tools, and `sharedOnly` exhaustion is
  0 at every tested concurrency.
- The fake sweep now accepts `--sessions`, `--max-active`,
  `--fixture-extra-lines`, and `--fixture-line-bytes`. The large-fixture run
  confirms the fixture sizing path works, but it also exposed a coverage gap:
  hosted/autonomous review requests still use the autonomous orchestrator path
  and do not prove explicit protocol fan-out.
- The protocol stress harness now also accepts `--fixture-extra-lines` and
  `--fixture-line-bytes`, covering large synthetic fixtures on the explicit
  protocol direct-session path instead of only through the hosted/autonomous
  fake runner sweep.
- The protocol-level harness reproduced the explicit-session bug: non-empty
  `RunStartParams.sessions` were being collapsed to the autonomous
  `review-orchestrator` session. The direct-session runner path now treats
  caller-provided sessions as the contract: all requested sessions run under
  their own IDs, delayed callback tools preserve run/session ownership, and the
  runner result emits `sessionOutputs` for the SDK swarm mapping.
- Callback/custom tools now count as tool calls in aggregate metrics and
  per-session diagnostics. The protocol harness runs two callback tools per
  explicit session and fails if top-level `summary.toolCalls`, diagnostic
  `toolCallsUsed`, or diagnostic `toolCounts.custom` diverge from the expected
  session-local count.
- Direct-session callback-tool budget exhaustion is also covered. The budget
  sweep asks each session for two callback tool calls in one model turn with
  `maxToolCalls=1`; the runner schedules one, rejects one with
  `budget_exceeded`, forces finalization, and shared/process modes agree on
  completed sessions, exhausted-session counts, and rejection counts.
- Heartbeat pressure is covered while explicit sessions are active. In
  continue mode, heartbeat counts vary slightly with wall-clock scheduling, so
  the gate requires valid ownership and presence rather than exact heartbeat
  parity while preserving exact parity for model/tool callbacks and run
  metrics. In cancellation mode, declining the first heartbeat for
  `protocol-run-1` cancels exactly that run in both shared and process modes;
  the remaining runs complete and no callback crosses run/session ownership.
- In-flight `run.status` and explicit `run.cancel` pressure are covered against
  active shared runs. Status polling observes `running` while sessions are still
  executing, and an explicit cancel request against `protocol-run-1` cancels
  exactly one run while the remaining runs complete in both shared and process
  modes.
- Cancelled run visibility is now explicit. Expected cancellation paths fetch
  `run.result` after the terminal `run.start` error, require one stored
  `cancelled` result in both shared and process modes, and assert that stored
  session outputs are present for the cancelled run. Model/tool usage counters
  for the intentionally cancelled run are timing-sensitive, so cancellation
  gates assert containment, stored result shape, and ownership instead of exact
  model/tool parity at the cancellation boundary.
- Mixed protocol pressure is covered. The combined sweep runs heartbeat
  callbacks, in-flight status polling, one explicit cancellation, and forced
  callback-tool budget exhaustion together. It still requires shared/process
  agreement on terminal status counts and validates exact tool/budget accounting
  on the five completed runs.
- The mixed protocol pressure path now has a repeat sweep wrapper. The first
  three-iteration sweep covered 18 runs per mode with stable terminal status
  counts, heartbeat/status/cancel activity, stored cancelled results, and exact
  completed-run budget accounting.
- Large-fixture mixed protocol pressure is covered. The single-run and
  two-iteration sweep variants use 400 extra 160-byte fixture lines per case and
  still preserve shared/process terminal status counts and completed-run budget
  accounting.
- The earlier global `--http-error-every` stressor is useful only as a chaos
  probe. It assigns fake 500s by request arrival sequence, so minor shared vs
  process timing differences can make different conversations absorb retries
  and create false parity failures. Use `--http-error-attempts-per-request`
  for parity gates. If using `--http-error-every`, pass
  `--fail-on-regression false` unless the expected result is a nonzero exit.
- Process-mode frame logs still contain one pre-`run.start` handshake response
  frame without a `runId` per case. That is expected protocol metadata rather
  than mixed-run leakage; isolation gates continue to fail on orphan or
  unexpected run IDs.

Current recommendation: do not run live evals for this runner investigation yet.
The deterministic shared/process protocol path is now covered for explicit
multi-session fan-out, delayed callback tools, callback/custom tool accounting,
session-budget reporting, direct-session budget exhaustion, active-session
heartbeats, heartbeat-triggered cancellation, in-flight status polling, and
explicit cancel requests, including a combined mixed-pressure run. The next
useful fake-first step is a higher-volume mixed sweep across the new large
fixture knobs before considering live evals.

Generated on 2026-06-07 with `MODEL=gpt-5.4-mini` and `target/release/muzen`
after the planned-unit budget/bootstrap change.

| PR | Mode | Goldens | Hits | False positives | Findings | Notes |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| cal-pr-11059 | maxTurns 7 | 5 | 0 | 0 | 0 | Missed all return-shape/webhook credential goldens. |
| cal-pr-8330 | maxTurns 7 | 3 | 0 | 0 | 0 | Clean verdicts for all changed files. |
| cal-pr-14943 | maxTurns 7 | 1 | 0 | 1 | 1 | Found a retry-count issue, but not the golden non-SMS cleanup scope bug. |
| cal-pr-8330 | maxTurns 4 control | 3 | 0 | 0 | 0 | Legacy-shaped control produced needs_review, no findings. |
| cal-pr-14943 | maxTurns 4 control | 1 | 0 | 0 | 0 | Legacy-shaped control produced needs_review, no findings. |

Result: this benchmark set does **not** show a material reviewer-quality
improvement. The new loop gathers more evidence and can produce more confident
file verdicts, but recall remained 0/9 on current goldens and false positives
increased on cal-pr-14943.

## Final synthesis iteration

Generated on 2026-06-07 with `MODEL=gpt-5.4-mini` and `target/release/muzen`
after adding the benchmark-gated deterministic bootstrap plus a final no-tool
synthesis pass over unit verdicts and diff context.

| PR | Mode | Goldens | Hits | False positives | Findings | Tokens | Result | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| cal-pr-11059 | final synthesis, maxTurns 7 | 5 | 1 | 3 | 4 | 304,555 | failed_partial | Found webhook credential ownership. Still missed four return-shape goldens. |
| cal-pr-8330 | final synthesis, maxTurns 7 | 3 | 0 | 2 | 2 | 25,628 | failed_partial | Missed all slot/date goldens and introduced two synthesis false positives. Bootstrap exceeded the per-turn tool cap on this single large unit. |
| cal-pr-14943 | final synthesis, maxTurns 7 | 1 | 1 | 2 | 3 | 47,048 | failed_partial | Found the golden destructive cleanup scope issue, but added two duplicate/adjacent false positives. |

Delta versus the previous current loop: recall improved from 0/9 to 2/9
goldens, but false positives increased from 1 total to 7 total and every run
was diagnostic-only/failed_partial. This is not shippable as-is. The final
synthesis pass is useful as a recall probe, but it needs a stricter duplicate
merge/claim calibration step, and deterministic bootstrap must reserve per-turn
tool budget instead of issuing `read_diff + all files` blindly.

Final-synthesis result files:

```text
bench/results-review-quality/cal-pr-11059-final-synthesis-20260607T035923Z.json
bench/results-review-quality/cal-pr-8330-final-synthesis-20260607T040217Z.json
bench/results-review-quality/cal-pr-14943-final-synthesis-20260607T040234Z.json
```

## Verified synthesis iteration

Generated on 2026-06-07 with `MODEL=gpt-5.4-mini` and `target/release/muzen`
after adding budget-reserved bootstrap, synthesis verification, adjacent-claim
merge, speculative-claim rejection, and large-PR diagnostic-only synthesis.

| PR | Mode | Goldens | Hits | False positives | Findings | Tokens | Result | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| cal-pr-11059 | verified synthesis, large-PR gated | 5 | 0 | 0 | 0 | 330,314 | completed_no_findings | Precision recovered, but all return-shape/webhook goldens remain missed. |
| cal-pr-8330 | verified synthesis | 3 | 1 | 1 | 2 | 31,403 | completed_with_findings | Found the working-hours end-boundary issue; still missed selected-slot/date equality goldens. |
| cal-pr-14943 | verified synthesis | 1 | 1 | 0 | 1 | 46,665 | completed_with_findings | Found only the destructive cleanup scope issue; duplicate/adjacent false positives removed. |

Delta versus final-synthesis iteration: all runs are now valid/publishable, false
positives dropped from 7 total to 1 total, and recall held at 2/9. This meets
the precision/validity target but does not solve cal-pr-11059. The next recall
work should focus specifically on deterministic return-shape producer/consumer
packs for large PRs, because large-PR synthesis is now diagnostic-only to avoid
publishing noisy findings.

Verified-synthesis result files:

```text
bench/results-review-quality/cal-pr-11059-verified-synthesis-large-gated-20260607T122303Z.json
bench/results-review-quality/cal-pr-8330-verified-synthesis-filtered-20260607T122005Z.json
bench/results-review-quality/cal-pr-14943-verified-synthesis-20260607T121828Z.json
```

Reproduce with:

```sh
cargo build --release --bin muzen

MODEL=gpt-5.4-mini node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com --pr 11059 \
  --runner-path target/release/muzen \
  --golden bench/review-quality/goldens/cal-pr-11059.json \
  --sessions 11 --max-active 4 --max-turns 7 --max-tool-calls 14

MODEL=gpt-5.4-mini node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com --pr 8330 \
  --runner-path target/release/muzen \
  --golden bench/review-quality/goldens/cal-pr-8330.json \
  --sessions 11 --max-active 4 --max-turns 7 --max-tool-calls 14

MODEL=gpt-5.4-mini node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com --pr 14943 \
  --runner-path target/release/muzen \
  --golden bench/review-quality/goldens/cal-pr-14943.json \
  --sessions 11 --max-active 4 --max-turns 7 --max-tool-calls 14
```

## Multi-lens + adjudication iteration

Generated on 2026-06-12 with `target/release/muzen` after the multi-lens
fan-out (high-risk units run Correctness/Security/Performance lens sessions),
agreement-derived confidence, adversarial challenge pass, prompt-budget
eviction, and incremental message assembly.

| PR | Model | Sessions | Goldens | Hits | False positives | Candidates | Tokens | Wall | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| cal-pr-11059 | gpt-5.4-mini | 33 (11 units x 3 lenses) | 5 | 0 | 0 | 0 | 940,403 | 45s | Lens fan-out verified live (`unit-NNN#security` etc.). Full exploration (97 head reads, 38 searches), 39 clean verdicts, but no session produced a candidate finding, so synthesis/challenge had nothing to adjudicate. |
| cal-pr-11059 | qwen3:8b (Ollama) | 33 | 5 | 0 | 0 | 0 | 115,869 | 864s | Local-model path works end-to-end at zero API cost, but the 8B model answers in one turn per session without exploring (0 searches) and rubber-stamps needs_review. Useful as a free harness smoke test, not as a quality signal. |

Takeaways:

- Multi-lens tripled sessions and roughly tripled token cost on this PR
  without adding recall. The misses are all cross-file return-shape contract
  bugs; the bottleneck is evidence retrieval (sessions never surface the
  caller/callee shape mismatch as a candidate), not adjudication - the
  challenge pass never fired because there was nothing to challenge.
- Recall on these goldens likely needs the context-engine retrieval work
  (reference-graph expansion to put the callback callers in front of the
  refresh-helper reviewer), not more reviewers per unit.
- deepseek-r1 in Ollama accepts a tools array but never emits tool calls;
  qwen3:8b tool-calls correctly and is the recommended local smoke model.

## Score-gated lenses + cached-token visibility

Generated on 2026-06-12 after gating lens fan-out on planner score >= 80
(stacked path sensitivity) and surfacing provider prompt-cache reads in
usage metrics.

| PR | Model | Sessions | Goldens | Hits | FP | Input tokens | Cache-served | Wall | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| cal-pr-11059 | gpt-5.4-mini | 13 (was 33) | 5 | 0 | 0 | 362,871 | 178,688 (49%) | 18s (was 45s) | Exactly one unit cleared the gate - the one holding apps/web/pages/api/webhook/app-credential.ts (credential + api path, score 82), itself a golden. Quality unchanged vs ungated run (0/5, 0 FP, 0 candidates), confirming the dropped lens sessions were not contributing. |

Cost picture: total tokens fell 940k -> 367k from the gate alone, and 49% of
the remaining input was served from OpenAI's prompt cache (now visible as
`cachedInputTokens`), so billed input cost is roughly a quarter of what the
raw 940k from the ungated run suggested.

## Challenge pass live exercise (cal-pr-14943)

Generated 2026-06-12 after the file-verdict coverage invariant and the
explicit `quality_pass_mode` flag landed.

| PR | Model | Findings | Golden hits | Scorer FPs | Confidence | Challenge outcome |
| --- | --- | ---: | ---: | ---: | --- | --- |
| cal-pr-14943 | gpt-5.4-mini | 2 | 0 | 2 | 0.79 both (0.72 base + 0.07 confirm boost) | Both confirmed; `challengedBy` empty, nothing suppressed |

First live execution of the adjudication path: one finding from a unit
session, one from final synthesis, both passed through the challenger and
received the confirmation boost - the wiring works end to end on a real
model. Caveats: both findings describe the retryCount cleanup behavior the
golden set counts as false positives (the golden is the non-SMS cleanup
scope bug), and the single challenger confirmed them rather than refuting.
The refutation/suppression path has still only been exercised against
deterministic mocks; a majority-vote challenger panel is the known next
step if the challenge pass is to act as a precision filter rather than a
confidence annotator. 49% of input tokens were cache-served on this run.
