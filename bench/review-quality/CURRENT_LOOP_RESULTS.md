# Current Loop Benchmark Results

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
