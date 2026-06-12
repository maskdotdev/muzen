# Context Engine Audit - 2026-06-12

## Verdict

Current Context Engine is good, not yet great.

It is strongest as a deterministic, trust-aware retrieval primitive: evidence has provenance, budget accounting is explicit, redaction/injection checks pass, and candidate recall is already high. Main weakness is ranking/packing: relevant candidates usually exist, but too many are selected too late or omitted under budget.

## Current Default Metrics

Source: latest public-CLI proof run `/tmp/context-next-layout-full.json`, 91 deterministic cases.

Overall:

- recall@10: `0.5494`
- nDCG@10: `0.3884`
- recall@25: `0.6728`
- candidate recall: `0.9936`
- candidate-present miss rate: `0.2334`
- first relevant rate: `0.9341`
- tokens to first relevant: `1581`
- useful evidence per 1k tokens: `1.6603`
- sufficiency insufficient when incomplete: `1.0`

External corpus:

- recall@10: `0.3666`
- nDCG@10: `0.2273`
- recall@25: `0.4672`
- candidate recall: `0.9917`
- candidate-present miss rate: `0.4559`
- first relevant rate: `0.8500`
- tokens to first relevant: `2538`
- useful evidence per 1k tokens: `0.1521`

This shape matters: indexing usually finds ground truth, but pack order and budget still fail too often, especially outside this repo.

Truth-source split:

- curated: 4 cases, recall@10 `1.0000`, nDCG@10 `0.5324`, candidate-present miss `0.0000`, tokens to first relevant `182`
- fixture: 8 cases, recall@10 `1.0000`, nDCG@10 `1.0000`, candidate-present miss `0.0000`, tokens to first relevant `52`
- mined follow-up: 79 cases, recall@10 `0.4810`, nDCG@10 `0.3192`, candidate-present miss `0.2472`, tokens to first relevant `1825`

This split is now part of the gate. Fixture/security cases prove basic behavior stays intact, curated strict cases prove causal behavior without future labels, and mined follow-up cases remain the hard stress set that cannot be averaged away.

Signal and optimizer ablations:

Source: `bench/results-context-engine/context-engine-ablation-summary.json`; pack-repair row from `/tmp/context-pack-repair-ablation-report.json`; optimizer rows from `/tmp/context-optimizer-ablation-report.json` and `/tmp/context-rank-token-ablation-report.json`. Each row disables one signal or optimizer component through the same public CLI with `--ablate-context-signal`.

| Disabled component | recall@10 delta | nDCG@10 delta | present-miss delta | tokens-to-first delta |
| --- | ---: | ---: | ---: | ---: |
| graph | `-0.1028` | `-0.0833` | `+0.0702` | `+541` |
| co-change | `-0.0651` | `-0.0586` | `+0.0702` | `+335` |
| test coverage | `-0.0410` | `-0.0331` | `+0.0070` | `+467` |
| lexical change | `-0.0236` | `-0.0008` | `+0.0105` | `+22` |
| path proximity | `-0.0146` | `-0.0040` | `+0.0140` | `+84` |
| rank diversity | `-0.0458` | `-0.0200` | `+0.0383` | `+268` |
| pack path diversity | `+0.0000` | `+0.0000` | `+0.0488` | `+102` |
| skeleton reserve | `+0.0000` | `+0.0000` | `+0.0209` | `+92` |
| pack repair | `+0.0000` | `+0.0000` | `+0.0035` | `+0` |
| token efficiency | `+0.0288` | `+0.0172` | `+0.0279` | `+70` |
| semantic change | `+0.0000` | `+0.0000` | `+0.0000` | `+0` |

This proves graph and co-change are carrying real retrieval value across repos. Rank diversity carries rank quality, omission pressure, and early usefulness. Test coverage is valuable for rank/order but not candidate-present misses. Pack path diversity and skeleton reserve measurably reduce budget omissions and earlier useful context without changing top-rank metrics. Pack repair measurably reduces budget omissions without changing rank metrics. Token efficiency is mixed: disabling it improves recall/nDCG, but it worsens omission pressure and tokens-to-first-relevant enough that removal fails the gate. Path and lexical signals are mixed: they improve recall slightly, but delay the first relevant item less when removed, so weights need tuning. Semantic-change is inert in the default no-vector tier, as expected.

## What Is Already Strong

- Typed evidence with trust, sensitivity, provenance, redaction.
- Deterministic chunking, graph expansion, BM25-style lexical index, semantic hooks, skeleton fallback, omission records.
- Context Graph includes imports/references/tests/co-change/same-module/feature-slice facts with bounded expansion.
- Sufficiency is honest: incomplete packs do not claim sufficient.
- Multi-repo benchmark exists and runs through public CLI, not internal test hooks.
- Real semantic tiers exist: hosted `text-embedding-3-small` and local ONNX `jina-embeddings-v2-base-code` previously improved recall/nDCG versus no-vector baseline.
- The pack compiler now gives first-pass priority to one full-content item per path before spending tail budget on duplicate chunks. This reduced candidate-present misses from `0.2903` to `0.2509` overall and from `0.5147` to `0.4779` on external cases without changing recall@10/nDCG@10.
- The eval harness now labels truth source as `fixture`, `mined_followup`, or `curated`, reports cohorts, includes truth source on weak cases, and gates fixture/mined performance separately.
- Public signal/optimizer ablation now exists through `muzen context --ablate-context-signal ...`; the bench harness can pass it through and write ablation deltas without hidden hooks.
- Strict curated fixture `curated-checkout-flow` now proves changed checkout logic retrieves direct API callers and route tests through import graph facts under a 4k pack budget.
- Strict curated fixture `curated-python-billing` now proves Python import graph facts retrieve a changed settlement module, API caller, and API test under a 3.5k pack budget despite unrelated payment/API/test distractors.
- Strict curated fixture `curated-rust-invoice` now proves Rust module import graph facts retrieve a changed settlement module's API caller and integration test under a 500-token pack budget with refund/inventory distractors.
- Strict curated fixture `curated-doc-contract` now proves explicit Markdown doc links retrieve linked implementation and test files under a 1.2k pack budget with adjacent contract/runtime/test distractors.
- Eval iteration can now run the same public CLI/gate in parallel with `--jobs N`. Same derived-cache root is serialized per repo to avoid cache write races, and result ordering stays stable for committed artifacts. Latest proof run: 91 cases with `--jobs 6` passed after document-link graph expansion.
- Eval iteration now builds the default `muzen` binary once when `--muzen-bin` is omitted, then reuses `target/debug/muzen` for every case. This preserves public-CLI proof while removing repeated `cargo run` overhead from default runs.
- Eval iteration now rejects stale `target/debug/muzen` binaries when `--muzen-bin` is provided explicitly and Rust build inputs are newer. Full gates therefore prove the current implementation, not an accidentally stale local build.
- Eval summaries now include run metadata for the evaluated binary, binary mtime, git head, git dirty flag, and whether local binary freshness was checked. Metric artifacts are now self-identifying enough to audit later.
- Eval run metadata now records semantic tier/model settings, local ONNX model directory, rerank endpoint/model settings, and active ablations without recording credentials. This makes "which model did this gate use?" auditable from the artifact; default deterministic gates report `forcedTier: none`.
- Eval iteration now supports focused `--case-id` and `--case-glob` diagnostic runs. Filtered runs are marked diagnostic-only, skip regression gates, and cannot write `baseline.json`, so speed cannot masquerade as proof.
- Eval iteration now has a deterministic summary comparator for experiment artifacts, so metric and case-level deltas can be inspected without ad hoc scripts while preserving the full-gate requirement.
- Eval summaries now report the slowest public-CLI cases with latency, token estimate, omitted count, recall, truth source, and candidate-present miss count. This makes speed work auditable without reducing coverage; the latest slowest cases are large external mined packs with roughly 7.9k omitted candidates.
- Weak cases now include omitted-candidate diagnostics for candidate-present misses: evidence id, kind, path, score, rank index, token estimate, and omission reason. This turns "candidate existed but missed" from a vague ranking failure into a concrete proof target such as `budget_exhausted`.
- Packs now expose selected-candidate score and rank metadata, and weak cases report the selected tail beside missed candidates with graph paths where relationships exist. This makes budget tradeoffs auditable: we can see which low-tail items consumed the budget that excluded a relevant candidate and whether those tail items had strong structural support.
- Runtime explain-pack diagnostics now also include graph paths for omitted candidates when `includeOmitted` is requested. This lets us audit why graph-connected evidence lost the budget fight instead of only explaining selected evidence.
- Bench weak-case summaries now preserve omitted-candidate graph paths from runtime explain-pack output. Latest 91-case run had `258` weak-case omitted details and `0` graph paths on those misses, which means current worst misses are candidate-found but not source-backed by graph paths.
- Eval summaries now aggregate omission pressure: candidate-present omission count, reason split, graph-path coverage, score/rank/token stats, and cases where a missed expected candidate outscored the selected tail. This makes pack-optimizer tradeoffs visible at summary level instead of requiring ad hoc scripts.
- Context Graph now includes source-backed `Documents` edges from explicit Markdown/RST document links to existing repo files. The resolver handles relative links, absolute workspace suffixes, fragments, queries, and line suffixes, and rejects ambiguous suffix links instead of guessing.
- Context Graph now includes source-backed Next App Router layout edges from ancestor `layout.*` files to changed `app/**/{page,route,...}.*` leaves. The edge is scoped to changed app-route leaves and capped at four ancestor layouts, which reduced candidate-present miss `0.2404 -> 0.2334` and external candidate-present miss `0.4706 -> 0.4559` with recall/nDCG/tokens unchanged.
- The pack compiler has a narrow budget-repair pass that may replace only low-confidence full-content tail evidence with a higher-scoring budget-exhausted candidate when score, token, path-limit, and protected-evidence invariants hold. Broad repair was rejected; the narrowed form preserved all external metrics and slightly improved mean per-case candidate-present miss rate and precision.
- The pack compiler now has a second, narrower skeleton-tail repair: when full-content reserve has room but total budget is blocked by low-value skeleton breadth, a budget-exhausted full-content candidate can replace skeletons only if it adds a new path and clears a score-margin check. This reduced candidate-present miss `0.2473 -> 0.2438` overall and `0.4779 -> 0.4706` on external cases while preserving recall@10, nDCG@10, recall@25, self metrics, and tokens to first relevant.
- Public optimizer ablations now cover rank diversity, pack path diversity, skeleton reserve, pack repair, and token efficiency. Latest full public-CLI proof: disabling rank diversity regressed recall@10 `-0.0458`, nDCG@10 `-0.0200`, candidate-present miss `+0.0383`, and tokens-to-first-relevant `+268`; disabling pack path diversity worsened candidate-present miss `+0.0488` and tokens-to-first-relevant `+102`; disabling skeleton reserve worsened candidate-present miss `+0.0209` and tokens-to-first-relevant `+92`; disabling token efficiency improved recall/nDCG but worsened candidate-present miss `+0.0279` and tokens-to-first-relevant `+70`.

## Main Gaps

1. **External rank quality.** External recall@10/nDCG are too low. Candidate recall says this is not mostly an indexing problem.
2. **Pack optimizer.** First-pass path diversity helps, but pack selection is still not a true bounded optimizer over utility, tokens, skeletons, and path coverage.
3. **Strict causal labels.** Four curated strict causal cases now exist across TypeScript, Python, Rust, and doc-to-code contract links, but most hard cases still use future commits as useful stress labels. Need more curated causal cases across frameworks and languages.
4. **Framework context.** Next App Router layout edges now cover one strong framework contract. Route/app-shell/shared-store relationships still need source-backed facts; previous broad path-convention attempts added noise.
5. **Semantic default proof.** Real embeddings improve metrics, but default deterministic path remains no-vector. Need a clear quality-tier story: no-vector baseline, local ONNX private tier, hosted/rerank best tier.
6. **Optimizer proof.** Signal ablations identify useful inputs, and public optimizer ablations now cover rank diversity, path diversity, skeleton reserve, repair, and token efficiency. Need remaining proof for true bounded allocation versus greedy selection.

## Proof Standard

Any retained change must:

- Improve or preserve gated deterministic metrics overall and for `pack`, `external`, and `self` cohorts.
- Improve or preserve `fixture`, `curated`, and `mined_followup` truth-source cohorts separately.
- Not worsen candidate-present miss rates beyond tight tolerance.
- Not reduce useful evidence density beyond tolerance.
- Pass redaction, prompt-injection, sufficiency, and full Rust/Python tests.
- Be general: no repo/path/case-specific rules.

The eval gate now tracks:

- recall@5, recall@10, recall@25, nDCG@10
- candidate recall
- candidate-present miss rate, miss case rate, mean per-case miss rate
- first relevant rate and tokens to first relevant
- useful evidence per 1k tokens
- sufficiency honesty
- truth-source cohorts and weak-case truth source
- candidate-present missed omitted candidates with reason/score/rank/token diagnostics
- selected-tail score/rank/token/graph-path diagnostics for weak cases
- aggregate omission-pressure diagnostics
- semantic/rerank/ablation run metadata
- public CLI signal and optimizer ablations for proof runs
- summary-to-summary metric and case-delta comparison for diagnostic artifacts

Recent rejected experiments:

- Diff-body lexical anchors: focused weak case improved (`recall@25 0 -> 0.5`), but broad pack diagnostics regressed recall@10/nDCG and increased noisy top-rank churn. Raw diff text, lower raw weight, path-like-only text, and lower path-like weight were all rejected.
- Changed-summary lexical anchors: focused weak case improved (`recall@25 0 -> 0.5`), but broad pack diagnostics rejected both summary-head and symbol-only variants. Full summary heads regressed pack recall@10 to `0.381` and nDCG@10 to `0.277`; symbol-only heads at weight `2.0` regressed recall@10 to `0.385` and candidate-present miss to `0.296`; symbol-only heads at weight `0.25` still regressed recall@10 to `0.427`, nDCG@10 to `0.302`, and candidate-present miss to `0.288`.
- Path proximity weight `0.05 -> 0.07`: improved recall@5 and tokens slightly, but candidate-present misses did not improve and recall@10/25 dipped.
- Default local hash semantic mode: recall@25 and precision moved slightly positive, but recall@10/nDCG dropped and cold latency increased substantially.
- Smaller skeleton reserve and high-confidence reserve borrowing: both improved isolated candidate-present misses but worsened tokens to first relevant and did not lift top-25 ranking enough to justify retention.
- Compact full-content reserve: candidate-present miss improved `0.2509 -> 0.247`, but tokens to first relevant regressed `1650 -> 1846` and external regressed `2538 -> 2941`.
- Path proximity weight `0.05 -> 0.03`: failed self recall@10 gate (`0.5983 -> 0.5756`).
- Co-change weight `0.15 -> 0.20`: recall@25 and ttfr moved slightly positive, but self recall and present-miss case rates regressed.
- Test-coverage weight `0.30 -> 0.35`: rank stayed mostly flat, present-miss rates worsened.
- Test-density frontier `6 -> 3`: exact `*-pack` diagnostics improved nDCG@10 slightly (`0.329 -> 0.331`) but regressed recall@25 (`0.632 -> 0.626`) and candidate-present miss (`0.252 -> 0.259`), so broad test coverage remains first-class.
- Token efficiency bonus bump: tokens to first relevant improved `1650 -> 1607`, but recall@25/self and external present-miss regressed.
- Token efficiency downscale: halving the bonus improved recall@10 (`0.549 -> 0.571`) and nDCG@10 (`0.388 -> 0.398`), but failed omission gates with candidate-present miss `0.254`, mined-follow-up miss `0.269`, and self present-miss `0.089`, so the current stronger budget-efficiency bias stays.
- Compact full-content reserve cap `200`: no quality gain; tokens to first relevant worsened to `1730`, and external tokens to first relevant failed gate at `2745`.
- Broad budget-repair swapping: fixed one self case but introduced one external candidate-present miss and failed external present-miss gate (`0.4779 -> 0.4853`); narrowed to low-confidence-tail evictions only.
- Broad skeleton-tail swapping: improved candidate-present miss `0.2473 -> 0.2403`, but external tokens to first relevant regressed `2538 -> 2790` and self candidate-present misses worsened. A score-margin-only version still failed (`external ttfr 2779`, self miss regressions). Final retained rule requires new path coverage plus score margin.
- Skeleton-tail repair margin `0.10 -> 0.05`: exact `*-pack` diagnostics improved candidate-present miss `0.2518 -> 0.2482` with flat recall/nDCG, but the full gate failed external tokens-to-first-relevant (`2747.2 > 2537.8 + 128`), so the stricter margin stays.
- Skeleton-tail repair margin `0.10 -> 0.00`: full gate improved candidate-present miss (`0.233 -> 0.223`) but failed external tokens-to-first-relevant (`2748.1 > 2537.8 + 128`). Late inclusion without earlier rank is not enough, so the stricter margin stays.
- Skeleton single-eviction repair: fixed the obvious local objective mismatch by trying the lowest-total-score single skeleton eviction before score-density multi-eviction. It improved overall candidate-present miss (`0.242 -> 0.235`) and fixed two external budget misses, but the full gate failed external tokens-to-first-relevant (`2742.3 > 2537.8 + 128`) because one added first relevant item landed around `9.6k` tokens. Late recall is not enough; pack repair must optimize early usefulness too.
- Rank-later reserve borrowing: tried allowing strong full-content candidates to borrow from the skeleton reserve only by evicting lower-score skeletons ranked after that candidate, with skeleton utility discounted. Focused weak-pack diagnostics showed no improvement in recall, nDCG, candidate-present misses, or tokens-to-first-relevant, so the change was discarded before full-gate promotion.
- Rank-later repair eviction guard: tried allowing budget repair to evict only selected evidence ranked after the omitted candidate. Exact weak-pack diagnostics and all `*-pack` diagnostics showed zero metric or case-level deltas, so the added complexity was discarded.
- Graph proximity weight `0.20 -> 0.25`: focused weak-pack diagnostics did not improve recall@10, nDCG@10, or recall@25 and worsened candidate-present miss to `0.757`, so graph rank needs better facts rather than a broad weight bump.
- Score-aware graph proximity: using graph path score instead of hop distance improved a few focused weak rows by moving first relevant evidence earlier, but the 91-case full gate failed recall@10 (`0.502 < 0.544`), nDCG@10 (`0.363 < 0.385`), tokens-to-first-relevant (`1917 > 1598 + 128`), and self/external cohort gates. Weak same-module paths are noisy, but current distance-only graph proximity is still carrying broad top-rank value.
- Rare shared-dependency graph edges: tried terminal lateral edges between production files importing the same low-fan-in local dependency. Focused weak-pack diagnostics regressed recall@25 to `0.000`, candidate-present miss to `0.700`, and tokens-to-first-relevant to `7074`, so shared imports are too noisy without a stronger contract signal.
- Next route-scoped layout edges: tried extending layout `Configures` edges from changed app route leaves to app-scoped modules import-connected to changed code. Edge-level tests passed, but focused app weak-pack diagnostics showed no recall/nDCG/top-25 gain and slightly worsened one first-relevant position, so this needs a stronger route/render contract before retention.

## Next Work To Reach Great

1. **Budget-aware pack optimizer.** Extend first-pass path diversity into deterministic bounded selection over score, token cost, representation, and path coverage. Prove against candidate-present miss, recall@25, and useful/1k.
2. **Graph edge quality over quantity.** Add framework conventions only when source facts are strong: import/export evidence, config manifests, route segment semantics, package exports. No generic path buckets.
3. **Semantic quality tier.** Make local ONNX/hosted semantic eval first-class in CI-like reports, even if not default CI. Add rerank acceptance when credentialed endpoint exists.
4. **Curated causal eval.** Add more strict causal cases with hand-verified expected evidence, then gate curated harder than mined follow-up stress cases.
5. **Weight tuning from ablations.** Keep graph/co-change/test coverage strong, tune path/lexical weights against recall, first-relevant latency, and present-miss cohorts.
6. **Iterative sufficiency loop.** Use sufficiency gaps to pull missing spans/tests after initial pack, then measure review-ready completeness under same budget.
7. **No-shortcut eval speed.** Keep full public-CLI gates, use filtered diagnostic runs only for local diagnosis, and pursue longer-term batch/index-cache work so repeated ablations do not rebuild identical repo state.

## Current Bottom Line

Good enough to be useful. Not yet SOTA. Path to SOTA is not more hand-tuned heuristics; it is better pack optimization, cleaner graph facts, real semantic/rerank tiers, and stricter proof.
