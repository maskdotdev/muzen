# Muzen SOTA Context Graph Plan

Generated: 2026-06-11

Status: accepted for implementation

Supersedes: `context-graph-primitive-plan.md` (draft)

## Decision

Make `ContextGraph` a first-class Muzen primitive owned by the Context Engine.
It replaces the current `ReferenceGraph` shape rather than wrapping it with a
compatibility layer.

The contract:

```text
A ContextGraph is a deterministic, bounded, explainable graph of
review-relevant relationships between repository artifacts, built
per snapshot.
```

It is not a compiler-accurate `CodeGraph`, a vector index, or an incidental
retrieval helper. It is the shared substrate for retrieval, review planning,
ranking, sufficiency, explainability, budgeting, benchmarking, and future
debug UI paths.

## Positioning: Which Graph Is SOTA For Review Retrieval

There are three levels of "code graph":

| Level                 | Knows                                                        | Examples                                  |
| --------------------- | ------------------------------------------------------------ | ----------------------------------------- |
| File import graph     | "A imports from B"                                           | Aider repo-map (roughly), Muzen today      |
| Chunk reference graph | "chunk `f` in A references symbol `g` defined in chunk in B" | stack-graphs-adjacent; Muzen target        |
| Semantic call graph   | "this call resolves to that method through types/dispatch"   | rust-analyzer, tsserver, SCIP indexers     |

The target is the **middle level**. The graph is a candidate generator for
review context, not a truth oracle: ranking, budgets, and trust filters
discard false edges cheaply, but no downstream stage can recover a missing
edge. So the SOTA bar for this primitive is **edge recall with explainable
provenance**, not compiler-grade edge precision.

Benchmark evidence (2026-06-11, 86-case corpus): the argus-app corpus scores
recall@10 `0.193` era-independently because `resolve_module` cannot resolve
`@/*` path aliases — roughly 85% of that repository's internal imports
produce no edge. Missing edges, not edge precision, is the binding
constraint. This ordering drives the implementation phases below.

## Why This Is A Primitive

Apply the deletion test. If the graph is deleted, the behavior reappears in
multiple callers:

- retrieval would rebuild related-file expansion
- ranking would infer graph distance and co-change signals separately
- sufficiency would rediscover callers, tests, and enclosing spans
- `explain_pack` would invent explanations after selection
- benchmarks would need bespoke expected-related-artifact logic
- future UI/debugging would reconstruct paths from selected evidence

That makes the graph a deep module opportunity. A small `ContextGraph`
interface can hide a large implementation: language resolvers, hunk-to-chunk
mapping, test discovery, co-change mining, path proximity, provenance, and
bounded traversal.

## Current State

The current implementation already proves the need:

- `src/context_engine/graph.rs` defines `ReferenceGraph`, `ReferenceEdge`,
  `GraphCandidate`, `co_change_stats`, and `expand_from_changes`.
- `src/context_engine/index.rs` stores `graph`, `graph_candidates`, and
  `relationships`, then uses them to fill `ContextRankSignals`.
- `src/context_engine/sufficiency.rs` reads `index.graph.referencers(...)` to
  decide caller and test coverage.
- `src/context_engine/symbol_query.rs` uses `ReferenceGraph` directly for
  `related_symbols`.
- `ContextRelationship` exists at evidence-id level, but the graph itself is
  still file-biased and candidate-oriented.

The architecture friction is that the graph's responsibilities are split
between graph construction, index assembly, ranking, sufficiency, and query
modules. The implementation is doing Context Graph work but the interface
still says "reference graph".

## Non-Goals

- Do not call this `CodeGraph`; that implies compiler-level precision.
- Do not preserve `ReferenceGraph` as a public alias.
- Do not add a second graph mode for older retrieval behavior.
- Do not require network access or model calls.
- Do not make resolver failures fatal when bounded low-confidence edges can
  represent the uncertainty.
- Do not expose ranking knobs as the main public interface.
- Do not promise cross-snapshot node identity. Node ids are stable within a
  snapshot; cross-snapshot graph diffing would need content-hash identity and
  is out of scope.
- Do not store traversal weights on edges. Edges carry facts; traversal
  computes value.
- Do not expose an arbitrary-pair `shortest_path` API. Expansion returns a
  path for every candidate, which covers explainability; an unbounded
  pair-query has no caller and invites unbounded traversal.

## Core Model

### Node IDs

Use one stable enum instead of path strings passed through many modules.
Node ids are deterministic from snapshot content and stable **within** a
snapshot.

```rust
pub enum ContextNodeId {
    Repo { snapshot_id: SnapshotId },
    File { path: RepoPath },
    Chunk { path: RepoPath, range: ContextRange },
    Symbol { path: RepoPath, name: String, range: ContextRange },
}
```

Chunk nodes are identified by what R1 chunking produces — `(path, range)` —
**not** by `EvidenceId`. The build pipeline is
`parsed artifacts -> ContextGraph -> evidence projection`; identifying chunks
by evidence id would make graph construction depend on the layer that is
supposed to be a projection of the graph. The graph-to-evidence mapping is
built at projection time (G5).

Tests and external contracts are chunk or file nodes plus node-kind
metadata, not separate id variants, until test structure becomes richer.

### Nodes

```rust
pub struct ContextNode {
    pub id: ContextNodeId,
    pub kind: ContextNodeKind,
    pub path: Option<RepoPath>,
    pub range: Option<ContextRange>,
    pub label: String,
    pub provenance: ContextGraphProvenance,
}
```

Node kinds:

- `Repo`
- `File`
- `Chunk`
- `Symbol`
- `Test`
- `Config`
- `RepositoryRule`
- `ExternalContract`

### Edges

```rust
pub struct ContextEdge {
    pub id: ContextEdgeId,
    pub from: ContextNodeId,
    pub to: ContextNodeId,
    pub kind: ContextEdgeKind,
    pub confidence: f32,
    pub reason: String,
    pub provenance: ContextGraphProvenance,
}
```

Edge kinds (one canonical direction each; the reverse view is a traversal
direction via `edges_to`, not a second edge kind):

- `Contains` (repo -> file -> chunk/symbol)
- `EnclosesHunk` (chunk -> hunk anchor)
- `Imports` (importer -> defining file/symbol)
- `Defines` (file/chunk -> symbol)
- `References` (chunk/symbol -> symbol; identifier-level)
- `Tests` (test chunk/file -> tested file/symbol)
- `CoChanged` (file <-> file; symmetric, store once with ordered endpoints)
- `SameModule` (file <-> file; symmetric, store once with ordered endpoints)
- `Convention` (declarative convention rule edge)
- `Configures` (config file -> affected target)
- `DependsOn` (manifest-level dependency)
- `Documents` (doc -> documented artifact)
- `ExternalContract` (artifact -> cross-repo contract)
- `GeneratedFrom` (generated artifact -> source)

Reverse kinds (`ImportedBy`, `ReferencedBy`, `TestedBy`) are deliberately
absent: storing both directions doubles the surface where construction can
go inconsistent and adds no traversal power.

`confidence` describes how certain the relationship fact is (a resolved
import is ~0.9; a bare-name fallback match is ~0.5). It is **not** relevance.
Traversal value is computed by `expand()` from `(kind, confidence, purpose)`
at query time, so sufficiency can value `Tests` edges differently from
retrieval ranking without the graph storing two drifting numbers.

### Provenance

```rust
pub enum ContextGraphSource {
    SnapshotManifest,
    DiffHunk,
    SyntaxTree,
    ImportResolver,
    IdentifierScan,
    TestConvention,
    GitHistory,
    HostMetadata,
    CrossRepoProvider,
}

pub struct ContextGraphProvenance {
    pub source: ContextGraphSource,
    pub detail: String,
    pub snapshot_id: Option<SnapshotId>,
}
```

This makes explainability a property of the graph instead of a post-hoc
string attached by the pack compiler.

## Interface

Keep the interface small and traversal-oriented:

```rust
impl ContextGraph {
    pub fn build(input: ContextGraphBuildInput) -> ContextGraph;

    pub fn node(&self, id: &ContextNodeId) -> Option<&ContextNode>;
    pub fn edges_from(&self, id: &ContextNodeId) -> impl Iterator<Item = &ContextEdge>;
    pub fn edges_to(&self, id: &ContextNodeId) -> impl Iterator<Item = &ContextEdge>;

    pub fn changed_anchors(&self) -> impl Iterator<Item = &ContextNodeId>;
    pub fn expand(&self, request: ContextGraphExpansionRequest) -> ContextGraphExpansion;
}
```

Expansion returns graph paths, not only file candidates:

```rust
pub struct ContextGraphExpansion {
    pub candidates: Vec<ContextGraphCandidate>,
    pub omitted: Vec<ContextGraphOmission>,
}

pub struct ContextGraphCandidate {
    pub node_id: ContextNodeId,
    pub anchor: ContextNodeId,
    pub score: f32,
    pub hop_count: u8,
    pub path: ContextGraphPath,
}
```

The path is the durable explanation:

```text
changed chunk -> defines symbol -> imported by chunk -> test chunk
```

## Build Pipeline

Replace the current flow:

```text
ContextIndex builds evidence
ContextIndex builds ReferenceGraph
ContextIndex expands graph candidates
ContextIndex patches rank signals
Sufficiency independently queries graph
```

with:

```text
RepoSnapshot
  -> parsed artifacts
  -> ContextGraph
  -> evidence/rank signal projection
  -> retrieval and ranking
  -> sufficiency
  -> ContextPack
```

`ContextIndex` still owns the compiled index, but `ContextGraph` owns all
relationship semantics. `ContextIndex` projects graph facts into
`ContextRelationship`, `ContextRankSignals`, and query outputs.

## Implementation Phases

Sequencing principle: the rename is mechanical and lands first; the resolver
fix lands second because it is the one change with benchmark evidence of a
large quality gap (argus recall@10 `0.193`), it touches only the leaf
`resolve_module` function, and the benchmark gate then protects the win
through the structural phases that follow.

### G0: Rename And Own The Module

Files:

- `src/context_engine/graph.rs`
- `src/context_engine/index.rs`
- `src/context_engine/sufficiency.rs`
- `src/context_engine/symbol_query.rs`
- `src/context_engine/evidence.rs`
- `src/context_engine/tests.rs`

Work:

- Rename `ReferenceGraph` to `ContextGraph`.
- Rename `GraphCandidate` to `ContextGraphCandidate`.
- Move `expand_from_changes` and `co_change_stats` onto `ContextGraph`.
- Delete old names instead of aliasing them.

Acceptance:

- No `ReferenceGraph` string remains in `src/context_engine`.
- Existing graph, sufficiency, query, and ranking tests pass.
- Public docs use `ContextGraph` and `Context Graph` consistently.

### G1: TypeScript Resolution Edge Source (The Binding Constraint)

Work:

- Read `tsconfig.json` (and `jsconfig.json`) `compilerOptions.baseUrl` and
  `compilerOptions.paths` from the snapshot during graph build.
- Resolve aliased specifiers (`@/lib/db` through `@/* -> ./src/*`) using the
  existing implicit extension and `index.*` logic.
- Follow barrel re-exports one hop: `export * from './foo'` and named
  re-exports in `index.ts`, so importers of a barrel connect to defining
  files instead of dead-ending.
- Keep resolver output as ordinary resolved edges with provenance
  (`ImportResolver`, detail naming the matched alias rule). No retrieval,
  ranking, or sufficiency branches.

Acceptance:

- Unit tests: alias resolution against `paths` patterns, baseUrl-relative
  specifiers, barrel hop, alias that matches no file produces no edge (falls
  back to bare-name path with existing fan-out cap).
- Benchmark: argus-corpus recall@10 and nDCG@10 improve materially; muzen
  corpus does not regress beyond tolerance; new expanded baseline written.
- Per-case bench output shows previously zero-recall argus cases gaining
  graph-sourced evidence.

### G2: Introduce First-Class Nodes

Work:

- Add `ContextNodeId`, `ContextNodeKind`, and `ContextNode`.
- Build file nodes for every indexed file.
- Build chunk nodes from R1 chunking output as `(path, range)`.
- Build symbol nodes from `ParsedSymbols.definition_ranges`.
- Add `Contains` edges: repo -> file, file -> chunk, file -> symbol, chunk ->
  symbol when ranges overlap.
- Replace path-only `ContextGraphCandidate.path` with `node_id`, while still
  allowing a candidate to project to a representative file/chunk evidence
  item.

Acceptance:

- A changed hunk maps to a chunk node, not only a file path.
- `ContextGraph::changed_anchors()` returns changed chunk nodes when chunks
  exist for the file.
- Existing pack relationships still project to `EvidenceId` pairs.

### G3: Make Edges Typed And Provenance-Carrying

Work:

- Add `ContextEdgeKind`, `ContextEdgeId`, `ContextGraphProvenance`, and
  explicit `confidence`.
- Convert current references into canonical-direction `Imports`, `Defines`,
  and `References` edges; narrow `References` to chunk granularity via
  identifier occurrence over already-parsed chunk bodies (the R4 promise of
  "definition -> referencing chunks").
- Convert `same_module_siblings` into `SameModule` edges and
  `co_change_stats` into `CoChanged` edges (count and decayed weight in
  provenance detail), stored once with ordered endpoints.
- Add deterministic edge ids from `(from, to, kind, provenance detail)`.

Acceptance:

- `explain_pack` can cite graph edge reasons without inventing new text.
- Co-change count and resolved-vs-unresolved import status survive as
  structured graph data.
- Low-confidence unresolved/name-match edges are visible and bounded.
- A 2,000-line candidate file contributes its referencing chunk, not all of
  its chunks, at hop-1 graph distance.

### G4: Move Traversal And Candidate Budgeting Into ContextGraph

Work:

- Replace free-function expansion with `ContextGraph::expand(...)`.
- Expansion input carries limits: max hops, max candidates per anchor,
  allowed node kinds, minimum confidence, purpose.
- Traversal value is computed from `(edge kind, confidence, purpose)`; ties
  break on stable node ids.
- Expansion output carries omitted candidates with reasons:
  - `BudgetExceeded`
  - `BelowConfidenceFloor`
  - `NoEvidenceProjection`
  - `DuplicateLowerScore`
- Return graph paths for every candidate.

Acceptance:

- `ContextIndex` no longer implements graph traversal policy.
- Graph candidate truncation warnings include omitted counts by reason.
- Candidate order is deterministic across two runs of the same snapshot.

### G5: Project Graph Facts Into Existing Context Products

Work:

- Build `ContextRelationship` from graph paths, not from path-only
  candidates; map chunk nodes to `EvidenceId` here.
- Fill `ContextRankSignals` from graph expansion:
  - `graph_distance` (now chunk-accurate)
  - `co_change_score`
  - `path_proximity`
  - future `test_coverage_score`
- Update `explain_selected_evidence` to cite Context Graph paths.
- Extend `ExplainPack` data with graph paths for included evidence.

Acceptance:

- Included evidence can answer "why this file/chunk?" with a path.
- Omitted evidence can answer "why not?" with budget or confidence reason.
- Pack relationships remain evidence-id-level for artifact stability.

### G6: Rebase Sufficiency On The Graph

Work:

- Replace `index.graph.referencers(path)` sufficiency logic with graph
  coverage queries:
  - hunk anchor has enclosing chunk
  - hunk anchor has caller/reference paths when they exist
  - hunk anchor has `Tests` coverage or explicit no-related-tests evidence
  - changed config has `Configures` targets when resolvable
  - external contracts are present when host metadata declares them
- Make "no related tests" an explicit graph-derived fact or omission, not
  only a local sufficiency branch.

Acceptance:

- Sufficiency and retrieval use the same graph paths.
- A missing caller/test gap includes the graph query that would fill it.
- Sufficiency does not independently recreate path-stem or test-convention
  logic outside the graph module.

### G7: Use The Graph For Benchmarks And Debugging

Work:

- Add graph metrics to `bench/context-engine/run.py`:
  - graph recall@k against expected paths
  - path found for expected artifact
  - expansion omitted by reason
  - confidence distribution by edge kind
- Add an opt-in JSON export for graph debug artifacts (nodes, edges, changed
  anchors, expansion results, omitted candidates). Not part of the default
  `ContextIndexReport.artifacts`; bench/debug runs only.
- Keep export deterministic and bounded.

Acceptance:

- Bench failures can show whether retrieval missed an artifact because the
  graph lacked an edge, traversal omitted it, or ranking/budgeting dropped
  it.
- A future UI can render graph paths without reverse-engineering pack data.

### G8: Remaining Resolver Hardening

Work:

- TypeScript: package `exports` maps, deeper re-export chains.
- Rust: `mod` declarations, re-export chains, workspace crate boundaries when
  captured in the snapshot.
- Python: relative imports, package `__init__.py` re-exports.
- Optional `Convention` edge source: a small declarative rule format
  (path-glob -> path-glob edge templates with confidence), shipped defaults
  plus repository-provided rules. Data, not per-framework code. Gated on
  benchmark evidence that a convention gap remains after G1/G3.

Acceptance:

- Resolver failures only affect edges from that source.
- Resolver tests assert graph edges and explainable paths, not just
  retrieval output.
- Any `Convention` defaults are justified by named bench cases they fix.

## Testing Strategy

Unit tests:

- stable node ids for file/chunk/symbol nodes
- edge id determinism
- tsconfig alias and barrel resolution edge provenance
- co-change edge confidence and provenance detail
- traversal budget omissions by reason
- graph path explain output

Integration tests:

- changed TS export surfaces importers (including alias importers) through a
  path of typed edges
- barrel export surfaces downstream importers without bare-name collisions
- changed Rust function surfaces module users and tests
- missing git history produces no co-change edges and no error
- sufficiency reports missing tests from graph coverage

Benchmark gates:

- G1: argus recall@10 / nDCG@10 improve; muzen corpus holds within tolerance;
  baseline rewritten once after review.
- G2–G6: recall@10, nDCG@10, and tokens-to-first-relevant hold against the
  post-G1 baseline; omitted counts stable under fixed budgets.

## Architecture Rules

- One direct product path: `ContextGraph`.
- No `ReferenceGraph` alias, legacy field, or compatibility shim.
- Graph construction is deterministic from snapshot, derived artifacts, host
  metadata, allowed external contracts, and bounded git history.
- Graph traversal is bounded by `ContextLimits`.
- Every edge has a kind, confidence, reason, and provenance. Edges store
  facts; traversal computes value.
- One canonical direction per edge kind; no reverse-kind duplicates.
- Every resolver is an edge source. Retrieval, ranking, and sufficiency
  consume graph facts; they do not contain resolver logic.
- Confidence is not relevance.
- Graph paths are the source of explainability.
- Node identity is snapshot-scoped; no cross-snapshot identity promises.

## Resolved Design Questions

Previously open, now decided:

1. **Separate `Test` node id variant?** No. Tests are chunk/file nodes with
   `ContextNodeKind::Test`; revisit only if test structure becomes richer.
2. **Module split?** Split `graph.rs` into `graph/model.rs`,
   `graph/build.rs`, `graph/expand.rs` at G3, when the model types land and
   the build/expand seams are obvious.
3. **Debug export placement?** Opt-in for bench/debug runs only; never in
   default report artifacts.
4. **Persist edge `weight`?** No. Edges persist `confidence` only; traversal
   computes value from `(kind, confidence, purpose)` at query time.
5. **Chunk node identity?** `(path, range)` from R1 chunking, never
   `EvidenceId`; evidence mapping happens at projection (G5).
6. **`shortest_path` API?** Cut. Expansion paths cover explainability; an
   arbitrary-pair query has no caller and no bound.

## First Implementation Slice

The smallest useful slice is G0 + G1:

1. Rename `ReferenceGraph` to `ContextGraph` and move expansion onto it.
2. Implement tsconfig alias + barrel resolution inside the graph's module
   resolver, with unit tests asserting edges and provenance.
3. Run the benchmark gate; expect material argus improvement and no muzen
   regression; rewrite the baseline once reviewed.

That banks the measured quality win immediately and establishes the primitive
boundary. The structural phases (nodes, typed edges, traversal, projection,
sufficiency, bench instrumentation) then proceed behind a gate that already
protects the improved numbers.
