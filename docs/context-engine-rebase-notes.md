# Context-Engine Rebase Notes

Status: written 2026-06-12, after the multi-lens/adjudication/transcript work
landed on `main`'s lineage (PR #4). Read this before rebasing
`feat/context-engine`.

## Why this is the next quality lever

Recall on the review-quality goldens is provably retrieval-bound, not
reviewer-bound. On cal-pr-11059 (5 goldens, all cross-file return-shape
contract bugs), three very different configurations produced **zero candidate
findings**:

| Config | Sessions | Candidates | Hits |
| --- | ---: | ---: | ---: |
| Multi-lens ungated, gpt-5.4-mini | 33 | 0 | 0/5 |
| Score-gated lenses, gpt-5.4-mini | 13 | 0 | 0/5 |
| Local qwen3:8b via Ollama | 33 | 0 | 0/5 |

The reviewer of `packages/app-store/_utils/oauth/refreshOAuthTokens.ts` never
sees the callback callers' shape expectations as evidence, so no amount of
extra lenses or adjudication can recover the bug. That caller/callee evidence
expansion is exactly what `feat/context-engine` builds (AST-aligned chunks,
BM25 + semantic fusion, resolved reference graph, typed structural ranking).

## The collision, precisely

`feat/context-engine` branched from `13cb4ad` and rewrites
`src/runtime/planned_units.rs` (+1,529/−220 at last measure). Since that
fork point, `main`'s lineage has substantially changed the same file:

- **Concurrent unit fan-out** in `run_with_cancel`: JoinSet bounded by the
  shared `max_active_sessions` semaphore, keyed `(unit_index, lens_index)`,
  reports re-sorted for determinism.
- **Multi-lens sessions**: `unit_lens_template_indices` (distinct-role
  templates, capped at 3, gated on `high_risk && score_max >= 80`),
  role-suffixed session ids (`unit-003#security`), `lens_focus` paragraphs
  appended to the unit system prompt, secondary lenses excluded from file
  reviews.
- **`run_unit` signature** now takes `lens_index` and `template_index`.
- **Adjudication**: `agreement_confidence` over `discovered_by` in
  `synthesize_findings`, plus `run_finding_challenge_pass` (populates
  `challenged_by`, suppresses refuted findings, skipped by
  `reconcile_file_reviews_with_findings`).
- **Quality gates** route through `RuntimeLimits::quality_pass_mode`
  (`Auto` keeps the legacy objective-phrase heuristic).
- **Prompt budget enforcement** (`runtime::transcript::enforce_prompt_budget`)
  is called before every model turn.
- **Coverage invariant**: `append_unverdicted_file_reviews` guarantees every
  assigned file ends with a verdict.

Adjacent new modules the rebase must respect:

- `src/runtime/transcript.rs` — evicts oldest tool-result payloads over
  budget. Anything that injects large evidence into transcripts should expect
  eviction to rewrite old tool results.
- `src/runtime/assembly.rs` — per-session incremental message rendering.
  Its fingerprints assume transcripts are append-only except eviction; new
  in-place transcript mutations must extend `item_fingerprint` or they will
  serve stale cached messages.
- `src/runtime/model_anthropic.rs` — cache breakpoints rely on tool_result
  blocks serializing byte-identically across turns.

Recommended shape: land context-engine's retrieval as an *input* to
`bootstrap_unit_evidence` and the unit transcript (better evidence into the
existing loop) rather than re-rewriting the loop itself. The loop's
concurrency, lensing, and adjudication are now tested behavior worth keeping.

## Ranking: confidence, not hops

The context graph has a known hub-effect problem: hop-count-based ranking
inflates hub files (barrel exports, shared utils) because everything is few
hops from them, and it ignores how confident we are in each edge/finding.
Two requirements for the rebased engine:

1. Evidence/edge ranking should consume confidence signals — including the
   now-real `FindingV1.confidence` (agreement- and challenge-derived) — not
   raw hop distance.
2. Gate each new edge source (imports, call resolution, semantic similarity)
   individually behind a review-quality bench run, so a noisy source can't
   silently regress precision.

## Verification recipe

Before and after the rebase, run the production bench on the three golden
PRs and compare hits/false positives/candidates:

```sh
cargo build --release --bin muzen
MODEL=gpt-5.4-mini node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com --pr 11059 --runner-path target/release/muzen
# likewise --pr 8330 and --pr 14943
```

Success criterion for the engine: the cal-pr-11059 unit reviewing
`refreshOAuthTokens.ts` should surface at least one callback caller's
expected credential shape in its evidence, and candidates > 0. `/tmp` bench
worktrees rot (macOS deletes old files, e.g. `.git/HEAD`); if `git fetch`
fails with "not a git repository", delete the worktree and let the harness
re-clone.
