# Argus-Like Review Flow Plan

Generated: 2026-06-06

## Goal

Muzen should adopt the review flow shape that already works well in Argus:

```text
change manifest
  -> deterministic review plan
  -> deterministic review units
  -> bounded exploration per unit/file
  -> focused review synthesis
  -> central validation, dedupe, metrics, and host projection
```

The current Muzen runtime has strong primitives: immutable snapshots,
concurrent sessions, scoped tools, artifacts, event streams, capability gates,
and finding validation. The weak part is the review workflow shape. Too much
review planning, file coverage, evidence collection, finding emission, and
session completion behavior is packed into the model-facing prompt and terminal
tool protocol.

The target is not to copy Argus product concepts into Muzen core. The target is
to copy the shape:

- deterministic planning before model work
- review units instead of implicit changed-file batch prompts
- bounded exploration before synthesis
- small model-facing tool surface
- structured unit outputs
- central validation and aggregation

## Current Muzen Flow

Today the runner path does this:

1. Materializes source and changed files in `src/runner/execution.rs`.
2. Builds one or more `ReviewSessionSpec` values.
3. Splits large reviews into changed-file batches with
   `expand_sessions_for_changed_file_batches`.
4. Runs sessions concurrently through `src/runtime/job_runtime.rs`.
5. Each session enters `src/runtime/session_loop.rs`.
6. `ReviewerPolicy::deterministic_bootstrap_tool_calls` injects initial
   `read_diff`, `list_changed_files`, changed-file reads, and a risk search.
7. The model explores and must call `record_finding`, `record_file_review`,
   and `finish`.
8. `ReviewerPolicy` and `ToolEngine` enforce evidence, scope, finding, and
   file-review constraints.
9. Findings are produced as side effects of `record_finding` tool calls and
   collected from the shared finding store.

This is functional, but the model is doing too much workflow bookkeeping.

## Argus Reference Shape

Argus has a clearer split:

- `packages/review/domain/src/planner/review-plan.ts` classifies files from the
  diff manifest.
- `packages/review/domain/src/planner/review-units.ts` sorts and batches files
  into review units.
- `packages/argus-agent/src/agent/explore-runner.ts` runs bounded deterministic
  and optionally model-enhanced exploration per file.
- `packages/argus-agent/src/tool/registry.ts` keeps the main review tool
  surface small: search/read/range/list/grep/diff/trace/delegate.
- The review workflow builds context, augments it, and then asks the reviewer
  to synthesize issues from prepared evidence.

Muzen should preserve provider-neutral boundaries while adopting this control
flow.

## Target Architecture

```text
Run
  -> materialize source snapshot
  -> build ReviewPlan
  -> build ReviewUnitPlan
  -> execute ReviewUnit workers concurrently
       -> deterministic exploration
       -> optional enhanced exploration
       -> focused review synthesis
       -> structured ReviewUnitResult
  -> validate unit results
  -> dedupe findings
  -> aggregate file verdicts, evidence, metrics, artifacts
  -> emit ReviewRunResult
```

### New Core Concepts

#### `ReviewPlan`

Provider-neutral classification of changed files.

```rust
pub struct ReviewPlan {
    pub snapshot_id: SnapshotId,
    pub change_id: String,
    pub counts: ReviewPlanCounts,
    pub files: Vec<PlannedReviewFile>,
}

pub struct PlannedReviewFile {
    pub file_id: String,
    pub path: RepoPath,
    pub status: ChangedFileStatus,
    pub content_state: PlannedFileContentState,
    pub estimated_bytes: Option<u64>,
    pub mode: ReviewPlanFileMode,
    pub score: u8,
    pub reasons: Vec<ReviewPlanReason>,
}

pub enum ReviewPlanFileMode {
    Full,
    Excluded,
}
```

Rules should initially mirror the Argus planner spirit:

- exclude unavailable, binary, generated, vendored, lockfile, build output, and
  ignored paths
- score security, billing, persistence, API surface, domain model, migration,
  workflow, auth, permissions, async/control-flow, rendering/template, and
  integration boundary files higher
- keep reasons structured so hosts can display and audit why a file was
  reviewed or skipped

#### `ReviewUnitPlan`

Deterministic execution units derived from the plan.

```rust
pub struct ReviewUnitPlan {
    pub counts: ReviewUnitPlanCounts,
    pub units: Vec<PlannedReviewUnit>,
}

pub struct PlannedReviewUnit {
    pub id: ReviewUnitId,
    pub mode: ReviewUnitMode,
    pub file_paths: Vec<RepoPath>,
    pub score_range: ScoreRange,
    pub estimated_bytes: u64,
    pub file_count: usize,
    pub requires_further_split: bool,
}
```

Units should be sorted by score descending, then path ascending, and split by:

- max files per unit
- max estimated bytes per unit
- optional high-risk single-file isolation
- optional generated/unavailable skip units for audit only

#### `ExplorationPlan`

Bounded per-file or per-unit exploration plan.

```rust
pub struct ExplorationPlan {
    pub unit_id: ReviewUnitId,
    pub actions: Vec<ExplorationAction>,
    pub summary: String,
    pub mode: ExplorationMode,
    pub warnings: Vec<String>,
}

pub enum ExplorationAction {
    ReadDiff { path: RepoPath },
    ReadHeadRange { path: RepoPath, start_line: u32, end_line: u32 },
    ReadHeadFile { path: RepoPath },
    ReadBaseFile { path: RepoPath },
    SearchText { query: String },
    FindRelatedFiles { path: RepoPath },
    FindTestsForFile { path: RepoPath },
    ListImports { path: RepoPath },
    Custom { tool_id: ToolId, args: serde_json::Value },
}
```

The first implementation should be deterministic:

- read the unit diff
- read changed ranges for each file
- read deleted/base content when needed
- search for symbols and suspicious changed identifiers
- find related files/tests/imports for high-risk files

Optional model-enhanced exploration can come later and must be bounded by
action count, tool allowlist, and timeout.

#### `ReviewUnitResult`

The model should return a structured unit result instead of driving terminal
review bookkeeping tools.

```rust
pub struct ReviewUnitResult {
    pub unit_id: ReviewUnitId,
    pub file_verdicts: Vec<FileReviewVerdict>,
    pub findings: Vec<CandidateFinding>,
    pub evidence: Vec<EvidenceRef>,
    pub summary: String,
    pub warnings: Vec<String>,
}

pub struct FileReviewVerdict {
    pub path: RepoPath,
    pub verdict: FileVerdictKind,
    pub summary: String,
    pub related_paths: Vec<RepoPath>,
}

pub enum FileVerdictKind {
    Clean,
    IssueFound,
    Skipped,
}
```

The existing `record_finding` validation logic should move behind a
post-processing validator where possible. The model can still use tool calling
for exploration, but it should not need to call `record_file_review` or
`finish`.

## Desired Model-Facing Tool Surface

The default unit reviewer should see a small tool set:

- `read_diff`
- `read_file_range`
- `read_head_file`
- `read_base_file`
- `search_text`
- `find_related_files`
- `find_tests_for_file`
- selected host callback tools

Avoid exposing these by default to the synthesis model:

- `list_files`
- `list_changed_files`
- `list_imports` unless needed by the unit mode
- `record_finding`
- `record_file_review`
- `finish`
- `challenge_finding`

Those can remain as compatibility tools while the new pipeline is introduced,
but the new path should make them unnecessary for normal operation.

## Execution Flow

### 1. Materialize Snapshot

Keep the existing source provider and snapshot materialization path. The output
needed by planning is:

- changed-file manifest
- diff manifest
- content availability
- estimated bytes
- generated/binary/deleted flags
- optional host ignore paths

### 2. Build Review Plan

Add a `review_plan` module that converts snapshot/change data into a
`ReviewPlan`.

The planner must be deterministic, testable, and independent of models.

Inputs:

- snapshot manifest
- change spec
- path policy
- host ignore paths
- optional host planner rules

Outputs:

- included files with score/reasons
- excluded files with reasons
- plan counts
- plan events

### 3. Build Review Units

Add a `review_units` module that converts a `ReviewPlan` into
`ReviewUnitPlan`.

Initial defaults:

- max files per full unit: 4
- max estimated bytes per full unit: 80 KB
- high-risk files can be isolated when score >= 80
- oversized files become one unit with `requires_further_split = true`

### 4. Execute Exploration

Add a `unit_exploration` module.

The deterministic explorer executes actions directly through the tool engine or
read/search services, not through model tool calls. It records artifacts and
evidence like normal tools so the audit trail stays intact.

Later, add optional bounded enhancement:

```text
deterministic plan
  -> model proposes at most N extra actions from an allowlist
  -> validate/dedupe/filter actions
  -> execute actions
```

This mirrors Argus' exploration runner without requiring Argus-specific tools.

### 5. Run Unit Review Synthesis

Add a `ReviewUnitRunner` that gives the model:

- unit metadata
- assigned file paths
- changed diff excerpts
- exploration artifacts/summaries
- related-file evidence
- repository/host instructions
- risk reasons from the planner
- strict output schema

The model returns `ReviewUnitResult`.

This runner should support both:

- callback model via runner protocol
- hosted model router

### 6. Validate Unit Results

Add a central `ReviewResultValidator`.

Validation rules:

- every included file in the unit has exactly one verdict
- verdict paths must belong to the unit
- findings must point at an included changed file
- finding line ranges must overlap changed or reviewable lines when possible
- `IssueFound` verdicts must have at least one matching finding
- `Clean` verdicts must not describe a concrete bug
- skipped verdicts require a concrete inspectability reason
- candidate findings must include evidence refs from the unit
- duplicate findings within a unit are merged or rejected

This should reuse existing useful logic from:

- `ReviewerPolicy::finding_scope_denial`
- `ReviewerPolicy::file_review_scope_denial`
- `ToolEngine::record_finding_result`
- finding store/evidence construction

### 7. Aggregate Run Result

The run aggregator should combine:

- validated findings
- file verdicts
- skipped/excluded file audit data
- per-unit metrics
- per-unit exploration warnings
- model/tool metrics
- artifacts/evidence

The final result should still project to current runner protocol shapes for
compatibility.

## Compatibility Strategy

Do this as a new execution mode first.

```rust
pub enum ReviewExecutionMode {
    SessionLoopV1,
    PlannedUnitsV2,
}
```

Defaults:

- CLI and benchmarks keep `SessionLoopV1` until parity tests pass.
- SDK/runner can opt into `PlannedUnitsV2`.
- Argus adapter should target `PlannedUnitsV2`.
- Keep old terminal tools registered while V1 exists.

Once V2 is proven:

- make `PlannedUnitsV2` default
- keep `SessionLoopV1` as legacy for one release window
- remove terminal tools from default model exposure
- eventually delete or internalize `record_file_review` and `finish`

## Implementation Phases

### Phase 1: Planning Domain

Add provider-neutral planning types and deterministic planner modules.

Files likely touched:

- `src/review_plan.rs`
- `src/review_units.rs`
- `src/reviewer.rs`
- `src/runner/types.rs`
- `src/runner/schema.rs`

Acceptance:

- unit tests cover path normalization, exclusions, scoring, reason counts, and
  deterministic sorting
- review unit tests cover batching by score, file count, bytes, and oversized
  files
- runner events can report plan/unit counts

### Phase 2: V2 Execution Skeleton

Add `PlannedUnitsV2` mode that builds a plan and unit plan, emits events, and
returns an empty validated result without changing V1 behavior.

Files likely touched:

- `src/runner/execution.rs`
- `src/runtime/job_runtime.rs` or a new `src/runtime/review_units.rs`
- `src/contracts.rs`
- `fixtures/runner-schema-v1.json`

Acceptance:

- existing tests pass
- V1 output unchanged
- V2 can be selected in tests and emits plan/unit lifecycle events

### Phase 3: Deterministic Exploration

Add deterministic unit exploration that uses existing snapshot read/search
services and artifact/evidence stores.

Files likely touched:

- `src/runtime/unit_exploration.rs`
- `src/runtime/tools/engine.rs`
- `src/runtime/tools/read.rs`
- `src/runtime/tools/search.rs`
- `src/runtime/dispatch.rs`

Acceptance:

- exploration reads every assigned changed file/range when available
- deleted/binary/unavailable files produce skipped evidence
- exploration actions are bounded and deterministic
- artifacts and metrics are recorded

### Phase 4: Structured Unit Synthesis

Add a unit-review model call with strict structured output.

Files likely touched:

- `src/runtime/unit_review.rs`
- `src/runtime/model_turn.rs`
- `src/runtime/policy.rs`
- `src/runner/execution.rs`

Acceptance:

- model receives unit evidence, not a broad open-ended prompt
- model returns `ReviewUnitResult`
- parse failures are retryable once, then produce a unit failure diagnostic
- no `record_file_review` or `finish` call is required

### Phase 5: Result Validation And Aggregation

Move review-output validation out of terminal tools and into a central
validator.

Files likely touched:

- `src/runtime/review_result_validator.rs`
- `src/runtime/tools/store.rs`
- `src/reviewer.rs`
- `src/contracts.rs`

Acceptance:

- invalid paths, missing verdicts, missing evidence, and mismatched findings
  are rejected
- valid findings become normal `FindingV1`
- file verdicts and skipped files are included in run diagnostics
- dedupe works across units

### Phase 6: SDK And Argus Adapter Shape

Expose V2 plan/unit controls through SDK and runner protocol.

Files likely touched:

- `sdk/typescript/packages/muzen-sdk/src/types.ts`
- `sdk/python/muzen`
- `src/runner/types.rs`
- `src/runner/schema.rs`
- `docs/provider-neutral-review-swarm-engine-plan.md`

Acceptance:

- SDK hosts can pass planner options and execution mode
- SDK hosts receive plan/unit/progress events
- Argus adapter can map Argus manifest/planner preferences to Muzen options
- Argus-specific fields stay in adapter metadata, not core

### Phase 7: Make V2 Default

After parity tests and Argus adapter proof:

- make `PlannedUnitsV2` default for runner reviews
- keep `SessionLoopV1` behind an explicit legacy option
- stop exposing terminal tools to the model by default
- simplify `ReviewerPolicy` down to prompt construction, tool exposure, and
  validation helpers

## Tests And Quality Gates

Minimum test suite:

- planner unit tests
- review-unit batching tests
- deterministic exploration tests
- structured-output parser tests
- result validator tests
- V1 compatibility tests
- V2 end-to-end mock model tests
- runner protocol fixture tests
- large-review batching tests
- skipped/unavailable/binary/deleted file tests
- duplicate finding dedupe tests
- event JSONL compatibility tests

Benchmarks:

- 50 and 100 session/unit mock runs should keep existing memory and runtime
  characteristics within an agreed envelope
- search/read singleflight should still collapse duplicate exploration work
- V2 should reduce model tool-call count versus V1 on large reviews

## Non-Goals

- Do not move Argus queues, leases, persistence, SSE, or product read models
  into Muzen.
- Do not add Argus-specific fields to Muzen core contracts.
- Do not require a model to decide which changed files deserve review.
- Do not remove existing custom tool or source provider capabilities.
- Do not delete V1 until V2 has parity proof.

## Open Decisions

- Should high-risk files always become single-file units, or only when the
  unit byte/file budget would otherwise group them?
- Should optional model-enhanced exploration ship in V2.0, or should V2.0 be
  deterministic-only?
- Should file verdicts become public first-class result data, or remain
  diagnostics until the host contracts stabilize?
- Should `record_finding` remain as an advanced compatibility tool for custom
  sessions, or become fully internal after V2 becomes default?
- What exact planner rule set should be shared with Argus versus implemented as
  Muzen's provider-neutral default?

## Recommended First Slice

Start with Phase 1 and Phase 2 together:

1. Add `ReviewPlan`, `PlannedReviewFile`, `ReviewUnitPlan`, and
   `PlannedReviewUnit`.
2. Port the Argus deterministic classification and unit-batching shape into
   provider-neutral Rust.
3. Add `ReviewExecutionMode::PlannedUnitsV2`.
4. Make V2 build and emit the plan/unit plan without changing review results.

That gives us a reviewable architectural checkpoint before touching the model
loop or finding pipeline.
