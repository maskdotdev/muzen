# Context Engine Audit - 2026-06-12

## Verdict

Current Context Engine is good, not yet great.

It is strongest as a deterministic, trust-aware retrieval primitive: evidence has provenance, budget accounting is explicit, redaction/injection checks pass, and candidate recall is already high. Main weakness is ranking/packing: relevant candidates usually exist, but too many are selected too late or omitted under budget.

## Current Default Metrics

Source: `bench/results-context-engine/context-engine-summary.json`, 90 deterministic cases.

Overall:

- recall@10: `0.5444`
- nDCG@10: `0.3850`
- recall@25: `0.6692`
- candidate recall: `0.9934`
- candidate-present miss rate: `0.2421`
- first relevant rate: `0.9326`
- tokens to first relevant: `1598`
- useful evidence per 1k tokens: `1.5891`
- sufficiency insufficient when incomplete: `1.0`

External corpus:

- recall@10: `0.3666`
- nDCG@10: `0.2273`
- recall@25: `0.4672`
- candidate recall: `0.9917`
- candidate-present miss rate: `0.4706`
- first relevant rate: `0.8500`
- tokens to first relevant: `2538`
- useful evidence per 1k tokens: `0.1479`

This shape matters: indexing usually finds ground truth, but pack order and budget still fail too often, especially outside this repo.

Truth-source split:

- curated: 3 cases, recall@10 `1.0000`, nDCG@10 `0.4787`, candidate-present miss `0.0000`, tokens to first relevant `202`
- fixture: 8 cases, recall@10 `1.0000`, nDCG@10 `1.0000`, candidate-present miss `0.0000`, tokens to first relevant `52`
- mined follow-up: 79 cases, recall@10 `0.4810`, nDCG@10 `0.3192`, candidate-present miss `0.2546`, tokens to first relevant `1825`

This split is now part of the gate. Fixture/security cases prove basic behavior stays intact, curated strict cases prove causal behavior without future labels, and mined follow-up cases remain the hard stress set that cannot be averaged away.

Signal ablations:

Source: `bench/results-context-engine/context-engine-ablation-summary.json`. Each row disables one signal through the same public CLI with `--ablate-context-signal`.

| Disabled signal | recall@10 delta | nDCG@10 delta | present-miss delta | tokens-to-first delta |
| --- | ---: | ---: | ---: | ---: |
| graph | `-0.1028` | `-0.0833` | `+0.0702` | `+541` |
| co-change | `-0.0651` | `-0.0586` | `+0.0702` | `+335` |
| test coverage | `-0.0410` | `-0.0331` | `+0.0070` | `+467` |
| lexical change | `-0.0236` | `-0.0008` | `+0.0105` | `+22` |
| path proximity | `-0.0146` | `-0.0040` | `+0.0140` | `+84` |
| semantic change | `+0.0000` | `+0.0000` | `+0.0000` | `+0` |

This proves graph and co-change are carrying real retrieval value across repos. Test coverage is valuable for rank/order but not candidate-present misses. Path and lexical signals are mixed: they improve recall slightly, but delay the first relevant item less when removed, so weights need tuning. Semantic-change is inert in the default no-vector tier, as expected.

## What Is Already Strong

- Typed evidence with trust, sensitivity, provenance, redaction.
- Deterministic chunking, graph expansion, BM25-style lexical index, semantic hooks, skeleton fallback, omission records.
- Context Graph includes imports/references/tests/co-change/same-module/feature-slice facts with bounded expansion.
- Sufficiency is honest: incomplete packs do not claim sufficient.
- Multi-repo benchmark exists and runs through public CLI, not internal test hooks.
- Real semantic tiers exist: hosted `text-embedding-3-small` and local ONNX `jina-embeddings-v2-base-code` previously improved recall/nDCG versus no-vector baseline.
- The pack compiler now gives first-pass priority to one full-content item per path before spending tail budget on duplicate chunks. This reduced candidate-present misses from `0.2903` to `0.2509` overall and from `0.5147` to `0.4779` on external cases without changing recall@10/nDCG@10.
- The eval harness now labels truth source as `fixture`, `mined_followup`, or `curated`, reports cohorts, includes truth source on weak cases, and gates fixture/mined performance separately.
- Public signal ablation now exists through `muzen context --ablate-context-signal ...`; the bench harness can pass it through and write ablation deltas without hidden hooks.
- Strict curated fixture `curated-checkout-flow` now proves changed checkout logic retrieves direct API callers and route tests through import graph facts under a 4k pack budget.
- Strict curated fixture `curated-python-billing` now proves Python import graph facts retrieve a changed settlement module, API caller, and API test under a 3.5k pack budget despite unrelated payment/API/test distractors.
- Strict curated fixture `curated-rust-invoice` now proves Rust module import graph facts retrieve a changed settlement module's API caller and integration test under a 500-token pack budget with refund/inventory distractors.
- Eval iteration can now run the same public CLI/gate in parallel with `--jobs N`. Same derived-cache root is serialized per repo to avoid cache write races, and result ordering stays stable for committed artifacts. Latest proof run: 90 cases with `--jobs 6` passed after baseline refresh.
- The pack compiler has a narrow budget-repair pass that may replace only low-confidence full-content tail evidence with a higher-scoring budget-exhausted candidate when score, token, path-limit, and protected-evidence invariants hold. Broad repair was rejected; the narrowed form preserved all external metrics and slightly improved mean per-case candidate-present miss rate and precision.
- The pack compiler now has a second, narrower skeleton-tail repair: when full-content reserve has room but total budget is blocked by low-value skeleton breadth, a budget-exhausted full-content candidate can replace skeletons only if it adds a new path and clears a score-margin check. This reduced candidate-present miss `0.2473 -> 0.2438` overall and `0.4779 -> 0.4706` on external cases while preserving recall@10, nDCG@10, recall@25, self metrics, and tokens to first relevant.

## Main Gaps

1. **External rank quality.** External recall@10/nDCG are too low. Candidate recall says this is not mostly an indexing problem.
2. **Pack optimizer.** First-pass path diversity helps, but pack selection is still not a true bounded optimizer over utility, tokens, skeletons, and path coverage.
3. **Strict causal labels.** Three curated strict causal cases now exist across TypeScript, Python, and Rust, but most hard cases still use future commits as useful stress labels. Need more curated causal cases across frameworks and languages.
4. **Framework context.** Route/layout/app-shell/shared-store relationships still need general graph edges, but previous path-convention attempts added noise.
5. **Semantic default proof.** Real embeddings improve metrics, but default deterministic path remains no-vector. Need a clear quality-tier story: no-vector baseline, local ONNX private tier, hosted/rerank best tier.
6. **Optimizer proof.** Signal ablations identify useful inputs, but not best allocation. Need optimizer ablations: greedy rank, path-diverse greedy, skeleton reserve, and bounded token utility.

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
- public CLI signal ablations for proof runs

Recent rejected experiments:

- Compact full-content reserve: candidate-present miss improved `0.2509 -> 0.247`, but tokens to first relevant regressed `1650 -> 1846` and external regressed `2538 -> 2941`.
- Path proximity weight `0.05 -> 0.03`: failed self recall@10 gate (`0.5983 -> 0.5756`).
- Co-change weight `0.15 -> 0.20`: recall@25 and ttfr moved slightly positive, but self recall and present-miss case rates regressed.
- Test-coverage weight `0.30 -> 0.35`: rank stayed mostly flat, present-miss rates worsened.
- Token efficiency bonus bump: tokens to first relevant improved `1650 -> 1607`, but recall@25/self and external present-miss regressed.
- Compact full-content reserve cap `200`: no quality gain; tokens to first relevant worsened to `1730`, and external tokens to first relevant failed gate at `2745`.
- Broad budget-repair swapping: fixed one self case but introduced one external candidate-present miss and failed external present-miss gate (`0.4779 -> 0.4853`); narrowed to low-confidence-tail evictions only.
- Broad skeleton-tail swapping: improved candidate-present miss `0.2473 -> 0.2403`, but external tokens to first relevant regressed `2538 -> 2790` and self candidate-present misses worsened. A score-margin-only version still failed (`external ttfr 2779`, self miss regressions). Final retained rule requires new path coverage plus score margin.

## Next Work To Reach Great

1. **Budget-aware pack optimizer.** Extend first-pass path diversity into deterministic bounded selection over score, token cost, representation, and path coverage. Prove against candidate-present miss, recall@25, and useful/1k.
2. **Graph edge quality over quantity.** Add framework conventions only when source facts are strong: import/export evidence, config manifests, route segment semantics, package exports. No generic path buckets.
3. **Semantic quality tier.** Make local ONNX/hosted semantic eval first-class in CI-like reports, even if not default CI. Add rerank acceptance when credentialed endpoint exists.
4. **Curated causal eval.** Add more strict causal cases with hand-verified expected evidence, then gate curated harder than mined follow-up stress cases.
5. **Weight tuning from ablations.** Keep graph/co-change/test coverage strong, tune path/lexical weights against recall, first-relevant latency, and present-miss cohorts.
6. **Iterative sufficiency loop.** Use sufficiency gaps to pull missing spans/tests after initial pack, then measure review-ready completeness under same budget.

## Current Bottom Line

Good enough to be useful. Not yet SOTA. Path to SOTA is not more hand-tuned heuristics; it is better pack optimization, cleaner graph facts, real semantic/rerank tiers, and stricter proof.
