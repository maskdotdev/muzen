# Muzen Context Engine Implementation Plan

Generated: 2026-06-07

Status: draft for review

## Decision

Build the Context Engine as a first-class Muzen primitive.

The Context Engine is a core evidence-compilation module that turns immutable
Muzen snapshots, changed-file manifests, repository guidance, host metadata,
tool results, and feedback into ranked, cited, permission-aware Context Packs
and query results.

The primitive should be standalone-capable, but it should not start as a
separate product-shaped runtime. The first adapter is the Reviewer Kernel. The
second adapter should be a standalone CLI/SDK/HTTP surface that asks the same
core module questions such as:

- What context would Muzen gather for this diff?
- Which tests, rules, config files, and callers are related to this file?
- Why did a review session see this evidence and omit that evidence?
- Is the evidence behind this proposed finding sufficient?

The core design rule:

```text
One Context Engine implementation.
Multiple adapters at the interface.
No duplicate context logic in reviewer prompts, tools, validators, or SDKs.
```

## Current Vocabulary

The project vocabulary in `CONTEXT.md` now defines:

- Context Engine
- Context Evidence
- Context Pack

Those terms are intentionally narrow. The Context Engine is not a vector
database, not prompt assembly, and not a grab bag of ad hoc tools. It compiles
typed evidence with provenance, trust, sensitivity, rankings, omissions, and
budget decisions.

## Why This Is A Primitive

Apply the deletion test. If the Context Engine is deleted, its behavior
reappears across many callers:

- planner sessions would infer risk and context by hand
- reviewer prompts would stuff inconsistent file snippets
- tool implementations would each rediscover related files
- validators would invent their own sufficiency checks
- artifacts would lack a common evidence manifest
- SDKs and HTTP routes would need custom context preview logic
- benchmarks could not isolate retrieval quality from model quality

That is a shallow system. A Context Engine gives Muzen one deep module with a
small interface and a large amount of leverage behind it.

The external seam is real because there should be at least two adapters:

1. Review-run adapter: the Reviewer Kernel uses Context Packs and context tools
   during normal review execution.
2. Standalone adapter: CLI/SDK/HTTP users can index, query, and explain context
   without running a full review.

The standalone adapter should come after review integration, not before it. The
review path will prove whether the interface is deep enough.

## Goals

- Compile deterministic Context Packs from immutable `RepoSnapshot` data.
- Make every included evidence item durable, cited, and replayable.
- Store omitted candidates and omission reasons, not only selected evidence.
- Keep context on demand through tools; avoid giant prompt prefixes.
- Make context trust and sensitivity structural, not prose conventions.
- Require findings to cite concrete evidence before they become publishable.
- Support no-vector local mode first; make embeddings optional later.
- Reuse Muzen snapshots, artifacts, events, tool grants, and runner protocol.
- Keep provider-specific host context behind adapters and tool providers.
- Make context retrieval benchmarkable independently from model quality.

## Non-Goals

- Do not build a vector database first.
- Do not require embeddings for the MVP.
- Do not read live worktree files after snapshot capture.
- Do not make repository guidance authoritative over kernel policy.
- Do not add Argus-specific fields to Muzen core.
- Do not make context memory silently override explicit rules.
- Do not expose internal ranking knobs as the primary public interface.
- Do not require every host to persist context indexes in phase one.
- Do not replace existing read/search tools immediately.

## Current Muzen Touchpoints

The Context Engine should compose with these existing modules:

- `src/runtime/repo.rs`: owns `RepoSnapshot`, `FileManifest`,
  `ChangedFileMeta`, captured snapshot bytes, content hashes, and diff content.
- `src/reviewer/snapshots.rs`: public-facing snapshot specs, path policy,
  change specs, snapshot storage policy, and snapshot readers.
- `src/review_plan.rs`: deterministic changed-file review planning and risk
  hints.
- `src/reviewer/run.rs`: `RunBuilder`, snapshot construction, per-snapshot
  shards, tool engine creation, run execution, and contextual event wrapping.
- `src/runtime/tools/engine.rs`: built-in tool execution, artifact store,
  read/search services, provider dispatch, authorization, metrics, and cache.
- `src/runtime/tools/catalog.rs`: built-in tool catalog and JSON schemas.
- `src/runtime/tools/provider.rs`: built-in, in-process, and JSON-RPC tool
  providers.
- `src/runtime/contracts.rs`: ids, events, artifacts, capabilities, runtime
  limits, event context, metrics, and tool contracts.
- `src/reviewer/artifacts.rs`: artifact export, persistence, retention, and
  redacted/raw views.
- `src/reviewer/events.rs` and `src/reviewer/runtime_events.rs`: review-event
  records, runtime-event records, JSONL export/load, and schema fixtures.
- `src/reviewer/report.rs`: report, artifact reader, evidence artifact views,
  and finding evidence lookup.
- `src/reviewer/spec.rs`: `RunSpec`, `ReviewSessionSpec`, capabilities,
  sessions, budgets, and limits.
- `src/runtime/planned_units.rs`: current planned review runtime, session
  prompts, exploration requirements, and evidence tracking.
- `src/runner/types.rs` and `src/runner/schema.rs`: runner protocol contracts.
- `sdk/typescript/packages/muzen-sdk/src/types.ts`: TypeScript SDK review
  sources, instructions, tools, sessions, events, findings, evidence, and
  artifact types.
- `sdk/python/muzen/models.py` and related Python SDK files: Python equivalents
  for runner and review contracts.

## Target Architecture

```text
Review Source
  -> Provider Materialization
  -> RepoSnapshot + changed-file manifest + diff artifact
  -> ContextEngine.index_snapshot(...)
  -> ContextIndexReport + context_manifest.json
  -> ContextEngine.build_pack(...) per session purpose
  -> context_pack.<session>.json artifacts
  -> Reviewer Kernel sessions
  -> context.* tools for follow-up
  -> findings with Context Evidence refs
  -> validator / dedupe / report
  -> context retrieval metrics and replay artifacts
```

The Context Engine sits after Provider Materialization and before reviewer
sessions. It must use the immutable snapshot as its source of truth.

## Core Interface

The first Rust interface should be small and stable:

```rust
#[async_trait::async_trait]
pub trait ContextEngine: Send + Sync {
    async fn index_snapshot(
        &self,
        request: ContextIndexRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextIndexReport>;

    async fn build_pack(
        &self,
        request: ContextPackRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextPack>;

    async fn query(
        &self,
        request: ContextQuery,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextQueryResult>;

    async fn record_feedback(
        &self,
        feedback: ContextFeedback,
        cancel: CancellationToken,
    ) -> RuntimeResult<ContextFeedbackReceipt>;
}
```

The first implementation should be:

```rust
pub struct SnapshotContextEngine {
    config: ContextEngineConfig,
    store: Arc<dyn ContextIndexStore>,
    clock: Arc<dyn ContextClock>,
}
```

The first store can be in-memory:

```rust
pub trait ContextIndexStore: Send + Sync {
    fn put_index(&self, index: ContextIndex) -> RuntimeResult<()>;
    fn get_index(&self, snapshot_id: &SnapshotId) -> Option<Arc<ContextIndex>>;
    fn remove_index(&self, snapshot_id: &SnapshotId) -> RuntimeResult<bool>;
}
```

Persistent index storage should wait until the index shape stabilizes. V0 must
persist durable artifacts, not necessarily a reusable index database.

## Proposed Module Layout

```text
src/context_engine/
  mod.rs
  engine.rs
  config.rs
  ids.rs
  evidence.rs
  pack.rs
  query.rs
  feedback.rs
  index.rs
  index/
    lexical.rs
    diff.rs
    rules.rs
    files.rs
    graph.rs
    symbols.rs
    tests.rs
    history.rs
    host.rs
  retrieval.rs
  retrieval/
    planner.rs
    ranker.rs
    compiler.rs
    sufficiency.rs
  tools.rs
  artifacts.rs
  events.rs
  metrics.rs
  store.rs
  redaction.rs
  replay.rs
  tests.rs
```

Initial phase files should be much smaller:

```text
src/context_engine/
  mod.rs
  engine.rs
  config.rs
  evidence.rs
  pack.rs
  query.rs
  index.rs
  tools.rs
  artifacts.rs
  events.rs
  metrics.rs
  store.rs
  tests.rs
```

Keep submodules internal until they are needed. The public facade should remain
the `ContextEngine` interface and serializable contract types.

## Core Types

### ContextIndexRequest

```rust
pub struct ContextIndexRequest {
    pub run_id: Option<String>,
    pub snapshot: Arc<RepoSnapshot>,
    pub review_plan: Option<ReviewPlan>,
    pub instructions: Vec<ContextInstruction>,
    pub host_metadata: BTreeMap<String, serde_json::Value>,
    pub limits: ContextLimits,
}
```

Rules:

- `snapshot` is the only source of repository file bytes.
- `review_plan` can be omitted for standalone local indexing.
- `instructions` preserve trust and source metadata.
- `host_metadata` is indexed only when explicitly allowed by config.
- `limits` must bound file count, bytes, evidence count, and token budget.

### ContextIndexReport

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexReport {
    pub index_id: ContextIndexId,
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub context_engine_version: String,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub indexed_bytes: u64,
    pub indexed_changed_files: usize,
    pub rule_count: usize,
    pub diff_hunk_count: usize,
    pub evidence_count: usize,
    pub elapsed_ms: u64,
    pub warnings: Vec<ContextIndexWarning>,
    pub artifacts: Vec<ArtifactId>,
}
```

### ContextEvidence

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEvidence {
    pub id: EvidenceId,
    pub kind: ContextEvidenceKind,
    pub source: ContextEvidenceSource,
    pub trust: ContextTrust,
    pub sensitivity: ContextSensitivity,
    pub scope: ContextScope,
    pub path: Option<RepoPath>,
    pub revision: Option<ContextRevision>,
    pub range: Option<ContextRange>,
    pub content_hash: Option<String>,
    pub summary: Option<String>,
    pub token_estimate: usize,
    pub provenance: ContextProvenance,
    pub created_at_utc: Option<String>,
    pub expires_at_utc: Option<String>,
}
```

`ContextEvidenceKind`:

```text
diff
file_span
symbol
test
config
doc
repository_rule
organization_rule
ticket
historical_pr
prior_finding
ci_failure
dependency
cross_repo_contract
tool_output
pack_summary
```

`ContextEvidenceSource`:

```text
snapshot
host
history
memory
tool
external
```

`ContextTrust`:

```text
kernel
host_trusted
organization_trusted
repository_untrusted
user_untrusted
external_untrusted
tool_provider
```

`ContextSensitivity`:

```text
public
private
secret_redacted
restricted
```

`ContextScope`:

```text
run
snapshot
workspace
repository
organization
external
```

### ContextRelationship

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextRelationship {
    pub from: EvidenceId,
    pub to: EvidenceId,
    pub kind: ContextRelationshipKind,
    pub confidence: f32,
    pub reason: String,
}
```

Relationship kinds:

```text
imports
calls
implements
tests
configures
documents
depends_on
same_symbol
similar_history
violates_rule
satisfies_ticket
contradicts
cross_repo_contract
```

### ContextPack

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPack {
    pub id: ContextPackId,
    pub run_id: Option<String>,
    pub snapshot_id: SnapshotId,
    pub session_id: Option<SessionId>,
    pub purpose: ContextPackPurpose,
    pub evidence: Vec<ContextEvidence>,
    pub relationships: Vec<ContextRelationship>,
    pub omitted_candidates: Vec<OmittedContextCandidate>,
    pub budget: ContextBudgetUsage,
    pub sufficiency: ContextSufficiency,
    pub compiler_version: String,
    pub created_at_utc: String,
}
```

Purposes:

```text
general_review
correctness
security
tests
architecture
performance
validator
standalone_query
```

### OmittedContextCandidate

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OmittedContextCandidate {
    pub evidence_id: EvidenceId,
    pub kind: ContextEvidenceKind,
    pub path: Option<RepoPath>,
    pub score: f32,
    pub token_estimate: usize,
    pub reason: ContextOmissionReason,
}
```

Omission reasons:

```text
budget_exhausted
duplicate
low_relevance
lower_trust
generated_file
binary_file
secret_redacted
outside_scope
superseded_by_summary
requires_ungranted_capability
```

### ContextQuery

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextQuery {
    pub run_id: Option<String>,
    pub snapshot_id: SnapshotId,
    pub session_id: Option<SessionId>,
    pub purpose: Option<ContextPackPurpose>,
    pub kind: ContextQueryKind,
    pub arguments: serde_json::Value,
    pub current_evidence: Vec<EvidenceId>,
    pub limits: ContextQueryLimits,
}
```

Query kinds for V0:

```text
search_text
read_span
explain_pack
related_tests
sufficiency_check
```

Later query kinds:

```text
related_symbols
related_configs
impact_graph
history_similar
rules_for_path
ticket_requirements
cross_repo_contracts
```

## Context Artifacts

All context artifacts should be normal Muzen artifacts so existing export,
redaction, persistence, and report code can handle them.

### context_manifest.json

Created by `index_snapshot`.

Required fields:

```json
{
  "schemaVersion": "muzen.context_manifest.v1",
  "contextEngineVersion": "0.1.0",
  "indexId": "ctxidx_...",
  "snapshotId": "snap_...",
  "manifestHash": "...",
  "pathPolicyHash": "...",
  "indexedFiles": 123,
  "skippedFiles": 4,
  "indexedBytes": 456789,
  "evidenceCount": 321,
  "ruleCount": 7,
  "diffHunkCount": 18,
  "skips": [
    {
      "path": "target/debug/app",
      "reason": "binary_file"
    }
  ],
  "warnings": []
}
```

### context_pack.<session>.json

Created by `build_pack`.

Required fields:

```json
{
  "schemaVersion": "muzen.context_pack.v1",
  "packId": "ctxpack_...",
  "snapshotId": "snap_...",
  "sessionId": "security-1",
  "purpose": "security",
  "compilerVersion": "0.1.0",
  "budget": {
    "maxTokens": 12000,
    "usedTokens": 7420
  },
  "sufficiency": {
    "status": "probably_sufficient",
    "missing": ["route-level middleware config"]
  },
  "evidence": [],
  "relationships": [],
  "omittedCandidates": []
}
```

### context_retrieval_log.jsonl

Appended by pack compilation and context queries.

One record per retrieval action:

```json
{
  "schemaVersion": "muzen.context_retrieval.v1",
  "timestampUtc": "2026-06-07T00:00:00Z",
  "runId": "run_...",
  "snapshotId": "snap_...",
  "sessionId": "correctness-1",
  "strategy": "related_tests",
  "query": {
    "path": "src/auth/token.rs"
  },
  "included": ["ev_1", "ev_2"],
  "omitted": [
    {
      "evidenceId": "ev_9",
      "reason": "low_relevance",
      "score": 0.18
    }
  ],
  "elapsedMs": 12
}
```

### context_findings_evidence.json

Created near report finalization.

Required fields:

```json
{
  "schemaVersion": "muzen.context_findings_evidence.v1",
  "runId": "run_...",
  "findings": [
    {
      "findingId": "finding_1",
      "primaryEvidence": ["ev_1"],
      "supportingEvidence": ["ev_2"],
      "contradictedBy": [],
      "sufficiency": "sufficient"
    }
  ]
}
```

## Runtime Events

Add context event variants to `RuntimeEvent` once the artifact schema is ready.

Candidate variants:

```rust
ContextIndexStarted {
    snapshot_id: SnapshotId,
}

ContextIndexCompleted {
    snapshot_id: SnapshotId,
    index_id: ContextIndexId,
    evidence_count: usize,
    indexed_files: usize,
    skipped_files: usize,
    ms: u64,
}

ContextPackStarted {
    session_id: Option<SessionId>,
    purpose: ContextPackPurpose,
}

ContextPackCompleted {
    pack_id: ContextPackId,
    session_id: Option<SessionId>,
    purpose: ContextPackPurpose,
    evidence_count: usize,
    omitted_count: usize,
    used_tokens: usize,
    sufficiency: ContextSufficiencyStatus,
    ms: u64,
}

ContextQueryCompleted {
    session_id: Option<SessionId>,
    query_kind: ContextQueryKind,
    result_count: usize,
    artifact_id: Option<ArtifactId>,
    ms: u64,
}
```

Event context rules:

- Index events carry `snapshot_id`.
- Pack events carry `snapshot_id` and maybe `session_id`.
- Query events carry `snapshot_id`, maybe `session_id`, and maybe
  `tool_call_id` when the query came through a tool.
- Finding evidence events carry `finding_id`.

Add JSONL fixture coverage when the event variants ship.

## Context Tools

Register context tools in the existing tool registry. They should be normal
Muzen tools with stable `ToolId`s, schemas, effects, cacheability, metrics, and
capability grants.

Use namespaced ids:

```text
context.search_text
context.read_span
context.explain_pack
context.related_tests
context.sufficiency_check
```

Do not remove existing generic tools in V0. Existing tools remain useful:

```text
list_changed_files
read_diff
read_file
read_file_range
read_base_file
read_head_file
search_text
find_related_files
find_tests_for_file
list_imports
```

The context tools add provenance, ranking, pack awareness, and explanation.

### context.search_text

Arguments:

```json
{
  "query": "authorize_request | anonymous",
  "pathGlobs": ["src/**", "tests/**"],
  "changedOnly": false,
  "maxResults": 20
}
```

Result:

```json
{
  "results": [
    {
      "evidenceId": "ev_...",
      "path": "src/auth/token.rs",
      "revision": "head",
      "range": { "startLine": 20, "endLine": 44 },
      "score": 0.82,
      "why": ["literal query match", "changed file dependency"]
    }
  ]
}
```

Effects:

```text
read_repo
write_artifact
```

Cacheable: yes. Cache key includes snapshot id, scope key, tool id, schema
digest, query args, redaction policy version, and context engine version.

### context.read_span

Arguments:

```json
{
  "path": "src/auth/token.rs",
  "revision": "head",
  "startLine": 20,
  "endLine": 44
}
```

Result:

```json
{
  "evidenceId": "ev_...",
  "path": "src/auth/token.rs",
  "revision": "head",
  "range": { "startLine": 20, "endLine": 44 },
  "contentHash": "...",
  "content": "..."
}
```

Effects:

```text
read_repo
write_artifact
```

Cacheable: yes.

### context.explain_pack

Arguments:

```json
{
  "packId": "ctxpack_...",
  "includeOmitted": true
}
```

Result:

```json
{
  "packId": "ctxpack_...",
  "purpose": "security",
  "included": [
    {
      "evidenceId": "ev_1",
      "score": 0.91,
      "why": ["auth-related changed file", "direct caller"]
    }
  ],
  "omitted": [
    {
      "evidenceId": "ev_9",
      "score": 0.13,
      "reason": "low_relevance"
    }
  ]
}
```

Effects:

```text
read_artifact
```

Cacheable: yes.

### context.related_tests

Arguments:

```json
{
  "path": "src/auth/token.rs",
  "symbol": "authorize_request",
  "maxResults": 12
}
```

V0 can use path and lexical heuristics. Symbol-aware behavior comes later.

Effects:

```text
read_repo
write_artifact
```

Cacheable: yes.

### context.sufficiency_check

Arguments:

```json
{
  "question": "Can I make a confident auth bypass finding?",
  "currentEvidence": ["ev_1", "ev_2"]
}
```

Result:

```json
{
  "status": "insufficient",
  "missing": [
    "callers of authorize_request",
    "route middleware config",
    "anonymous session tests"
  ],
  "suggestedQueries": [
    {
      "tool": "context.search_text",
      "arguments": { "query": "authorize_request" }
    }
  ]
}
```

V0 can be heuristic. Later versions can use validator sessions.

## Pack Compilation Strategy

Context Pack compilation has three layers.

### Layer A: Mandatory Session Context

Small, stable context that every session sees:

- review objective
- assigned changed files
- changed-file summary
- available context tools
- applicable kernel/host rules
- context pack id
- evidence requirement reminder

This should be a session instruction or prompt fragment. It should not include
large file contents.

### Layer B: Session-Specific Context Pack

The pack contains structured evidence selected for the session role.

Correctness pack:

- changed file spans
- direct callers/callees when known
- changed config files
- state machine or contract hints
- related tests
- prior clean/bug evidence from the current run

Security pack:

- auth and permission files
- input validation paths
- secret handling code
- network/file/process effects
- security rules
- route/middleware config
- prior security findings when available

Tests pack:

- changed files
- direct tests
- fixtures
- CI config
- test helpers
- uncovered changed paths

Architecture pack:

- public exports
- interface definitions
- dependency direction hints
- `CONTEXT.md`
- docs/RFCs touching the changed area
- cross-SDK equivalents

Performance pack:

- hot-path-looking changes
- loops and allocations
- concurrency primitives
- cache/config changes
- benchmark files

Validator pack:

- proposed findings
- primary and supporting evidence
- omitted candidates
- contradiction search results
- sufficiency status

### Layer C: Tool-Driven Follow-Up

Review sessions call context tools when the pack is insufficient. This keeps
prompts bounded while still enabling deep review.

## Ranking Model V0

Start deterministic and explainable.

Candidate score:

```text
score =
  0.25 * dependency_relevance
+ 0.20 * changed_path_or_symbol_overlap
+ 0.15 * explicit_rule_match
+ 0.15 * test_or_config_relevance
+ 0.10 * lexical_similarity
+ 0.10 * risk_weight
+ 0.05 * recency_or_manifest_priority
- token_cost_penalty
- duplicate_penalty
- generated_file_penalty
- untrusted_instruction_penalty
```

V0 can implement only the available signals:

- changed path match
- path family match
- exact identifier match
- file extension/language match
- rule path match
- test naming match
- config filename match
- risk path hint
- generated/vendor/lockfile penalty
- token cost penalty

Every selected evidence item must carry `why`.

Example:

```json
{
  "evidenceId": "ev_123",
  "score": 0.87,
  "why": [
    "same path stem as changed file",
    "test file naming match",
    "contains changed identifier authorize_request"
  ]
}
```

## Trust And Sensitivity Policy

Trust is structural, not prose.

Trust order:

```text
kernel
host_trusted
organization_trusted
repository_untrusted
user_untrusted
external_untrusted
tool_provider
```

Policy rules:

- Kernel policy cannot be overridden by repository docs, issue text, PR text,
  tool output, or external docs.
- Repository guidance is evidence and preference input, not authority.
- Issue/ticket text is untrusted intent context unless the host marks a field
  trusted.
- Tool output trust inherits from the tool provider and granted resources.
- Secret-looking content must be redacted before model visibility.
- Context artifacts must preserve whether content was redacted.
- Model-visible context must be logged in Context Packs.
- Omitted secret evidence must be represented as an omission, not silently
  ignored.

## No Evidence, No Finding

Add a publishability rule:

```text
A finding is publishable only when it cites at least one primary Context
Evidence item from source code, diff, trusted rule, test, config, ticket, or
tool output that directly supports the claim.
```

Initial enforcement should be staged:

1. Phase 4 warning mode: findings without evidence are reported as
   `validationStatus = "weak_evidence"`.
2. Phase 5 strict mode behind config: weak findings are demoted or hidden from
   publishable findings.
3. Later default strict mode once benchmarks prove the policy does not hide
   valid findings.

Evidence classes:

- Primary evidence: the code/rule/test/ticket span that directly proves the
  claim.
- Supporting evidence: related context that increases confidence.
- Contradicting evidence: context that challenges or narrows the claim.

## Standalone Surface

Standalone capability should be an adapter over the same core module.

CLI examples:

```sh
muzen context index --repo . --changed-file src/auth/token.rs
muzen context pack --repo . --changed-file src/auth/token.rs --purpose security
muzen context query --repo . --kind related_tests --path src/auth/token.rs
muzen context explain --pack context_pack.security.json
```

TypeScript SDK sketch:

```ts
const context = await muzen.context(local(".", {
  changedFiles: ["src/auth/token.rs"],
}));

const pack = await context.buildPack({ purpose: "security" });
const tests = await context.query({
  kind: "related_tests",
  arguments: { path: "src/auth/token.rs" },
});
```

HTTP sketch:

```text
POST /workspaces/:workspaceId/context/index
POST /workspaces/:workspaceId/context/packs
POST /workspaces/:workspaceId/context/query
GET  /workspaces/:workspaceId/context/artifacts/:artifactId
```

Do not build this before the review-run adapter uses the same interface.

## Implementation Phases

### Phase 0: Planning And Vocabulary

Intent:

- Record the primitive decision.
- Add official vocabulary.
- Give implementation agents a detailed blueprint.

Files:

- `CONTEXT.md`
- `muzen-context-engine/README.md`
- `muzen-context-engine/implementation-plan.md`

Acceptance:

- Context Engine, Context Evidence, and Context Pack are defined in
  `CONTEXT.md`.
- The implementation plan explains interface, modules, rollout, tests,
  events, artifacts, and standalone adapter direction.

### Phase 1: Contracts And No-Op Engine

Intent:

- Create the Rust module and serializable types without changing review
  behavior.
- Add a disabled/no-op implementation so the Reviewer Kernel can accept a
  Context Engine handle before indexing exists.

Files:

- `src/context_engine/mod.rs`
- `src/context_engine/engine.rs`
- `src/context_engine/config.rs`
- `src/context_engine/evidence.rs`
- `src/context_engine/pack.rs`
- `src/context_engine/query.rs`
- `src/context_engine/events.rs`
- `src/context_engine/artifacts.rs`
- `src/context_engine/store.rs`
- `src/lib.rs`
- `src/runtime/contracts.rs`

Work items:

- Add id wrappers: `ContextIndexId`, `ContextPackId`.
- Reuse existing `EvidenceId`, `ArtifactId`, `SnapshotId`, and `SessionId`.
- Define `ContextEvidence`, `ContextRelationship`, `ContextPack`,
  `ContextQuery`, `ContextFeedback`, `ContextIndexReport`, and warning types.
- Define `ContextEngineConfig` with `mode = disabled | snapshot_v0`.
- Define `NoopContextEngine`.
- Add a `context_engine` optional field to `RunBuilder`, but keep default
  behavior disabled.
- Add unit tests for serialization, id validation, default config, and no-op
  behavior.

Tests:

- `cargo test context_engine::`
- serde round-trip tests for every public context contract.
- Ensure `Run::builder(...).build()` still succeeds without a Context Engine.

Acceptance:

- No behavior change to existing reviews.
- Context contracts are stable enough to generate runner/SDK types later.
- The disabled engine is explicit, not represented by `None` across many call
  sites.

### Phase 2: Snapshot Index V0

Intent:

- Build a deterministic, no-vector index from `RepoSnapshot`.
- Produce `context_manifest.json`.
- Avoid live filesystem reads after snapshot capture.

Files:

- `src/context_engine/index.rs`
- `src/context_engine/index/lexical.rs` or inline V0 module
- `src/context_engine/index/diff.rs` or inline V0 module
- `src/context_engine/index/rules.rs` or inline V0 module
- `src/context_engine/artifacts.rs`
- `src/context_engine/metrics.rs`
- `src/runtime/repo.rs` only if existing snapshot accessors are insufficient
- `src/reviewer/run.rs`

Work items:

- Iterate `RepoSnapshot.manifest.files`.
- Index only files with `SnapshotCaptureStatus::Captured`.
- Build line tables with byte offsets and content hashes.
- Build a literal token index for identifiers, path segments, config keys, and
  simple words.
- Build changed-file evidence from `changed_file_entries`.
- Build diff evidence from `snapshot.diff.content`.
- Discover repository guidance files from captured snapshot bytes:
  - `CONTEXT.md`
  - `AGENTS.md`
  - nested `AGENTS.md`
  - `Readme.md` / `README.md`
  - docs/RFC files when changed or path-relevant
  - `.github/copilot-instructions.md`
  - `.cursorrules`
- Classify generated/vendor/lock/binary/unavailable files.
- Emit `ContextIndexReport`.
- Write `context_manifest.json` as an artifact.
- Store the in-memory index keyed by snapshot id and manifest hash.

Tests:

- Snapshot with captured text files indexes expected paths.
- Binary/skipped files produce skip records.
- Changed files produce diff and changed-file evidence.
- `CONTEXT.md` is discovered as repository guidance.
- No test reads files directly from the live worktree after snapshot creation.
- Index output is deterministic across repeated runs.

Acceptance:

- A local review can produce `context_manifest.json`.
- Indexing respects snapshot path policy and storage policy.
- Indexing has bounded memory and reports skipped bytes/files.

### Phase 3: Context Tools V0

Intent:

- Register context query tools that use the Context Engine.
- Keep existing read/search tools available.

Files:

- `src/context_engine/tools.rs`
- `src/runtime/tools/catalog.rs`
- `src/runtime/tools/registry.rs`
- `src/runtime/tools/provider.rs`
- `src/runtime/tools/engine.rs`
- `src/reviewer/tools.rs`
- `src/tests/support.rs`

Work items:

- Add tool ids:
  - `context.search_text`
  - `context.read_span`
  - `context.explain_pack`
  - `context.related_tests`
- Add schemas and descriptions.
- Decide whether context tools are a new built-in provider id or registered
  in-process tools backed by the Context Engine.
- Pass a `ContextEngine` handle into `ToolEngine`.
- Ensure tool authorization uses normal `CapabilitySet` grants.
- Ensure result cache keys include context engine version and scope key.
- Emit `ContextQueryCompleted` events or tool `details` with context metadata.
- Write query result artifacts when output is large.

Tests:

- Sessions without grants cannot call context tools.
- Sessions with grants can call context tools.
- `context.read_span` validates path and range.
- `context.search_text` honors file scope.
- `context.related_tests` returns deterministic heuristic results.
- Tool metrics include context tools with dynamic `ToolMetricKey`.
- Cached context tool calls do not leak across scope keys.

Acceptance:

- Review sessions can use context tools through normal model tool calls.
- Context tools are observable, bounded, redacted, and capability-secured.

### Phase 4: Pack Compiler And Session Integration

Intent:

- Build role-specific Context Packs and make them available to sessions.
- Persist exact model-visible context as artifacts.

Files:

- `src/context_engine/retrieval.rs`
- `src/context_engine/retrieval/planner.rs` or inline V0 module
- `src/context_engine/retrieval/ranker.rs` or inline V0 module
- `src/context_engine/retrieval/compiler.rs` or inline V0 module
- `src/context_engine/pack.rs`
- `src/runtime/planned_units.rs`
- `src/reviewer/run.rs`
- `src/reviewer/report.rs`

Work items:

- Map `Role` to `ContextPackPurpose`.
- Build packs for each `SessionScope` before `PlannedReviewRuntime` starts.
- Add a compact pack summary to session instructions.
- Store full pack as `context_pack.<session>.json`.
- Include pack id in session-visible instructions.
- Add retrieval log records for selected and omitted evidence.
- Track budget usage.
- Track sufficiency status.
- Keep pack compilation deterministic.

Tests:

- Security and tests sessions receive different packs for the same snapshot.
- Omitted candidates are stored with reasons.
- Pack budget is enforced.
- Context Pack artifacts are exported with normal artifact export policy.
- Existing review flows still pass when context mode is disabled.

Acceptance:

- Each review session can be tied to the exact Context Pack it saw.
- The pack explains why evidence was included and omitted.

### Phase 5: Evidence-Backed Findings

Intent:

- Make Context Evidence the normal support structure for findings.
- Start with warning mode, then add strict mode behind config.

Files:

- `src/context_engine/evidence.rs`
- `src/context_engine/retrieval/sufficiency.rs`
- `src/runtime/planned_units.rs`
- `src/reviewer/report.rs`
- `src/runner/types.rs`
- `src/runner/schema.rs`
- `sdk/typescript/packages/muzen-sdk/src/types.ts`
- `sdk/python/muzen/models.py`

Work items:

- Extend internal finding models if needed to reference `EvidenceId`s.
- Reuse existing SDK `ReviewFindingEvidence` shape where possible.
- Add `contextSufficiency` or `validationStatus` values:
  - `sufficient`
  - `weak_evidence`
  - `insufficient`
- Create `context_findings_evidence.json`.
- Add warning-mode policy for findings without primary evidence.
- Add config-gated strict mode.
- Update validator prompts/tools to challenge weak evidence.

Tests:

- Finding with primary evidence remains publishable.
- Finding without evidence becomes weak in warning mode.
- Finding without evidence is demoted or hidden in strict mode.
- Report evidence lookup returns referenced Context Evidence artifacts.
- Runner schema fixtures include evidence refs.
- TypeScript and Python SDK round-trip evidence fields.

Acceptance:

- No publishable finding can be untraceable when strict mode is enabled.
- Warning mode provides migration safety.

### Phase 6: Standalone CLI, SDK, And HTTP Adapters

Intent:

- Expose the same Context Engine interface outside full review runs.
- Avoid duplicate indexing/retrieval logic.

Files:

- `src/cli.rs`
- `src/service.rs`
- `src/review_session/http.rs` or a new context router module
- `src/runner/types.rs`
- `src/runner/schema.rs`
- `sdk/typescript/packages/muzen-sdk/src/types.ts`
- `sdk/typescript/packages/muzen-sdk/src/index.ts`
- `sdk/python/muzen/*.py`
- `Readme.md`

Work items:

- Add `muzen context index`.
- Add `muzen context pack`.
- Add `muzen context query`.
- Add `muzen context explain`.
- Add runner protocol methods only if SDK local mode needs them:
  - `context.index`
  - `context.pack`
  - `context.query`
- Add HTTP routes behind workspace auth:
  - `POST /workspaces/:workspaceId/context/index`
  - `POST /workspaces/:workspaceId/context/packs`
  - `POST /workspaces/:workspaceId/context/query`
- Add SDK methods after the route/protocol shape is stable.

Tests:

- CLI can build a pack for a local repo.
- HTTP routes enforce auth and workspace scoping.
- SDK local mode and remote mode return the same contract shape.
- Standalone indexing produces the same manifest as review-run indexing for
  the same snapshot.

Acceptance:

- Users can inspect context without paying for a full model review.
- Standalone adapters do not bypass snapshot policy or trust labels.

### Phase 7: Symbol And Test Impact

Intent:

- Improve retrieval beyond lexical/path heuristics.
- Start with Rust, TypeScript, and Python because Muzen already has all three.

Files:

- `src/context_engine/index/symbols.rs`
- `src/context_engine/index/graph.rs`
- `src/context_engine/index/tests.rs`
- `src/context_engine/retrieval/planner.rs`
- `Cargo.toml`

Work items:

- Evaluate tree-sitter crates and licensing.
- Extract symbols for Rust, TypeScript/TSX, Python.
- Extract imports and exports.
- Build simple reference maps from exact identifier matches.
- Detect likely tests by naming, imports, path conventions, and symbol overlap.
- Add `context.related_symbols`.
- Improve `context.related_tests`.
- Add graph relationships:
  - `imports`
  - `tests`
  - `same_symbol`
  - `depends_on`

Tests:

- Rust changed function maps to tests with matching identifiers.
- TypeScript exported type changes map to imports/examples.
- Python module changes map to test files.
- Symbol index failure degrades to lexical retrieval.
- Generated files are not over-prioritized.

Acceptance:

- Public API changes trigger architecture context.
- Changed symbols map to likely tests and direct importers.

### Phase 8: Host And Ticket Context

Intent:

- Include issue/ticket/product intent without making host-specific fields core.

Files:

- `src/context_engine/index/host.rs`
- `src/context_engine/query.rs`
- `src/runner/types.rs`
- `sdk/typescript/packages/muzen-sdk/src/types.ts`
- provider/host adapter modules as needed

Work items:

- Define host context input as typed evidence records, not provider-specific
  fields.
- Support issue title/body, acceptance criteria, labels, linked incidents,
  release targets, and host-provided trusted fields.
- Add `context.ticket_requirements`.
- Add trust labels per field.
- Add ticket compliance pack purpose later if needed.

Tests:

- Untrusted ticket text cannot override kernel policy.
- Trusted host policy fields are ranked above repository comments.
- Ticket requirements can support a finding only when combined with code
  evidence for code claims.

Acceptance:

- Reviewers can cite ticket evidence for scope/intent claims.
- Muzen core stays provider-neutral.

### Phase 9: History And Feedback

Intent:

- Use prior review outcomes and maintainer feedback without creating spooky
  memory.

Files:

- `src/context_engine/feedback.rs`
- `src/context_engine/index/history.rs`
- `src/context_engine/store.rs`
- review-session store modules if persistence is needed

Work items:

- Define `ContextLearning`.
- Define feedback sources:
  - accepted finding
  - dismissed finding
  - human feedback
  - merged PR
  - manual rule
- Store proposed learnings as artifacts first.
- Require explicit approval before repository/workspace/org learnings become
  active.
- Add expiration and scope.
- Add `context.history_similar`.

Tests:

- Dismissed finding can create a proposed learning.
- Proposed learning does not affect packs until approved.
- Expired learning is not applied.
- Repository learning does not become organization learning automatically.

Acceptance:

- Repeated false positives can be suppressed with explanation.
- Feedback remains inspectable and reversible.

### Phase 10: Cross-Repo Context

Intent:

- Support API/schema/contract impact across linked repositories.

Files:

- `src/context_engine/index/host.rs`
- `src/context_engine/index/graph.rs`
- provider adapters or host tool registrations

Work items:

- Define linked repository config as host-provided evidence.
- Add cross-repo evidence source and trust metadata.
- Add `context.cross_repo_contracts`.
- Keep network reads behind explicit capabilities.
- Store cross-repo omissions when access is not granted.

Tests:

- Without network/read grants, cross-repo query returns capability omission.
- With grants, cross-repo result is cited and scoped.
- Cross-repo evidence cannot be confused with current snapshot evidence.

Acceptance:

- Breaking contract findings can cite affected consumers.
- Provider-specific assumptions do not leak into core.

### Phase 11: Pluggable Embeddings

Intent:

- Add semantic retrieval as an optional adapter, not a required runtime.

Files:

- `src/context_engine/index/semantic.rs`
- `src/context_engine/retrieval/ranker.rs`
- SDK/provider config files as needed

Work items:

- Define `EmbeddingProvider` trait.
- Define vector index adapter trait.
- Support no-vector mode.
- Support local embedding mode.
- Support hosted embedding provider mode.
- Add semantic score as one ranking signal, never the only signal.

Tests:

- Context Engine works with no embeddings.
- Semantic retrieval improves conceptual matches in benchmark fixtures.
- Embedding provider failures degrade to lexical/graph retrieval.
- Embedding inputs respect redaction and sensitivity policy.

Acceptance:

- Embeddings improve recall without becoming mandatory.
- Hosted embeddings never receive unredacted restricted evidence unless config
  explicitly permits it.

### Phase 12: Context Evaluation Suite

Intent:

- Benchmark retrieval quality directly.

Files:

- `bench/context-engine/`
- `bench/results-context-engine/`
- `fixtures/context-engine/`

Metrics:

- context recall
- context precision
- evidence coverage
- sufficiency calibration
- false positive rate
- finding acceptance proxy
- useful evidence per 1k tokens
- index latency
- query latency
- pack compile latency
- cache hit rate
- prompt injection resistance
- secret redaction correctness

Work items:

- Create seeded PR fixtures.
- Define retrieval gold files.
- Add false-positive fixtures.
- Add prompt-injection fixtures.
- Add replay tests for Context Packs.
- Add benchmark summary script.

Tests:

- Pack includes known required evidence for seeded bugs.
- Pack omits irrelevant generated/vendor files.
- Prompt injection text is labeled untrusted.
- Secret-looking text is redacted before pack visibility.

Acceptance:

- Retrieval/ranking changes can be regression-tested without full model runs.

## MVP PR Slice

Recommended implementation slices:

1. PR 1: `CONTEXT.md` vocabulary and plan.
2. PR 2: contracts, no-op engine, config, serde tests.
3. PR 3: snapshot index V0 and `context_manifest.json`.
4. PR 4: context tools V0 with capability grants.
5. PR 5: role-specific Context Packs and artifacts.
6. PR 6: warning-mode evidence-backed findings.
7. PR 7: standalone CLI preview.
8. PR 8: symbol/test impact V1.

Each PR should be independently shippable and should leave existing review
behavior unchanged unless context mode is explicitly enabled.

## Configuration

Initial config:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEngineConfig {
    pub mode: ContextEngineMode,
    pub max_indexed_files: usize,
    pub max_indexed_bytes: usize,
    pub max_evidence_items: usize,
    pub max_pack_tokens: usize,
    pub max_query_results: usize,
    pub include_repository_guidance: bool,
    pub include_host_context: bool,
    pub strict_evidence_required: bool,
}
```

Defaults:

```text
mode: disabled for first merged contract PR, snapshot_v0 after integration
max_indexed_files: inherit path policy max_directory_entries
max_indexed_bytes: inherit snapshot storage captured bytes
max_evidence_items: 5000
max_pack_tokens: 12000
max_query_results: inherit runtime max_search_matches
include_repository_guidance: true
include_host_context: false in V0
strict_evidence_required: false
```

## Backward Compatibility

- Existing review runs must work when Context Engine mode is disabled.
- Existing runner protocol frames must remain valid.
- Existing SDK `ReviewFindingEvidence` fields should be reused or extended in a
  backward-compatible way.
- Existing review-event JSONL loaders need fixture updates only when context
  events are added.
- Existing artifact export should include context artifacts under normal
  retention policy.

## Failure Modes

Context indexing failure:

- If mode is optional, emit warning and continue review without context packs.
- If mode is required, fail the run before model calls.

Pack compilation failure:

- Session can continue with mandatory minimal context only when config allows.
- Emit context pack failure event and artifact if possible.

Context tool query failure:

- Return normal tool error envelope.
- Do not panic or poison the whole review run.

Redaction failure:

- Treat as hard failure for model-visible context.

Evidence policy failure:

- Warning mode: mark finding weak.
- Strict mode: demote or hide finding from publishable results.

## Security Checklist

- No live worktree reads after snapshot capture.
- No untrusted instruction can override kernel policy.
- Every evidence item has trust and sensitivity.
- Every model-visible evidence item has provenance.
- Secret-looking evidence is redacted before model visibility.
- Context tools obey `CapabilitySet`.
- Context cache keys include scope.
- Context artifacts export under existing artifact policy.
- Network/host/external context requires explicit grants.
- Omitted restricted evidence is represented as an omission.

## Test Matrix

Unit tests:

- id parsing
- serde contracts
- evidence construction
- token estimates
- ranking order
- omission reasons
- trust/sensitivity propagation
- config defaults

Snapshot tests:

- captured text files
- skipped binary files
- skipped large files
- changed-file manifests
- inline diffs
- path policy denied files
- content-addressed snapshot storage
- remote object snapshot storage

Tool tests:

- authorization denial
- query success
- invalid args
- cache hits
- scope isolation
- redaction
- artifact creation

Pack tests:

- purpose-specific evidence
- budget enforcement
- omitted candidates
- sufficiency status
- deterministic output
- artifact export

Review integration tests:

- review run with context disabled
- review run with context enabled
- multi-session packs
- validator pack sees findings
- weak finding warning mode
- strict evidence mode

Runner/SDK tests:

- schema generation includes context types
- TypeScript round-trip
- Python round-trip
- local runner context query
- remote HTTP context query once routes exist

Replay tests:

- same snapshot plus same config gives same pack
- retrieval log can explain every selected evidence item
- context event JSONL export/load handles context variants

## Open Questions For Review

1. Should Context Pack summaries be injected as session instructions, or should
   sessions receive only a pack id and use `context.explain_pack`?
2. Should context tools replace existing `search_text` and `read_file_range`
   long term, or stay as higher-level alternatives?
3. What is the first strict evidence default: warning-only forever, or strict
   after benchmarks meet a threshold?
4. Should context indexes be persisted in Postgres, object storage, or only as
   artifacts for the first service-backed release?
5. Should repository guidance discovery follow Codex-style nested instruction
   precedence exactly, or use simpler path-specific matching first?
6. Should the first standalone adapter be CLI-only, or should SDK local mode be
   added at the same time?
7. Is `Context Engine` the final product term, or should the external surface
   say `Evidence Engine` while core remains `context_engine`?

## Review Checklist

- The primitive has one small external interface.
- The implementation hides indexing, ranking, and pack compilation details.
- Review runs and standalone adapters share the same core module.
- Context is compiled from snapshots, not live filesystem state.
- Every evidence item has provenance, trust, and sensitivity.
- Every omission has a reason.
- Context artifacts are durable and exportable.
- Context events are replayable.
- Context tools use normal tool grants and metrics.
- Findings can cite Context Evidence.
- MVP works without embeddings.
- Future embeddings are adapter-based and optional.
