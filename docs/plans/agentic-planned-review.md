# Agentic Planned Review Plan

Generated: 2026-06-12 after grilling the planned-review defaults and large-PR behavior.

## Core Direction

Planned review should behave like a productized reviewer, not like a narrowly budgeted LLM prompt.

The model should drive exploration inside guardrails. The guardrails are read-only tool permissions, hard caller caps, evidence requirements, challenge passes, coverage accounting, and publishability checks. They should not be tiny default budgets that make the model stop before it can discover cross-file contracts.

Direct sessions and planned review should intentionally differ:

- Direct sessions are the raw model/tool runtime. They stay caller-controlled and generic.
- Planned review is the review product. It owns bootstrap, lens selection, adaptive budgets, contract packs, final synthesis, challenge, diagnostics, and coverage reporting.

Muzen is unreleased, so do not preserve weak review behavior with compatibility modes. Make the stronger planned-review path the only planned-review path.

## Why The Current Defaults Are Wrong

The current planned-unit fallback budget of roughly `2 turns / 8 tools` is too tight for real review work.

That shape can work for a tiny local bug, but it is structurally bad for review because many important failures require at least one of:

- reading a caller and callee together
- checking a changed return shape against existing call sites
- finding tests or fixtures that define the contract
- tracing a filter, credential, tenant, or destructive-operation boundary
- comparing base/head behavior across multiple files

If the runtime stops exploration before those checks can happen, the model will produce low-confidence summaries or miss the defect entirely. The answer is not literal unlimited access; it is generous bounded autonomy with strict evidence and publication gates.

## Principles

1. Model-driven exploration by default.
   The reviewer decides where to look next within a read-only capability boundary.

2. Budgets prevent runaway work, not ordinary investigation.
   Defaults should be high enough for normal cross-file reasoning. Explicit caller budgets remain hard caps.

3. Evidence decides publishability.
   A finding that cannot cite the changed code and the relevant contract should not be published.

4. Coverage must be honest.
   A sampled review can be useful, but it must be reported as sampled. It must not look like a clean full review.

5. Challenge must be tool-enabled.
   A text-only second opinion is not enough for top-tier review. The challenger needs scoped read/search tools and prior evidence.

6. Large PRs scale by tiering and fan-out.
   A 50k-line PR cannot be reviewed as one tiny session. The runtime should create more planned units, more lenses, and more contract packs, while reporting what was sampled or insufficient.

## Runtime Shape

### Direct Session Runtime

Direct sessions should keep their current role as the generic primitive:

- caller-provided session spec
- caller-provided tools and budget
- no automatic review lens panel
- no mandatory contract packs
- no automatic challenge or publishability policy unless the caller asks for it

This is the low-level primitive for custom agents and tests.

### Planned Review Runtime

Planned review should become an opinionated workflow:

- deterministic bootstrap when useful
- default review lens panel when the caller provides no sessions
- adaptive budgets based on PR size and risk
- generated contract packs for high-value contracts
- final synthesis from review evidence and diagnostics
- tool-enabled challenge of candidate findings
- publishable-only final findings
- compact audit artifact for rejected candidates and coverage limitations

Do not keep `quality_pass_mode` as a compatibility branch. The planned-review path should always run the applicable quality passes unless prevented by explicit hard caps or missing data.

## Default Budgets

These are starting defaults, not user-visible promises. They should be constants with telemetry-friendly names.

| Scope | Default |
| --- | --- |
| Per-turn tool call cap | `8` |
| Per-session tool parallelism | `4` |
| Baseline planned unit | `10 turns / 32 tools` |
| High-risk planned unit | `12-14 turns / 64 tools` |
| Secondary lens unit | `6 turns / 20 tools` |
| High-value secondary lens | `8 turns / 32 tools` |
| Challenge batch | `4 turns / 16 tools` |
| Challenge batch size | max `3 findings` |
| Default large-PR active sessions | `8` |
| High-risk lens active cap | `4` |
| Contract-pack cap | scales up to max `32 selected packs` |

Bootstrap should consume the same unit tool budget, but it must not starve model-driven exploration. For a `32` tool baseline unit:

- bootstrap max: `8` calls
- reserved model exploration: at least `20-24` calls
- bootstrap tools per turn: subject to the same per-turn cap of `8`

Final synthesis and finding challenge should use separate run-level budgets. They should not steal the unit budget that was meant for exploration.

## Budget Source

Add an explicit budget source so the runtime can explain why a limit exists:

```rust
pub enum BudgetSource {
    CallerHardCap,
    PlannedDefault,
    AdaptiveReview,
    RunReserve,
}
```

Rules:

- Caller-provided budgets are hard caps.
- Planned defaults apply only when the caller did not provide a cap.
- Adaptive review may raise planned defaults within run-level limits.
- Runtime downgrades coverage when caps prevent required evidence.

This avoids confusing a caller cap with a product default.

## Default Lens Panel

When a planned review has no caller-provided sessions, the planner should generate a default panel:

- `correctness`: primary generalist lens
- `security`: credentials, authorization, tenant boundaries, secrets, destructive operations
- `architecture-contracts`: public APIs, return shapes, lifecycle, adapters, state boundaries
- `performance`: query shape, repeated work, fan-out, large data paths, resource use

If the caller supplies custom sessions, those replace the default panel. The runtime can still add contract packs and challenge passes around those sessions.

## Large PR Strategy

Large PRs should be reviewed with tiered coverage, not fake exhaustiveness.

### Size Tiers

| Tier | Signal |
| --- | --- |
| Small | `<= 2k` changed lines and `<= 40` files |
| Medium | `<= 10k` changed lines and `<= 150` files |
| Large | `<= 50k` changed lines or `<= 500` files |
| Huge | `> 50k` changed lines or `> 500` files |

The large and huge tiers should support multiple waves of planned units with `max_active_sessions` around `8`, subject to caller caps.

### Planned Unit Count Formula

Start with this formula for generated planned-review units:

```text
size_units = ceil(changed_files / 35) + ceil(changed_lines / 5000)
risk_units = ceil(high_risk_files / 4) + ceil(selected_contract_packs / 4)
lens_units = default_lens_count

planned_units = clamp(lens_units + size_units + risk_units, min=4, max=32)
max_active_sessions = min(caller_cap_or_default_8, 8)
```

For huge PRs, more units are queued than active at once. The scheduler runs review waves and records coverage level per file and per lens.

### File Risk Formula

Use a file score to decide target coverage and lens routing:

```text
file_score =
  contract_pack_hit
+ high_risk_path
+ public_api_or_adapter
+ caller_callee_boundary
+ persistence_or_query_boundary
+ credential_or_tenant_boundary
+ changed_test_contract
+ changed_lines_weight
+ related_changed_paths_weight
```

Suggested weights:

| Signal | Weight |
| --- | --- |
| selected contract pack touches file | `100` |
| credential, tenant, auth, destructive, or secret path | `90` |
| query/filter/persistence boundary | `80` |
| exported API, SDK, adapter, protocol, or return-shape boundary | `70` |
| caller/callee boundary with changed related path | `50` |
| tests or fixtures changed for same behavior | `35` |
| changed lines weight | `min(40, changed_lines_in_file / 25)` |
| many related changed paths | `min(30, related_changed_paths * 5)` |

Target coverage:

| Score | Target |
| --- | --- |
| `>= 120` | `full` |
| `70-119` | `standard` |
| `30-69` | `sampled` |
| `< 30` | `sampled` or omitted with diagnostics |

High-risk and contract-pack files must not default to sampled coverage. They may only end sampled or insufficient because of explicit caps, unreadable files, or exhausted budgets, and the audit must report that gap.

## Coverage Model

Add formal coverage to file reviews and the run summary:

```rust
pub enum ReviewCoverage {
    Full,
    Standard,
    Sampled,
    Insufficient,
}

pub enum ReviewVerdict {
    Clean,
    IssueFound,
    NeedsReview,
}
```

Coverage and verdict are separate.

Examples:

- `coverage=Sampled, verdict=Clean` means no issue was found in the sampled portion.
- `coverage=Insufficient, verdict=NeedsReview` means the runtime could not gather enough evidence.
- `coverage=Full, verdict=IssueFound` means the file or contract was deeply reviewed and produced a finding.

Initial coverage should be derived from evidence, not from a model declaration. The model can effectively upgrade coverage by doing more exploration. The runtime owns downgrades when evidence is missing or budgets are exhausted.

### Coverage Definitions

`Full` means:

- changed file was read at relevant ranges
- relevant callers/callees or contract peers were checked
- tests or fixtures were searched and read when applicable
- base/head or before/after behavior was compared when needed

`Standard` means:

- changed file was read at relevant ranges
- at least the primary related contract path was checked when applicable
- obvious tests or fixtures were searched

`Sampled` means:

- representative changed ranges were inspected
- no claim of exhaustive contract coverage is made

`Insufficient` means:

- required evidence could not be gathered
- tool, budget, parse, or scope limits prevented review
- high-risk coverage target was not met

## Contract Packs

Contract packs should focus planned exploration on high-value review contracts.

Candidate kinds:

- credential ownership
- query/filter/destructive scope
- return shape and caller contract
- time boundary

All generated packs should run. Pack volume is controlled before generation:

1. Build candidate packs.
2. Score candidates.
3. Select up to the adaptive cap.
4. Generate selected packs.
5. Run every generated pack.
6. Report omitted candidates in diagnostics.

### Candidate Scoring

```text
pack_score = severity + evidence_quality + breadth_risk - cost_penalty - duplicate_penalty
```

Severity:

| Kind | Weight |
| --- | --- |
| credential/security ownership | `100` |
| query/filter/destructive scope | `95` |
| return-shape/caller contract | `80` |
| time boundary | `65` |

Evidence quality:

| Signal | Weight |
| --- | --- |
| primary changed file | `10` |
| related changed path | `8` |
| changed-line seed identifiers | `6` |
| head content confirms relationship | `6` |
| tests or fixtures found | `5` |

Breadth and risk:

| Signal | Weight |
| --- | --- |
| multiple related changed paths | up to `15` |
| high review-plan score | up to `10` |
| repeated integration or adapter pattern | up to `10` |

Cost penalties:

| Signal | Penalty |
| --- | --- |
| too many related paths to inspect meaningfully | up to `15` |
| huge files or generated-looking files | up to `10` |
| duplicate or near-duplicate candidate | up to `20` |

Selection cap:

```text
selected_contract_packs = clamp(scored_candidates, min=0, max=adaptive_cap)
adaptive_cap = min(32, 4 + ceil(changed_lines / 5000) + ceil(high_risk_files / 2))
```

Omitted packs are not silent. The audit should include their kind, score, affected paths, and omission reason.

## Evidence Requirements

A publishable finding must have:

- changed path and changed range
- concrete failure predicate
- behavior before and behavior after, when applicable
- supporting artifact ids
- checked paths
- confidence rationale
- challenge status

For cross-file findings, require evidence from both sides of the contract. For example:

- changed producer plus existing consumer
- changed filter plus caller that depends on the filter
- changed return shape plus call site
- changed credential source plus authorization boundary

If cross-file evidence is missing, reject the candidate from public findings and record the rejection in diagnostics.

Tests are not executed for now. Tests and fixtures should still be searched and read when they define the behavior. Add explicit test evidence tracking to the evidence model.

## Finding Challenge

Finding challenge must be tool-enabled and claim-scoped.

The challenger gets:

- the candidate finding
- prior supporting artifacts
- diff and file read tools
- search tools
- import and related-file tools
- test and fixture read access
- full read-only repo scope

The challenger must not look for new findings. It only verifies the claims it received.

Challenge batches:

- max `3` related findings per batch
- budget `4 turns / 16 tools`
- read-only tools only
- no test execution for now

Structured result:

```json
{
  "findingId": "finding_...",
  "verdict": "confirmed | refuted | insufficient",
  "reason": "...",
  "supportingArtifactIds": ["art_..."],
  "checkedPaths": ["src/a.ts", "src/b.ts"]
}
```

Finding challenge status:

```rust
pub enum ChallengeStatus {
    Confirmed,
    Refuted,
    Insufficient,
    NotRun,
    Incomplete,
}
```

Rules:

- `confirmed`: publishable, confidence may increase.
- `refuted`: suppress from public findings.
- `insufficient`: suppress from public findings.
- `not_run`: only allowed when challenge was not applicable or disabled by explicit caller cap.
- `incomplete`: infrastructure failure; do not suppress solely because of challenge failure, but do not boost confidence.

Challenge status dominates publishability. Agreement count can tune confidence only after challenge.

## Diagnostics And Audit Artifact

Final output should keep public findings clean. Rejected and suppressed candidates belong in diagnostics.

Create a compact planned-review audit artifact with:

- coverage counts by level
- coverage counts by lens
- high-risk files below target coverage
- selected contract packs
- omitted contract pack candidates
- accepted finding candidates
- rejected finding candidates and rejection reason
- challenge batches and verdict counts
- sessions run, budgets used, caps hit
- explicit caller caps that changed behavior

Final synthesis receives a compact audit summary and must mention coverage limitations when they exist.

## Implementation Map

Likely touchpoints:

- `src/runtime/contracts.rs`
  - raise `max_tool_calls_per_turn` to `8`
  - raise `max_tool_parallelism_per_session` to `4`
  - remove `quality_pass_mode`
  - add budget source metadata where session/run budgets are represented

- `src/runtime/planned_units.rs`
  - replace `2 turns / 8 tools` fallback
  - make bootstrap, contract packs, synthesis, and challenge always applicable in planned review
  - reserve exploration budget after bootstrap
  - add coverage derivation and coverage downgrades
  - add challenge orchestration
  - add audit artifact emission

- `src/runner/planning.rs`
  - generate default lens panel when no sessions are provided
  - raise large-review active session default to `8`
  - add adaptive unit generation for large PRs
  - distinguish caller hard caps from planned defaults

- `src/reviewer/spec.rs`
  - represent optional caller budgets without forcing early concrete defaults
  - carry budget source or hard-cap metadata into runtime planning

- `src/runtime/contract_packs.rs`
  - add candidate scoring
  - cap selected generated packs adaptively up to `32`
  - report omitted candidates

- evidence tracking
  - add test/fixture evidence
  - enforce multi-path evidence for cross-file findings
  - preserve rejected candidates for diagnostics

## Rollout Slices

### Slice 1: Strong Planned-Review Path

- remove `quality_pass_mode`
- make bootstrap, contract packs, final synthesis, and challenge always run when applicable
- raise per-turn tool cap to `8`
- raise per-session tool parallelism to `4`
- replace planned-unit fallback with `10 turns / 32 tools`
- keep unrelated direct-session behavior unchanged

### Slice 2: Adaptive Budgets And Lens Panel

- add `BudgetSource`
- keep caller budgets as hard caps
- generate default lens panel for planned reviews with no sessions
- add baseline, high-risk, and secondary-lens budget classes
- separate unit exploration budget from synthesis and challenge reserves

### Slice 3: Large PR Scaling

- classify PR size tiers
- generate multiple planned units for large and huge PRs
- set default large-review active sessions to `8`
- cap high-risk lens active sessions at `4`
- add contract-pack scoring and adaptive cap up to `32`
- add formal coverage level on file reviews
- aggregate coverage counts in the run summary

### Slice 4: Tool-Enabled Challenge

- batch related findings, max `3` per challenge batch
- give challenger read-only repo tools and prior artifacts
- forbid new-finding exploration in the challenge prompt/schema
- suppress `refuted` and `insufficient`
- mark infra failures as `incomplete` without automatic suppression
- write challenge status onto findings

### Slice 5: Evidence Quality Polish

- add test and fixture evidence tracking
- require multi-path evidence for cross-file findings
- reject candidates missing required evidence into diagnostics
- improve final synthesis to report coverage limitations clearly
- tune budget and scoring constants from observed review traces

## Acceptance Criteria

The new planned-review behavior is acceptable when:

- a planned review with no sessions creates a real lens panel, not one tiny generalist session
- ordinary cross-file findings have enough budget to inspect both sides of the contract
- high-risk and contract-pack files are not silently downgraded to sampled coverage
- large PRs produce coverage-aware output instead of pretending to be exhaustive
- public findings are confirmed or explicitly explain incomplete challenge state
- refuted and insufficient candidates are absent from public findings but present in diagnostics
- explicit caller caps are honored and reported when they constrain review quality
- direct sessions remain available as the raw lower-level runtime

## Non-Goals For Now

- executing tests
- preserving old planned-review behavior
- adding callback-based escape hatches
- introducing a second compatibility mode for weak review
- claiming full coverage for huge PRs when only sampled review was possible
