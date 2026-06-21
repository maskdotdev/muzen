# Current Loop Benchmark Results

## Shared/process runner fake-proxy investigation

Generated on 2026-06-21 with `target/release/muzen-runner` while diagnosing
shared/concurrent runner behavior before any further subscription-backed evals.

The current fake-first gate sequence is:

```sh
node bench/review-quality/check-local.mjs \
  --runner-path target/release/muzen-runner \
  --include-codex-proxy true
```

That opt-in gate runs the standard fake local probes plus a Codex-proxy-shaped
retry probe. The proxy probe starts the real local Codex Responses proxy with a
temporary fake auth file, points it at the fake Responses server, and exercises
the same `OPENAI_BASE_URL` path used by live subscription evals without making
live model calls.

Latest `check-local --include-codex-proxy true` result:

| Probe | Shared findings | Process findings | Shared model calls | Process model calls | Shared tool calls | Process tool calls | Shared provider errors | Process provider errors | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| codex-proxy-deterministic-retry | 5 | 5 | 46 | 46 | 10 | 10 | 21 | 21 | passed |

Stress sweep:

```sh
node bench/review-quality/tools/run-fake-runner-mode-sweep.mjs \
  --runner-path target/release/muzen-runner \
  --concurrency 5,8,12 \
  --cases 12 \
  --max-tool-calls 6 \
  --max-turns 10 \
  --tools-before-final 1 \
  --http-error-attempts-per-request 1 \
  --final-mode candidate \
  --latency-ms 25 \
  --jitter-ms 0 \
  --max-concurrent 1 \
  --via-codex-proxy true
```

`run-fake-runner-mode-sweep.mjs` exits nonzero by default if it reports
parity, release, or isolation regressions. Use `--fail-on-regression false`
only for exploratory chaos probes where a failing JSON report is the desired
artifact.

| Concurrency | Findings shared/process | Model calls shared/process | Tool calls shared/process | Tokens shared/process | Fake 500s shared/process | Retry backoff ms shared/process | Provider queue mean delta ms | Provider queue p95 delta ms | Result |
| ---: | --- | --- | --- | --- | --- | --- | ---: | ---: | --- |
| 5 | 12 / 12 | 109 / 109 | 24 / 24 | 1080 / 1080 | 49 / 49 | 22060 / 22071 | 0.14 | 2 | passed |
| 8 | 12 / 12 | 109 / 109 | 24 / 24 | 1080 / 1080 | 49 / 49 | 22044 / 22058 | -0.10 | 0 | passed |
| 12 | 12 / 12 | 109 / 109 | 24 / 24 | 1080 / 1080 | 49 / 49 | 22061 / 22068 | -3.12 | -1 | passed |

Interpretation:

- The deterministic fake/proxy path does not reproduce a shared-only
  concurrency regression. Shared and process modes match on findings, model
  calls, tool calls, token totals, retry counts, and terminal completion across
  queue pressure, candidate publication, schema repair, and proxy-shaped retry.
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

Current recommendation: do not run live evals until the proxy-shaped fake gate
is clean on the branch under test. If a live eval is needed after this point, it
should be a deliberately approved, small runner-mode diagnostic through the
subscription proxy with semantic scoring disabled first.

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
