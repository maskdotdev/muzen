# Muzen Context Engine SOTA Retrieval Plan

Generated: 2026-06-10

Status: draft for review

## Decision

Upgrade the Context Engine retrieval core from deterministic V0 heuristics to
state-of-the-art retrieval quality while keeping the properties that
differentiate Muzen: typed evidence with trust and provenance, deterministic
replay, budget and omission accounting, capability scoping, and an evaluation
harness that gates every change.

The single design rule for this plan:

```text
Every retrieval change lands with a benchmark number.
The stronger behavior replaces the old behavior.
No second mode exists only to preserve V0 output.
```

This plan builds on the shipped `feat/context-engine` work described in
`implementation-plan.md`. It does not change the core interface
(`index_snapshot`, `build_pack`, `query`, `record_feedback`), the artifact
contracts, or the trust/sensitivity policy. It changes what happens behind
that interface.

## Current State Assessment

What is already strong and must not regress:

- Trust-ranked, provenance-carrying evidence (`ContextTrust`,
  `ContextProvenance`, sensitivity, redaction).
- Capability-scoped cross-repo evidence with explicit denial reporting.
- Deterministic snapshot indexing, pack budgets, omission records, and
  `explain_pack`.
- A runnable benchmark harness (`bench/context-engine/run.py`) with recall,
  precision, token efficiency, redaction, and prompt-injection metrics.

Where retrieval quality is below state of the art:

1. **Evidence granularity.** `src/context_engine/index.rs` emits one evidence
   item per file with `token_estimate = file_size / 4`. Packs select whole
   files. Symbol evidence carries a name and range but no body. SOTA systems
   retrieve AST-aligned function/class chunks (cAST-style structural
   chunking).
2. **Lexical search.** `search_evidence` in
   `src/context_engine/retrieval.rs` is OR-of-substrings over
   path + summary + content with no scoring; results are index-ordered and
   truncated. SOTA baseline is BM25 over code-aware tokens.
3. **Hybrid fusion.** `merge_semantic_search` appends semantic hits after
   lexical hits and only accepts a semantic hit when the candidate text
   lexically contains a query term. That gate filters out exactly the
   paraphrase matches semantic retrieval exists to find. SOTA fusion is
   rank-based (Reciprocal Rank Fusion), with optional cross-encoder
   reranking of the fused candidates.
4. **Local embeddings.** `LocalHashEmbeddingProvider` is a 256-dim hashed
   bag of words. It is a deterministic test fixture, not a semantic ranker.
5. **Symbol graph.** `ContextSymbolGraph` maps bare symbol-name strings to
   importers, so `get` in one module collides with `get` everywhere. There is
   no enclosing-definition lookup for diff hunks, no caller/callee expansion,
   and no co-change signal.
6. **Ranking.** `score_for_purpose` in `src/context_engine/pack.rs` is a
   static kind/purpose bonus table. The changed-file bonus is parsed from the
   summary string (`summary.contains("changed")`), which is fragile. Ranking
   ignores the actual content of the change.
7. **Sufficiency.** `SufficiencyCheck` in `src/context_engine/engine.rs` is
   `empty -> insufficient`, otherwise `probably_sufficient`. It cannot drive
   iterative retrieval.
8. **Token accounting.** `estimate_tokens` divides byte length by four and is
   applied to whole-file sizes, so budget decisions are made against inflated
   estimates.
9. **Evaluation corpus.** Two fixtures (`simple-auth`, `security-guidance`)
   cannot detect ranking regressions or justify ranking changes.
10. **Index lifecycle.** `ContextIndex::build` runs from scratch per
    snapshot. Content hashes exist on every evidence item but nothing is
    cached across runs.

## Goals

- Retrieval unit becomes the AST-aligned chunk, not the file.
- Lexical retrieval becomes scored BM25 with code-aware tokenization.
- Lexical and semantic candidates fuse by rank, not by append-with-gate.
- Diff hunks anchor retrieval: enclosing definitions, callers, tests, and
  co-changed files form a change-rooted candidate set.
- Ranking uses structural signals (graph distance, co-change frequency, path
  proximity) carried as typed fields, not summary-string parsing.
- Sufficiency becomes structural and per-hunk, and can drive iterative
  retrieval through the existing query tools.
- Every phase is gated by the expanded benchmark suite with recall@k, nDCG,
  and token-efficiency deltas reported in the PR.
- Determinism is preserved in the default (no-vector and local) modes.

## Non-Goals

- Do not change the `ContextEngine` trait, artifact schema versions bump only
  when payload shape actually changes.
- Do not weaken trust, sensitivity, redaction, or capability scoping for any
  retrieval gain.
- Do not require embeddings, network access, or model calls in the default
  path.
- Do not keep the V0 substring search or the V0 file-level retrieval unit as
  a fallback mode once their replacements pass the benchmark gate.
- Do not build or operate an external vector database.
- Do not add learned ranking weights before the evaluation corpus is large
  enough to validate them.

## Target Architecture

```text
RepoSnapshot + diff
  -> chunker (AST-aligned, tree-sitter)        [Phase R1]
  -> chunk evidence + skeleton evidence        [Phase R1, R7]
  -> BM25 inverted index (code tokens)         [Phase R2]
  -> optional vector index (existing traits)   [unchanged]
  -> change anchors: hunk -> enclosing defs    [Phase R4]
  -> blast radius: callers, callees, tests,
     co-changed files                          [Phase R4]
  -> candidate generation:
       BM25 + vectors -> RRF fusion            [Phase R3]
       + change-rooted graph expansion         [Phase R4]
  -> ranking: purpose bonuses + structural
     signals (graph distance, co-change,
     path proximity)                           [Phase R5]
  -> pack compiler (existing budgets/omissions)
  -> structural sufficiency per hunk           [Phase R6]
  -> iterative retrieval loop via context.*    [Phase R6]
```

The benchmark suite expansion (Phase R0) precedes and gates all of the above.

## Phase Order And Dependencies

```text
R0 (eval corpus)          gates everything
R1 (chunk evidence)       -> R2, R5, R7
R2 (BM25)                 -> R3
R3 (RRF fusion)           -> R8
R4 (change-rooted graph)  -> R5, R6
R5 (ranking signals)      requires R1, R4
R6 (structural sufficiency) requires R4
R7 (skeletons + tokens)   requires R1
R8 (real embeddings, rerank) requires R3, optional
R9 (incremental indexing) independent, last
```

## Implementation Phases

### Phase R0: Evaluation Corpus Expansion

Intent:

- Make every later phase measurable. Two fixtures cannot detect ranking
  regressions. Build a corpus where ground truth comes from real changes.

Files:

- `bench/context-engine/run.py`
- `bench/context-engine/cases/`
- `bench/context-engine/corpus/` (new)
- `scripts/` (new corpus mining script)

Work items:

- Add a corpus miner that converts merged PRs into evaluation cases:
  - Input: a repository, a merge commit, and the follow-up fix or review
    commits that touched the same area.
  - Ground truth: the file paths and line spans the fix/review actually
    touched, minus the spans already in the diff under review.
  - Output: a case JSON with `repo`, `changedFiles`, `expectedPaths`,
    `expectedRanges`, and `query` entries.
- Mine an initial corpus of at least 30 cases:
  - Muzen's own history (this repository).
  - Two or three permissive-license OSS repositories spanning Rust,
    TypeScript, and Python, vendored as fixtures or pinned by commit.
- Add ranking-aware metrics to `run.py`:
  - recall@5, recall@10, recall@25 against `expectedPaths` and
    `expectedRanges`.
  - nDCG@10.
  - tokens-to-first-relevant (budget position of the first ground-truth
    item in the compiled pack).
- Keep existing metrics: precision, token efficiency, omission accounting,
  redaction, prompt-injection trust, latency.
- Write per-phase baseline snapshots to
  `bench/results-context-engine/` so PRs can report deltas.
- Add a CI-friendly regression gate: the run fails when recall@10 or nDCG@10
  drops more than a configured tolerance against the committed baseline.

Tests:

- Corpus miner is deterministic for a pinned commit pair.
- A case with no ground-truth spans is rejected at load time.
- Metrics are stable across two consecutive runs in no-vector mode.

Acceptance:

- `python3 bench/context-engine/run.py` reports recall@k and nDCG@10 over at
  least 30 cases and fails on regression beyond tolerance.
- Every later phase PR includes the before/after summary JSON.

### Phase R1: AST-Aligned Chunk Evidence

Intent:

- Make the retrieval unit a self-contained, semantically coherent chunk
  (function, class, impl block, config section) instead of a whole file.
  This is the highest-leverage change for recall, precision, and token
  efficiency at once (cAST-style structural chunking).

Files:

- `src/context_engine/syntax.rs` (extend or split into `chunking.rs`)
- `src/context_engine/index.rs`
- `src/context_engine/evidence.rs`
- `Cargo.toml` (tree-sitter crates)

Work items:

- Adopt tree-sitter parsers for Rust, TypeScript/TSX, JavaScript/JSX, and
  Python. Replace the hand-rolled extractors in `syntax.rs` with
  tree-sitter-backed extraction behind the same `ParsedSymbols` surface.
  This also closes the "broader parser coverage" hardening item.
- Implement recursive AST chunking with a token-size limit:
  - Split nodes larger than the limit into child-node chunks.
  - Merge adjacent small siblings up to the limit.
  - Fall back to indentation/blank-line chunking for unparsed languages.
- Emit `FileSpan` evidence per chunk with:
  - `range` set to the chunk's line span.
  - `token_estimate` computed from the chunk content, not the file size.
  - `content_hash` over the chunk content.
  - a structured summary: enclosing symbol path, kind, and first doc line.
- Replace whole-file `FileSpan` evidence with chunk evidence as the
  retrieval unit. Whole-file evidence remains only for kinds where the file
  is the natural unit (`RepositoryRule`, `Config`, `Doc` under a size
  threshold).
- Add an `is_changed_span` structured field on evidence: true when the chunk
  overlaps a diff hunk. Stop encoding changed-ness in the summary string.
- Respect existing `ContextLimits` budgets; chunking must not blow
  `max_evidence_items` (cap chunks per file, record the rest as skips with a
  new `ChunkBudgetExceeded` reason).

Tests:

- A 1,000-line Rust file yields function/impl-level chunks, each within the
  token limit, with correct line ranges.
- A chunk overlapping a diff hunk has `is_changed_span = true`.
- Unparsed language files fall back to blank-line chunking.
- Chunk evidence ids are stable across two index runs of the same snapshot.
- Evidence-count budgets hold with pathological many-function files.

Acceptance:

- Benchmark gate: recall@10 and token efficiency improve over the R0
  baseline; precision does not regress beyond tolerance.
- Packs cite line-ranged chunks instead of whole files for code evidence.

### Phase R2: BM25 Lexical Retrieval

Intent:

- Replace unscored substring matching with ranked BM25 over code-aware
  tokens. Deterministic, no model, no network.

Files:

- `src/context_engine/lexical.rs` (new)
- `src/context_engine/retrieval.rs`
- `src/context_engine/index.rs`

Work items:

- Implement a code tokenizer:
  - Split on non-alphanumerics, then split `camelCase`, `PascalCase`,
    `snake_case`, and digit boundaries.
  - Index both the full identifier and its subtokens so exact identifier
    queries still hit precisely.
  - Lowercase subtokens; preserve the original identifier token.
- Build an in-memory inverted index over chunk evidence at index time:
  postings of term -> (evidence id, term frequency), document lengths, and
  corpus statistics.
- Implement BM25 scoring (k1 = 1.2, b = 0.75 as starting constants, held in
  `ContextEngineConfig`).
- Field weighting: path and symbol-name terms weigh higher than body terms.
- Replace `search_evidence` substring logic with BM25 ranking. Delete the
  substring path; do not keep it as a fallback.
- `search_text` query results return scored, rank-ordered evidence.

Tests:

- Query `getUserId` ranks the chunk defining `get_user_id` and `getUserId`
  above chunks that merely mention `user`.
- Exact identifier query ranks the exact match first.
- Rare-term queries (error codes, config keys) hit the defining chunk.
- Empty and stop-word-only queries return empty results, not errors.
- Deterministic ordering: ties break on evidence id, stable across runs.

Acceptance:

- Benchmark gate: recall@10 and nDCG@10 improve over the R1 baseline on
  lexical-query cases.
- A hand-rolled implementation stays dependency-light; tantivy is adopted
  only if corpus-size benchmarks show the in-memory index is too slow at
  `max_indexed_files` scale.

### Phase R3: Rank Fusion For Hybrid Retrieval

Intent:

- Fix the self-defeating semantic merge. Fuse lexical and semantic candidate
  lists by rank with Reciprocal Rank Fusion so paraphrase matches surface
  without score-normalization tricks.

Files:

- `src/context_engine/retrieval.rs`
- `src/context_engine/semantic.rs`
- `src/context_engine/config.rs`

Work items:

- Delete the lexical-term-overlap gate in `merge_semantic_search`.
- Implement RRF: `score(d) = sum over lists of 1 / (k + rank_in_list(d))`
  with `k = 60` held in config.
- Inputs: BM25 ranked list (R2) and vector ranked list (existing
  `InMemoryVectorIndex::search`). In no-vector mode the fusion is a no-op
  passthrough of the BM25 list.
- Fused results carry both source ranks in provenance-adjacent query
  metadata so `explain_pack` can say "lexical rank 3, semantic rank 1".
- Apply trust and sensitivity filters after fusion, before truncation, so a
  high-ranking restricted item is omitted with a recorded reason rather than
  silently dropped.

Tests:

- A paraphrase query ("end of agreement" vs "termination") in local
  semantic mode surfaces the semantically matching chunk with zero lexical
  term overlap.
- No-vector mode output equals the pure BM25 output.
- An item appearing in both lists outranks an item appearing in one list at
  similar positions.
- Restricted evidence excluded by fusion appears in omission records.

Acceptance:

- Benchmark gate: semantic-mode cases improve recall@10 without degrading
  no-vector cases (which must be bit-identical to R2 output).

### Phase R4: Change-Rooted Graph Expansion

Intent:

- Use the diff as the retrieval anchor. For review, the highest-precision
  context is the blast radius of the change: enclosing definitions, callers,
  callees, tests, and historically co-changed files.

Files:

- `src/context_engine/syntax.rs` / `chunking.rs`
- `src/context_engine/graph.rs` (new)
- `src/context_engine/index.rs`
- `src/context_engine/symbol_query.rs`
- `src/runtime/repo.rs` (read-only git history access)

Work items:

- Resolve diff hunks to enclosing definitions using the tree-sitter chunk
  tree from R1: each hunk maps to the smallest enclosing chunk and its
  symbol path.
- Replace bare-name importer matching with scoped resolution:
  - Key symbols by `(file path, symbol path)` instead of bare name.
  - Resolve imports to defining files using module-path resolution per
    language (Rust `use` paths, TS/JS relative and package specifiers,
    Python module paths). Unresolvable imports degrade to name matching
    with a lower-confidence relationship label.
- Build a reference graph: definition -> referencing chunks (callers /
  users), via resolved imports plus identifier occurrence within importing
  files.
- Compute git co-change frequency:
  - Walk the last N commits (config, default 500) of the materialized
    checkout.
  - For each changed file in the review, record files that co-occurred in
    past commits with count and recency decay.
  - Deterministic: pinned to the snapshot's commit, pure function of
    history.
- Emit change-rooted candidates with typed `ContextRelationship` entries:
  `encloses_hunk`, `calls`, `called_by`, `tests`, `co_changed`,
  `same_module`.
- Expansion is bounded: max hops (default 2), max candidates per anchor,
  all held in `ContextLimits`.

Tests:

- A hunk inside a Rust function maps to that function's chunk, not the file.
- Changing an exported TS function surfaces its importing call sites as
  `called_by` candidates with resolved paths (no bare-name collisions
  across modules).
- Two files that changed together in 10 of the last 50 commits surface as
  `co_changed` candidates with frequency recorded.
- Expansion respects hop and candidate budgets; overflow lands in omission
  records.
- A repository with no git history degrades cleanly (no co-change signal,
  no error).

Acceptance:

- Benchmark gate: recall@10 on review-shaped cases (ground truth mined from
  fix commits in R0) improves materially; this phase is expected to be the
  largest single gain.
- `related_symbols` and `related_tests` answers come from the resolved
  graph.

### Phase R5: Ranking Signals V1

Intent:

- Replace summary-string heuristics with typed structural signals. Ranking
  stays deterministic and explainable; weights live in config, justified by
  the benchmark.

Files:

- `src/context_engine/pack.rs`
- `src/context_engine/evidence.rs`
- `src/context_engine/config.rs`

Work items:

- Add structured ranking inputs to evidence/candidates:
  - `is_changed_span` (R1)
  - `graph_distance` from nearest change anchor (R4; 0 = encloses hunk)
  - `co_change_score` (R4)
  - `path_proximity` (shared directory depth with changed files)
- Rewrite `score_for_purpose`:
  - Keep purpose/kind bonuses and trust-aware ordering.
  - Replace `summary.contains("changed")` with `is_changed_span`.
  - Add weighted structural signals; weights in `ContextEngineConfig` with
    documented defaults.
  - Keep `semantic_score_for_purpose` as an additive signal.
- Update `explain_selected_evidence` to cite the actual signals
  ("encloses changed hunk", "co-changed with src/auth/token.rs in 8 recent
  commits", "direct caller of changed function").
- Re-rank omission candidates with the same scorer so omission ordering is
  meaningful.

Tests:

- The chunk enclosing a hunk outranks an unrelated same-kind chunk.
- A frequently co-changed file outranks a lexically similar but historically
  unrelated file at equal kind/purpose bonus.
- Explanations reference structural signals, not "deterministic V0
  heuristics".
- Weight changes in config change ordering predictably (snapshot test).

Acceptance:

- Benchmark gate: nDCG@10 and tokens-to-first-relevant improve over R4
  baseline.
- No ranking input is parsed from a display string anywhere in
  `pack.rs`.

### Phase R6: Structural Sufficiency And Iterative Retrieval

Intent:

- Make sufficiency a per-hunk structural check that can drive an agentic
  retrieval loop through the existing `context.*` tools, instead of
  `empty -> insufficient`.

Files:

- `src/context_engine/engine.rs`
- `src/context_engine/pack.rs`
- `src/context_engine/tools.rs`
- `src/reviewer/` session integration points

Work items:

- Define per-hunk coverage requirements for a pack purpose:
  - the enclosing definition chunk is present,
  - at least one caller/usage chunk is present (or the definition is
    verifiably unreferenced),
  - related tests are present, or an explicit `no_related_tests` gap is
    recorded.
- `SufficiencyCheck` evaluates coverage per hunk and returns typed gaps:
  `{ hunk, missing: [enclosing_definition | callers | tests], suggested_query }`.
  `suggested_query` is a ready-to-run `context.*` query that would fill the
  gap.
- Pack compiler records sufficiency from the same coverage logic, so the
  pack artifact and the query result can never disagree.
- Reviewer session loop: when a pack is `insufficient`, the session is
  prompted with the gap list and tool grants to run the suggested queries
  before producing findings. Iteration count is bounded by existing session
  budgets.
- Calibrate thresholds against the R0 corpus: a pack that contains all
  ground-truth spans should report sufficient; packs missing ground truth
  should report gaps that, when filled, include the ground truth.

Tests:

- A pack missing the enclosing definition of a hunk reports an
  `enclosing_definition` gap with a runnable suggested query.
- Running the suggested query returns evidence that clears the gap.
- A change to an unreferenced private helper does not demand callers.
- Sufficiency in the pack artifact equals a `SufficiencyCheck` query over
  the same evidence set.
- Iteration is bounded; budget exhaustion downgrades to
  `probably_sufficient` with recorded unresolved gaps.

Acceptance:

- Benchmark gate: sufficiency calibration metrics (gap precision/recall
  against ground truth) are reported; review-run evaluation shows iterative
  retrieval fills gaps within session budgets.

### Phase R7: Skeleton Evidence And Honest Token Accounting

Intent:

- Spend the token budget better: relevant-but-large context enters as a
  signatures-only skeleton instead of being omitted entirely, and budget
  math uses real content sizes.

Files:

- `src/context_engine/chunking.rs`
- `src/context_engine/index.rs`
- `src/context_engine/pack.rs`

Work items:

- Generate skeleton views per file from the tree-sitter chunk tree:
  definitions and doc comments retained, bodies elided to `...`, line
  numbers preserved.
- Add a `Skeleton` representation on evidence (full chunk vs skeleton), with
  separate token estimates.
- Pack compiler degradation ladder: when a candidate's full chunk does not
  fit the remaining budget but its skeleton does, include the skeleton and
  record the downgrade in omission/explanation data.
- Deduplicate overlapping spans: a selected chunk suppresses its enclosing
  file/skeleton duplicate.
- `estimate_tokens` operates on actual evidence content lengths everywhere;
  remove file-size-based estimates.

Tests:

- A large related file enters a budget-constrained pack as a skeleton with
  preserved line numbers.
- A chunk and its skeleton are never both included.
- Budget usage accounting matches the sum of included content estimates.
- Skeletons preserve enough signature text for the benchmark's
  range-coverage checks.

Acceptance:

- Benchmark gate: token efficiency and tokens-to-first-relevant improve;
  recall does not regress.

### Phase R8: Real Embeddings And Optional Reranking

Intent:

- Make semantic mode actually semantic. The hashed provider remains only as
  the deterministic test fixture; quality paths use real models.

Files:

- `src/context_engine/semantic.rs`
- `src/context_engine/config.rs`
- `src/cli.rs` / context CLI flags

Work items:

- Hosted mode: validate against code-tuned embedding models through the
  existing OpenAI-compatible provider; record model id in provenance;
  document recommended models.
- Local mode: evaluate a real local embedding model via ONNX runtime behind
  the existing `EmbeddingProvider` trait, as the privacy-sensitive quality
  tier. Adopt only if the benchmark shows material gains over BM25+RRF
  alone at acceptable index latency.
- Optional cross-encoder rerank stage over the fused top-50, hosted mode
  only, off by default, subject to the same restricted-evidence input
  policy as hosted embeddings (`validate_embedding_batch` equivalents).
- Embed chunks (R1 units), not whole files; reuse content-hash keyed
  caching from R9 when available.

Tests:

- Hosted embedding and rerank requests reject restricted evidence unless
  explicitly allowed (existing policy extended to rerank inputs).
- Rerank-off output is unchanged from R3 fusion output.
- Provider failure degrades to fused lexical results with a recorded
  warning, consistent with existing failure-mode policy.

Acceptance:

- Live hosted benchmark run (credentialed, not in CI) shows recall/nDCG
  gains over BM25+RRF; results committed to
  `bench/results-context-engine/`.

Result (2026-06-11): shipped. Embeddings reach pack ranking through the
`semantic_change_score` rank signal (similarity to the nearest change
anchor, `weight_semantic_change` 0.10, swept against 0.15/0.20); the
static semantic kind-bonus table is deleted. Live 46-case runs versus
the deterministic baseline (recall@10 0.540, nDCG@10 0.469,
recall@25 0.604, first-relevant rate 0.804):

- Hosted `text-embedding-3-small`: recall@10 0.612, nDCG@10 0.515,
  recall@25 0.649, precision 0.212, first-relevant rate 0.891, paired
  tokens-to-first-relevant 2356 -> 1897, warm mean latency 174 ms.
- Local ONNX `jina-embeddings-v2-base-code` (quantized): recall@10
  0.597, nDCG@10 0.510, recall@25 0.658, precision 0.222,
  first-relevant rate 0.891, warm mean latency 195 ms. Adopted as the
  privacy-sensitive quality tier; cold indexing pays CPU inference
  once per corpus, then the R9 vector cache holds.
- Rerank stage ships off by default behind the Cohere-style `/rerank`
  contract (Cohere, Jina, vLLM, Infinity, in-house servers; bearer
  credential optional). Contract-tested against a loopback server;
  no live cross-encoder acceptance yet for lack of a credentialed
  endpoint.
- Provider failures degrade: index build falls back to lexical-only
  with a `semantic_provider_failed` warning; query-time embedding or
  rerank failures keep lexical/fused results and record the
  degradation in query data.

### Phase R9: Incremental, Persistent Indexing

Intent:

- Stop rebuilding from scratch per snapshot. Repeated reviews of the same
  repository should reuse chunking, parsing, postings, and embeddings for
  unchanged content.

Files:

- `src/context_engine/store.rs`
- `src/context_engine/index.rs`
- workspace host wiring (`muzen-service`, mirroring the learning-store
  pattern)

Work items:

- Key derived data by content hash: chunk sets, parsed symbols, postings
  contributions, and embedding vectors per chunk hash.
- Add a derived-data cache behind `ContextIndexStore` with in-memory and
  durable (JSON/file, matching the learning-store pattern) backends.
- Index build becomes: diff the snapshot manifest against cached hashes,
  recompute only changed files, merge cached postings and vectors.
- Cache invalidation: context engine version and chunker version are part
  of the cache key; bumping either invalidates cleanly.
- Workspace hosts opt in via a sanitized store root, mirroring
  `MUZEN_CONTEXT_LEARNING_STORE_ROOT`.

Tests:

- Re-indexing an unchanged snapshot performs zero chunk/parse/embed work
  (assert via counters) and produces an identical index id and manifest.
- A one-file change recomputes only that file's derived data.
- A version bump invalidates the cache and rebuilds.
- Cache corruption degrades to full rebuild with a warning, not an error.

Acceptance:

- Benchmark latency metrics show order-of-magnitude index-time improvement
  on warm cache for the largest corpus fixture.
- Hosted embedding spend on warm re-index is near zero (only changed
  chunks).

## Configuration

New `ContextEngineConfig` fields (defaults in parentheses):

- `chunk_max_tokens` (400), `chunks_per_file_max` (64)
- `bm25_k1` (1.2), `bm25_b` (0.75), field weight map
- `rrf_k` (60)
- `graph_expansion_hops` (2), `graph_candidates_per_anchor` (32)
- `co_change_commit_window` (500), recency half-life
- ranking weights: `weight_changed_span`, `weight_graph_distance`,
  `weight_co_change`, `weight_path_proximity`
- `sufficiency_iteration_max` (2)
- `rerank_enabled` (false), rerank provider settings
- derived-data cache root and size bounds

All defaults are committed alongside the benchmark baseline that justified
them. Ranking knobs remain internal configuration, not primary public API,
per the original plan's non-goal.

## Failure Modes

- Tree-sitter parse failure on a file: fall back to blank-line chunking for
  that file, record an index warning, never fail the index.
- Git history unavailable (shallow clone, tarball snapshot): co-change
  signal is zero, relationship omitted, no error.
- Embedding/rerank provider failure: degrade to fused lexical results with
  a recorded warning (existing policy).
- Derived-data cache unreadable: full rebuild with warning.
- Budget exhaustion at any stage: omission records with typed reasons, never
  silent truncation.

## Security Checklist

- Chunk evidence inherits file-level trust and sensitivity; chunking must
  not launder `RepositoryUntrusted` guidance into kernel-trusted spans.
- Co-change and graph expansion operate only on the materialized snapshot
  and its own git history; no network access.
- Rerank inputs enforce the same restricted-evidence policy as hosted
  embedding inputs.
- Skeleton views pass through the same redaction as full content.
- Derived-data cache paths are sanitized like learning-store paths; cached
  content is keyed by content hash and never crosses workspace boundaries.
- Prompt-injection bench cases must keep passing at every phase: hostile
  guidance ranked higher by BM25/fusion must still carry untrusted labels
  and lose trust-policy conflicts.

## Test Matrix

Beyond per-phase tests:

- Determinism: no-vector mode produces byte-identical pack artifacts across
  runs at every phase.
- Replay: a pack built at phase N can be explained (`explain_pack`) with
  signal-level reasons.
- Budgets: `ContextLimits` hold under adversarial inputs (huge files, many
  small functions, deep import cycles, enormous git history).
- Trust: trust-rank ordering and evidence-policy checks unchanged by all
  ranking changes.
- Cross-surface: CLI, HTTP, runner stdio, and both SDKs return identical
  results for identical queries at each phase boundary (existing adapter
  test pattern).

## Open Questions For Review

- Corpus licensing: which OSS repositories to vendor for R0, and pinned at
  which commits?
- Should co-change analysis live in `src/context_engine/graph.rs` or in
  `src/runtime/repo.rs` next to materialization, given it reads git
  history?
- Is tantivy acceptable as a dependency if the hand-rolled BM25 index is
  too slow at `max_indexed_files` scale, or do we raise the in-memory
  ceiling instead?
- For R6, does the iterative retrieval loop live in the reviewer kernel's
  session driver or in planned-units exploration requirements?
- Local ONNX embedding runtime (R8): acceptable dependency weight, or
  hosted-only for the quality tier? Resolved 2026-06-11: adopted (`ort`
  + `tokenizers`); the local tier matches hosted quality on the eval
  corpus at equal warm latency with no data leaving the host.

## Review Checklist

- [ ] R0 corpus merged with baseline metrics committed.
- [ ] Each phase PR includes before/after benchmark summary.
- [ ] No phase keeps its predecessor as a runtime fallback mode.
- [ ] No ranking input parsed from display strings.
- [ ] Determinism tests pass in no-vector mode at every phase.
- [ ] Trust, redaction, and injection bench cases pass at every phase.
- [ ] Adapter parity (CLI/HTTP/runner/SDKs) verified at phase boundaries.
- [ ] `CONTEXT.md` vocabulary updated if new terms become structural
      (chunk, blast radius, skeleton, sufficiency gap).
