# Muzen x Argus Provider-Neutral Review Swarm Plan

Generated: 2026-06-05

## Decision

Use Argus as Muzen's first production proving adapter, not as the shape of
Muzen core.

Muzen should become a provider-neutral review swarm engine that can review:

- GitLab merge requests
- GitHub pull requests
- Perforce changelists and shelves
- local Git changes
- host-provided snapshots
- raw diffs or source bundles
- future source systems

Argus should integrate Muzen at its review-worker engine boundary. Argus keeps
its product, queue, persistence, SSE, feedback, and publishing behavior while
Muzen owns the review execution kernel: source snapshots, concurrent sessions,
tools, model calls, evidence, findings, aggregation, and execution metrics.

The core principle:

```text
Argus-specific concepts belong in an Argus adapter.
Provider-specific concepts belong in source providers.
Muzen core should only know review execution concepts.
```

## Why This Is The Right Seam

Argus already has a useful execution seam:

```text
web/API request
  -> enqueue review flow run
  -> review worker claims leased run
  -> resolve provider/model/user settings
  -> call review engine
  -> persist review run
  -> stream progress through existing SSE/read models
```

Muzen should initially replace only the review engine:

```text
runReviewEngine(...)
  -> runMuzenReview(...)
```

That gives Argus the Muzen advantages without forcing an immediate rewrite of
Argus infrastructure:

- one review run can fan out into many sessions
- sessions share one source snapshot
- sessions share tool/search caches
- sessions can use bounded custom tools
- model and tool metrics are unified
- findings can be deduped and validated before Argus persists them
- Argus UI and SSE can keep working through event mapping

Do not start by making Muzen own Argus queues, leases, or read models. That is
a later durable-boundary decision after engine parity is proven.

## What Muzen Must Not Become

Muzen core must not contain first-class Argus fields:

- `flowRunId`
- `traceId` as a required core id
- `gitlabActorUserId`
- `mergeRequestIid`
- `sourceBaseSha`
- `sourceStartSha`
- `sourceHeadSha`
- `ArgusProgressStage`
- `ArgusFinding`
- `issueContext`
- `personaTemplates` as an Argus-specific concept

Those can appear in:

- Argus adapter input
- provider metadata
- host metadata
- custom tool payloads
- result projection code
- event mapping code

They should not define Muzen's public review request, runner protocol, event
taxonomy, or finding model.

## Target Architecture

```text
Argus web/API
  - authentication
  - quota and reservation
  - enqueue and dedupe
  - SSE endpoints
  - read models
  - comment publishing

Argus review-worker
  - claims flow runs
  - resolves user/model/provider settings
  - calls selected review backend
  - maps backend progress to Argus progress
  - persists Argus review rows

Argus Muzen adapter
  - maps claimed Argus run -> Muzen ReviewRequest
  - registers Argus host tools
  - maps Muzen events -> Argus progress/events
  - maps Muzen findings -> Argus findings
  - maps Muzen metrics -> Argus metrics

Muzen SDK / runner boundary
  - starts review runs
  - handles model callbacks
  - handles tool callbacks
  - streams ordered events
  - exposes artifacts and metrics

Muzen core
  - resolves source providers
  - materializes immutable snapshots
  - schedules swarm sessions
  - enforces tool capabilities
  - calls model providers
  - records evidence and artifacts
  - aggregates, validates, and dedupes findings

Source providers
  - local
  - git
  - GitLab MR
  - GitHub PR
  - Perforce changelist
  - raw snapshot
  - custom
```

## Current Codebase Touchpoints

### Muzen

Important Muzen context:

- `docs/sdk-runner-implementation-plan.md` already points toward an SDK to
  runner protocol with model callbacks, tool callbacks, events, and artifacts.
- `sdk/typescript/packages/muzen-sdk/src/types.ts` exposes provider-neutral
  sources, change specs, layered instructions, callback models, callback
  source providers, callback tools, per-session grants, review hooks, limits,
  and metadata.
- `src/runner/types.rs` and `src/runner/schema.rs` include source-provider,
  change, instruction, tool, session-grant, model-callback, and finding-location
  wire contracts.
- `src/runner/execution.rs` registers callback tools from run params, validates
  grants, carries layered instructions into sessions, and shares one materialized
  snapshot across the run.
- `src/review_session/session.rs` passes source, change, instructions, and tools
  into runner-backed durable review creation.
- `src/reviewer.rs` already exposes a Rust-side `ReviewToolRegistry` and
  scoped custom tool grants with explicit provider-neutral effects.
- `src/runtime/policy.rs` currently keeps the system review policy
  kernel-owned and primarily allows objective/session influence, so prompt
  customization should be layered instructions before raw system prompt
  replacement.

### Argus

Important Argus context:

- `apps/review-worker/src/worker-runtime.ts` builds the review worker runtime
  and injects dependencies such as provider settings, review execution,
  persistence, quota release, cleanup, and queue snapshots.
- `apps/review-worker/src/review-argus-agent.ts` wraps the current Argus agent
  engine and provides GitLab diff/file access, user token resolution, issue
  context, workspace checkout, artifacts, logging, and environment.
- `packages/review/server/src/argus-agent/review-runner.ts` defines the
  current Argus agent review input shape. This is the shape the Argus adapter
  can accept, not the shape Muzen core should expose.
- `packages/review/server/src/worker-run-processor.ts` owns claim, fencing,
  leases, provider settings, progress updates, backend execution, and final
  persistence.
- `packages/review/server/src/flow-run-enqueue.ts` owns enqueue, dedupe,
  flow-run insertion, config hashing, and notification.
- `packages/review/persistence/src/schema/review.ts` owns Argus flow-run and
  review-run persistence, progress stages, execution backend enum, findings,
  metrics, issue context, refs, visibility, and feedback.
- `packages/argus-agent/src/review/workflow.ts` is the current agent workflow:
  diff parsing, context building, issue/repository instruction handling,
  exploration, model review, and events.
- `packages/argus-agent/src/tool/registry.ts` lists host tools such as code
  search, file reads, ranges, diff access, tracing, grep, and delegated
  exploration.
- `apps/web/src/app/api/review/diff/route.ts` and
  `apps/web/src/app/api/review/diff/stream/route.ts` should not need
  structural changes in the first Muzen adapter phase.

## Core Public Shape

The target SDK and runner shape should look like this conceptually:

```ts
type ReviewRequest = {
  source: ReviewSource
  change?: ChangeSpec
  sessions?: ReviewSessionSpec[]
  model?: ModelSpec
  tools?: ToolRegistration[]
  instructions?: InstructionSpec[]
  limits?: ReviewLimits
  metadata?: Record<string, unknown>
  eventSink?: ReviewEventSink
  cancellation?: CancellationSignal
}
```

An Argus adapter can build this from a claimed Argus flow run:

```ts
const result = await runMuzenReview({
  source: gitlabMergeRequest({
    instanceUrl,
    projectId,
    mergeRequestIid,
    credential: gitlabTokenRef,
  }),
  change: {
    kind: "revision_range",
    baseRevision: sourceBaseSha,
    startRevision: sourceStartSha,
    headRevision: sourceHeadSha,
  },
  instructions: [
    hostInstructions(argusReviewPolicy),
    repositoryInstructionsProvider(),
    personaModeInstructions(personaMode, personaTemplates),
  ],
  model: openAiCompatibleModel({ apiUrl, model, token: tokenRef }),
  metadata: {
    host: "argus",
    hostRunId: flowRunId,
    traceId,
    sourceProvider: "gitlab",
  },
  eventSink: mapMuzenEventToArgusProgress,
  cancellation,
});
```

The key is that the adapter knows Argus. Muzen core knows only the generic
request.

## Core Contracts

### `ReviewSource`

`ReviewSource` describes where code comes from.

```ts
type ReviewSource =
  | LocalSource
  | GitSource
  | GitHubPullRequestSource
  | GitLabMergeRequestSource
  | PerforceChangelistSource
  | RawSnapshotSource
  | CustomSource
```

Examples:

```ts
type GitLabMergeRequestSource = {
  kind: "gitlab_merge_request"
  instanceUrl: string
  projectId: string | number
  mergeRequestIid: string | number
  credential?: SecretRef
  metadata?: Record<string, unknown>
}
```

```ts
type PerforceChangelistSource = {
  kind: "perforce_changelist"
  server: string
  changelist: string | number
  client?: string
  depotPaths?: string[]
  credential?: SecretRef
  metadata?: Record<string, unknown>
}
```

Core can route by `kind`, but provider-specific fields should only be consumed
by the provider implementation.

### `ChangeSpec`

`ChangeSpec` describes what should be reviewed. It must not assume Git commits.

```ts
type ChangeSpec = {
  kind:
    | "revision_range"
    | "snapshot_pair"
    | "diff"
    | "provider_review"
  baseRevision?: string | null
  startRevision?: string | null
  headRevision?: string | null
  changedFiles?: ChangedFileSpec[]
  diff?: string | null
  reviewTarget?: string | null
  metadata?: Record<string, unknown>
}
```

Provider mappings:

- GitLab MR: base/start/head SHAs, MR IID in source/provider metadata.
- GitHub PR: base/head SHAs, PR number in source/provider metadata.
- Perforce: changelist number, shelf id, depot file revisions, or provider
  review handle.
- Local Git: merge base/head, staged changes, working tree, or synthetic
  revisions.
- Raw diff: `kind = "diff"` with inline diff and optional changed file list.
- Raw snapshot: `kind = "snapshot_pair"` with content-addressed manifests.

### `ReviewSessionSpec`

A Muzen run should contain one or many sessions:

```ts
type ReviewSessionSpec = {
  id?: string
  role?: ReviewRole
  objective: string
  model?: ModelSpec
  instructions?: InstructionSpec[]
  toolGrants?: ToolGrant[]
  cwd?: string
  limits?: SessionLimits
  output?: SessionOutputPolicy
}
```

Useful roles:

- generalist
- correctness
- security
- performance
- maintainability
- architecture
- tests
- documentation
- accessibility
- validator
- custom

Argus persona mode maps into this. Muzen should not hardcode Argus personas.

### `ToolRegistration`

Tools are the main way hosts add capabilities without changing Muzen core.

```ts
type ToolRegistration = {
  id: string
  description: string
  parameters: JsonSchema
  effects: ToolEffect[]
  resources?: ProviderResourceRef[]
  cacheable?: boolean
  maxOutputBytes?: number
  handler?: ToolHandler
}
```

Tool effects:

- `read_repo`
- `read_diff`
- `read_artifact`
- `read_host`
- `read_network`
- `read_scratch`
- `write_artifact`
- `write_scratch`

Write effects that mutate host state should not be part of V1. Posting
comments, updating tickets, or changing review state should stay in host
projectors/workflows until Muzen has a deliberate write-capability model.

### `InstructionSpec`

Instructions should be layered, not one raw prompt blob:

```ts
type InstructionSpec = {
  kind:
    | "kernel_policy"
    | "host_policy"
    | "organization_policy"
    | "repository_policy"
    | "session_objective"
    | "provider_context"
  text?: string
  provider?: InstructionProviderRef
  trusted?: boolean
  metadata?: Record<string, unknown>
}
```

Rules:

- kernel policy is owned by Muzen
- host policy is owned by the host adapter
- repository and provider content are untrusted by default
- repository instructions cannot override kernel safety policy
- raw system prompt replacement is an advanced API, not the default

### `ModelSpec`

Muzen should support host-provided model routing:

```ts
type ModelSpec = {
  kind: "openai_compatible" | "callback" | "builtin" | "custom"
  model?: string
  baseUrl?: string
  credential?: SecretRef
  options?: Record<string, unknown>
}
```

Argus can pass its existing provider settings through this model contract.
Other hosts can use a different model provider without changing review
execution.

### `ReviewEvent`

Muzen events should be provider-neutral and replayable:

```ts
type ReviewEvent = {
  sequence: number
  timestamp: string
  runId: string
  sessionId?: string
  kind: ReviewEventKind
  payload: Record<string, unknown>
  metadata?: Record<string, unknown>
}
```

Core event kinds:

- `run.created`
- `run.started`
- `source.resolved`
- `source.materialized`
- `snapshot.created`
- `plan.created`
- `session.started`
- `model.started`
- `model.completed`
- `tool.started`
- `tool.completed`
- `finding.created`
- `finding.updated`
- `artifact.created`
- `session.completed`
- `run.completed`
- `run.failed`
- `run.cancelled`

Argus maps these into existing progress stages:

- source/snapshot/plan -> `chunking`
- session/model/tool activity -> `analyzing`
- finding anchoring/projection -> `mapping`
- final host persistence -> `saving`
- terminal success -> `done`
- terminal failure -> `failed`

Muzen should not use Argus stage names as core event kinds.

### `ReviewFinding`

Core findings should be host-neutral:

```ts
type ReviewFinding = {
  id: string
  title: string
  message: string
  severity: "info" | "warning" | "error"
  category?: string
  confidence?: number
  evidence: EvidenceRef[]
  locations: FindingLocation[]
  suggestedFix?: SuggestedFix
  discoveredBy?: string[]
  validatedBy?: string[]
  metadata?: Record<string, unknown>
}
```

Locations should not assume GitHub/GitLab diff comments:

```ts
type FindingLocation = {
  path: string
  revision?: "base" | "head" | string
  startLine?: number
  endLine?: number
  side?: "base" | "head" | "additions" | "deletions"
  providerAnchor?: Record<string, unknown>
}
```

Projection examples:

- Argus review rows
- GitHub review comments
- GitLab discussion notes
- Perforce changelist annotations
- SARIF
- CLI markdown
- JSON artifacts

Projection code should live outside core or in optional adapter packages.

## The Seven Argus Requirements, Generalized

### 1. Custom Prompts, Tools, Models, And Events

Argus needs repository instructions, persona settings, model provider settings,
issue context, and progress callbacks.

Generic Muzen requirement:

- public SDK support for custom tools
- public SDK support for model specs/callbacks
- public SDK support for layered instructions
- per-session tool grants
- ordered event streaming
- host metadata echo

This should not be an Argus-only request shape.

### 2. GitLab Merge Request Source

Argus needs GitLab MR review with base/start/head refs.

Generic Muzen requirement:

- a GitLab source provider
- a provider-neutral `ChangeSpec`
- opaque provider metadata
- snapshot materialization that runtime sessions consume generically

This should not make GitLab the default source abstraction.

### 3. One Run, Many Sessions

Argus needs multi-persona review to become cheaper and more coherent.

Generic Muzen requirement:

- one `ReviewRun`
- many `ReviewSessionSpec`s
- shared snapshot
- shared tool/search caches
- unified metrics
- aggregation, dedupe, and validator pass

This is the main performance and quality reason to use Muzen.

### 4. Progress Events

Argus needs SSE-compatible replay.

Generic Muzen requirement:

- durable ordered events
- stable event taxonomy
- host event mappers
- heartbeats for long phases
- terminal failure/cancellation events

### 5. Finding Anchoring

Argus needs publishable GitLab comment locations.

Generic Muzen requirement:

- provider-neutral location model
- provider anchors as metadata
- projector per host/provider

GitHub, GitLab, and Perforce should all project from the same finding model.

### 6. Heartbeat And Cancellation

Argus needs lease safety.

Generic Muzen requirement:

- cancellation token propagation
- heartbeat callbacks or runtime events
- cleanup hooks
- partial artifact preservation
- structured terminal failure reasons

### 7. Host Context Tools

Argus needs issue context, code search, symbol tracing, repository
instructions, and diff/file access.

Generic Muzen requirement:

- host tools with namespaced ids
- explicit effects
- output limits
- session grants
- metrics and denial events

Context that can be fetched on demand should usually be a tool, not a giant
prompt prefix.

## Source Provider Model

Muzen needs a source provider boundary in Rust and SDK protocol form.

Conceptual Rust shape:

```rust
trait SourceProvider {
    fn provider_id(&self) -> SourceProviderId;

    async fn resolve(
        &self,
        request: SourceResolveRequest,
        cancel: CancellationToken,
    ) -> RuntimeResult<ResolvedSource>;

    async fn materialize(
        &self,
        source: ResolvedSource,
        policy: MaterializationPolicy,
        cancel: CancellationToken,
    ) -> RuntimeResult<MaterializedSource>;
}
```

SDK/runner V1 callback boundary:

- `source.materialize`

V1 should not add `source.resolve`, `source.readDiff`, or `source.readFile` as
runner callbacks. The host or provider callback materializes an immutable
snapshot plus changed-file manifest; after that, Muzen's built-in snapshot tools
own diff and file reads. That keeps source providers provider-neutral while
avoiding a lazy remote file API in the first release. Revisit `source.resolve`
or lazy read callbacks only if a real production host cannot afford snapshot
materialization.

Initial providers:

- `local`
- `git`
- `gitlab_merge_request`
- `github_pull_request`
- `raw_snapshot`

Future providers:

- `perforce_changelist`
- `custom`
- provider callbacks over JSON-RPC
- MCP-backed source providers later

### Perforce Requirements

Perforce is the forcing case that prevents Git-only overfitting.

The source/change model must support:

- depot paths
- client workspaces
- changelist numbers
- shelved changelists
- file revisions
- source materialization without Git commits
- changed-file manifests without Git diff assumptions
- provider anchors that can map to depot file/revision locations

If Perforce cannot fit without changing core request fields, the source model
is too Git-centric.

## Argus Adapter Plan

### Adapter Input

The Argus adapter can accept the current Argus-shaped engine input:

```ts
type ArgusMuzenAdapterInput = {
  flowRunId: string
  traceId: string
  userId: string
  gitlabActorUserId: string | null
  integrationId: string
  instanceUrl: string
  projectId: string
  mergeRequestIid: string
  sourceBranch: string | null
  sourceBaseSha: string
  sourceStartSha: string
  sourceHeadSha: string
  apiUrl: string
  model: string
  organization?: string
  token: string
  personaMode: "disabled" | "single" | "multi"
  persona: string
  personaTemplates: Record<string, string>
  ignorePaths: string[]
  repositoryInstructionsEnabled: boolean
  onProgress(update: ArgusProgressUpdate): Promise<void> | void
  onHeartbeat(): Promise<void> | void
}
```

This type belongs in Argus or an Argus adapter package.

### Adapter Mapping

Map input to Muzen:

- `instanceUrl`, `projectId`, `mergeRequestIid` -> GitLab MR source
- `sourceBaseSha`, `sourceStartSha`, `sourceHeadSha` -> `ChangeSpec`
- `personaMode`, `persona`, `personaTemplates` -> sessions
- `apiUrl`, `model`, `token` -> `ModelSpec`
- `ignorePaths` -> review scope/path policy
- `repositoryInstructionsEnabled` -> instruction provider toggle
- `flowRunId`, `traceId` -> metadata
- `onProgress` -> event mapper
- `onHeartbeat` -> heartbeat callback

Register Argus tools:

- `argus.issue_context`
- `argus.repository_instructions`
- `argus.search_code`
- `argus.trace_symbol`
- `argus.read_file_range`
- `argus.get_diff`

Use Muzen built-ins for generic source operations:

- `list_changed_files`
- `read_diff`
- `read_file`
- `read_base_file`
- `read_head_file`
- `search_text`
- `record_finding`
- `challenge_finding`
- `finish`

### Adapter Output

The adapter returns the shape Argus already expects:

```ts
type ArgusReviewEngineResult = {
  findings: ArgusFinding[]
  summary: string
  totalBatches: number
  reviewedFiles: string[]
  patchLength: number
  verificationPatch: string
  issueContext: ReviewIssueContext | null
  metrics: {
    llmRequests: number
    promptTokens: number
    completionTokens: number
    totalTokens: number
    toolCallCount: number
    latestProvider: string | null
    latestModel: string | null
  }
}
```

The conversion from Muzen result to this shape should not live in Muzen core.

## Security And Credential Rules

Provider-neutral does not mean credential-neutral.

Use secret references where possible:

```ts
type SecretRef = {
  kind: "env" | "host" | "inline_for_dev_only"
  name?: string
  value?: string
}
```

Rules:

- production hosts should pass secret refs, not raw secrets
- inline secrets are for local/dev examples only
- events and artifacts should redact credentials before persistence/export
- repository content is untrusted
- issue text, MR descriptions, PR comments, and repo instructions are untrusted
- repository instructions cannot override Muzen kernel policy
- tool outputs marked sensitive should be redacted from public artifacts

Capability rules:

- read-only tools first
- explicit tool effects
- per-session grants
- provider resource allowlists
- bounded tool output
- denial events for unauthorized calls
- host-visible write actions stay outside V1

## Observability

Muzen should emit structured metrics so Argus and other hosts do not need to
parse transcripts.

Per run:

- wall time
- source resolution time
- materialization time
- snapshot size
- changed-file count
- session count
- model request count
- token usage
- tool call count
- artifact count
- finding count before aggregation
- finding count after aggregation
- failure or cancellation reason

Per session:

- role
- model
- wall time
- token usage
- tool usage
- finding count
- validator/challenge result

Per tool:

- tool id
- call count
- latency
- input byte totals
- output byte totals
- cache hits
- denial count

Host correlation metadata:

- `host`
- `hostRunId`
- `traceId`
- `sourceProvider`
- `sourceReviewId`

Core echoes this metadata. Adapters interpret it.

## Failure And Recovery

Muzen terminal events should distinguish:

- `source_unavailable`
- `auth_failed`
- `tool_failed`
- `model_failed`
- `budget_exhausted`
- `cancelled`
- `policy_denied`
- `internal_error`

Each terminal failure should include a retry hint:

- `retryable`
- `not_retryable`
- `retry_after`
- `requires_user_action`

Partial results should be preserved when safe:

- emitted events
- source manifest
- completed session summaries
- findings created before cancellation
- tool and model metrics
- artifacts

Adapters decide whether partial findings are user-visible.

## Implementation Roadmap

### Phase 0: Contract Lock

Goal: prevent accidental Argus leakage before implementation accelerates.

Tasks:

- define `ReviewRequest`
- define `ReviewSource`
- define `ChangeSpec`
- define `ReviewSessionSpec`
- define `InstructionSpec`
- define `ToolRegistration`
- define `ReviewEvent`
- define `ReviewFinding`
- add protocol fixtures
- add provider-neutrality tests that fail on Argus-only core fields

Acceptance:

- no Muzen core request/event/finding type requires Argus terms
- GitLab, GitHub, Perforce, local, and raw snapshot examples can be expressed
- existing preview APIs can map into the new request shape

### Phase 1: SDK Tools, Instructions, And Models

Goal: expose the primitives Argus needs without coupling to Argus.

Tasks:

- add `tools` to the public TypeScript SDK request/options
- expose async tool handlers
- support JSON Schema validation for tool inputs
- add tool output limits
- expose per-session tool grants
- expose instruction providers/layers
- expose model specs or model callbacks
- wire durable review creation to pass tools instead of `Vec::new()`
- add cancellation propagation through callbacks

Acceptance:

- a TS SDK caller can register a custom read-only tool
- a session can be granted or denied that tool
- tool metrics are visible
- model config/callbacks work through the runner
- repository instructions can be supplied without replacing kernel policy

### Phase 2: Argus Adapter Spike

Goal: prove one Argus claimed flow run can complete through Muzen.

Tasks:

- add `muzen` to Argus execution backend enum
- add `runMuzenReview` beside `runArgusAgentReview`
- map Argus input into Muzen `ReviewRequest`
- use GitLab MR source with base/start/head refs
- register minimal Argus host tools
- map Muzen events to Argus progress
- map Muzen findings to Argus findings
- map Muzen metrics to Argus metrics
- keep Argus persistence unchanged
- gate with an env flag or feature flag

Acceptance:

- manual Argus review can queue and complete through Muzen
- existing Argus review page receives progress
- existing SSE replay works
- review row persists with findings and summary
- comment publish workflow can publish Muzen-produced findings
- no structural Argus API route changes

### Phase 3: Whole-MR Swarm Efficiency

Goal: make Argus benefit from Muzen's efficient session spawning.

Tasks:

- map Argus multi-persona mode to multiple Muzen sessions
- add shared snapshot across sessions
- add shared search/tool cache across sessions
- add session concurrency controls
- add aggregation policy
- add duplicate finding merge
- add validator/challenge pass

Acceptance:

- one Argus MR review maps to one Muzen run
- multi-persona review uses multiple Muzen sessions inside that run
- Muzen reports per-session and aggregate metrics
- duplicate findings are merged before Argus persistence
- wall-clock time and/or cost improves against current Argus-agent baseline

### Phase 4: Event And Result Parity

Goal: make Argus UI behavior unchanged while Muzen owns execution.

Tasks:

- finalize Muzen event taxonomy
- add Muzen-to-Argus event mapper tests
- add finding anchor mapper for GitLab changed lines
- map severity/confidence/persona/evidence
- map reviewed files and coverage
- map suppressed/rejected validator output if present
- add replay fixtures

Acceptance:

- Argus active run polling works
- Argus SSE replay works
- Argus observability shows model/token/tool metrics
- GitLab comments anchor correctly for changed lines
- unmappable locations degrade gracefully instead of failing the run

### Phase 5: Provider Expansion

Goal: prove Muzen did not become a GitLab/Argus backend.

Tasks:

- add GitHub PR provider example
- add local Git provider example
- add raw snapshot provider example
- write Perforce provider design
- add provider conformance suite
- add source materialization event fixtures

Acceptance:

- same session/tool/policy code runs against GitLab, GitHub, local, and raw
  snapshot sources
- Perforce design fits without changing `ReviewRequest`
- provider-specific fields remain in provider metadata
- no Argus-specific fields appear in core request/event/result types

### Phase 6: Durable Boundary Decision

Goal: decide whether Muzen should eventually own more than execution.

Only do this after adapter parity.

Questions:

- should Muzen own review run persistence?
- should Muzen own retries and leases?
- should Argus subscribe to a Muzen event stream instead of mapping live events?
- should Argus store only product projections?
- can existing Argus review rows migrate safely?

Acceptance:

- written comparison of Argus flow-run semantics and Muzen durable semantics
- cancellation and retry behavior are equivalent or better
- SSE replay can be bridged without UI regression
- a no-migration option remains valid if the adapter seam is sufficient

## Test Plan

### Contract Tests

- provider-neutral request examples compile
- GitLab/GitHub/Perforce/local/raw examples share the same top-level shape
- Argus-specific fields are rejected from core types
- event fixtures stay stable
- result fixtures stay stable

### SDK Tests

- custom tool registration
- tool input validation
- tool output limit enforcement
- per-session grants
- denied tool call events
- model callback execution
- cancellation during model callback
- cancellation during tool callback

### Source Provider Tests

- local source materialization
- Git source materialization
- GitLab MR source materialization
- GitHub PR source materialization
- raw snapshot materialization
- Perforce contract fixture
- large diff behavior
- rename handling where provider supports it

### Argus Adapter Tests

- claimed flow run maps to Muzen request
- Muzen event maps to Argus progress
- Muzen failure maps to Argus failed/cancelled state
- Muzen finding maps to Argus finding
- Muzen metrics map to Argus metrics
- heartbeat is called during long phases
- cancellation propagates from Argus to Muzen
- persistence shape remains unchanged

### Performance Tests

- one Muzen run with N sessions versus N separate runs
- shared snapshot reuse
- shared cache reuse
- tool batch concurrency
- memory under many sessions
- Argus-agent baseline comparison

### Security Tests

- repository instruction cannot override kernel policy
- unauthorized tool call is denied
- secret refs are not emitted in events
- sensitive tool output is redacted
- provider-resource allowlist is enforced

## Acceptance Matrix

Muzen remains broad if:

- GitLab MR support does not require Argus fields in core
- GitHub PR support uses the same request shape
- local review uses the same request shape
- raw snapshot review uses the same request shape
- Perforce fits without Git-only commits or SHAs as required fields
- hosts can register tools without changing core
- hosts can supply instructions without replacing kernel policy
- hosts can project results into their own schemas

Argus benefits if:

- Argus can select `muzen` as a review backend
- Argus queue, lease, SSE, and persistence remain intact initially
- one Argus MR review becomes one Muzen run
- multi-persona review becomes many Muzen sessions
- sessions share snapshots and caches
- findings persist and publish through existing Argus workflows
- progress and metrics are visible in existing Argus surfaces

## Risks

### Argus Leaks Into Core

Mitigation:

- adapter package owns Argus mappings
- provider metadata is opaque
- provider-neutrality tests reject Argus field names in core contracts
- core event names stay Muzen-native

### Git Assumptions Leak Into Source Model

Mitigation:

- keep revisions as generic strings
- make changed-file manifests first-class
- prototype Perforce early
- support raw snapshots early
- avoid requiring commit SHA fields in core

### Tool API Becomes Unsafe

Mitigation:

- no host-visible write tools in V1
- explicit effects
- session grants
- output limits
- redaction
- denial events

### Prompt Customization Becomes Too Powerful

Mitigation:

- layered instructions first
- kernel policy remains fixed
- raw system prompt replacement is advanced/explicit
- repository content is untrusted

### Too Much Stays In Argus

If Argus keeps doing all batching, context construction, and exploration before
Muzen, Muzen will not prove the swarm engine value.

Mitigation:

- first adapter can be conservative
- second adapter must pass the whole MR into one Muzen run
- measure snapshot/cache/session efficiency

## Near-Term Work Breakdown

Recommended first slices:

1. Write the provider-neutral request/event/finding contracts and fixtures.
2. Expose custom tools, instructions, and model callbacks in the TS SDK.
3. Wire durable session creation to pass tools through the runner.
4. Build a minimal Argus adapter package or module.
5. Add Muzen backend flag in Argus.
6. Run one manual Argus MR review through Muzen.
7. Add multi-session persona mapping.
8. Add event/result parity tests.
9. Add GitHub/local/raw provider examples.
10. Write the Perforce provider design before freezing the source API.

## Implementation Status

Completed in Muzen:

- provider-neutral source and change contracts for GitLab, GitHub, local, raw
  snapshots, Perforce changelists, and custom sources
- runner execution now maps provider-neutral `ChangeSpec` revisions, review
  targets, changed-file statuses, and source kind into core `ChangeSpec`
  instead of collapsing SDK runs to a synthetic local change
- inline `ChangeSpec.diff` is preserved through the runtime snapshot contract
  and exposed through Muzen's built-in `read_diff` tool
- SDK and runner callback seams for source materialization, model completion,
  custom tool execution, and host review event hooks
- V1 source-provider callback scope is deliberately `source.materialize` only;
  diff and file access stay as Muzen snapshot tools after materialization
- native GitHub and GitLab git checkout materialization with host callback
  materialization for custom, Perforce, and host-owned source systems
- layered run/session instructions with Muzen-owned kernel policy preserved
- per-session callback tool grants with provider-resource allowlists
- explicit provider-neutral tool effects on SDK and runner tool registrations,
  mapped into Rust runtime capability grants; runner V1 rejects unknown effects
  and host-visible external side effects
- runtime validation for custom tool JSON arguments against the registered
  schema subset
- provider-neutral finding locations with optional provider anchors
- provider-neutral finding provenance and evidence are exposed through Rust,
  runner schema, TypeScript SDK, and Python SDK result contracts
- progress and result projection helpers for host adapters
- heartbeat/lease callbacks through `run.heartbeat`, allowing hosts to renew
  long-running reviews or cancel if a lease cannot be extended
- protocol fixtures and tests for the expanded runner contract
- provider-neutrality contract tests that reject Argus-owned field names from
  production Muzen contracts
- SDK review cancellation signals that flow into source/model/tool callbacks and
  remote HTTP requests without serializing local callback state
- local SDK aborts now send `run.cancel`, and the interactive Rust runner can
  preempt an in-flight `run.start` with an active cancellation token while
  preserving partial stored results
- structured `run.failed` notifications with provider-neutral failure kinds and
  retry hints
- Python SDK parity for provider-neutral sources, changes, layered
  instructions, tool registrations, session grants, and finding provenance

Still outside Muzen core:

- Argus execution-backend flag and worker adapter
- Argus-specific progress, persistence, metric, and finding projection code
- provider-native Perforce materialization beyond host callback materialization
- deeper recovery policies for partial result preservation after terminal errors

## Open Questions

- Should source providers live primarily in Rust, SDK callbacks, or both?
- Should Muzen materialize source itself, or accept host-provided snapshots as
  the default for production adapters?
- What is the minimal useful core finding schema for V1?
- How much prompt policy customization should be public in V1?
- Should validator sessions be built into core or modeled as normal sessions?
- Should result projectors be packaged with Muzen or live entirely in hosts?
- What durable semantics would justify moving queue/persistence from Argus to
  Muzen later?
- How much of Argus indexed search should become a host tool versus a generic
  Muzen search provider?

## Recommended V1

V1 should be deliberately narrow in implementation but broad in shape.

Include:

- provider-neutral `ReviewRequest`
- source/change contracts
- sessions
- custom scoped tools with explicit effect grants, excluding host-visible writes
- layered instructions
- model specs/callbacks
- ordered events
- cancellation/heartbeat
- generic findings
- Argus adapter spike

Defer:

- host-visible write tools
- raw system prompt replacement as default API
- Muzen-owned durable queue
- native comment publishing
- full Perforce implementation
- MCP provider support unless needed by a real host

This keeps the first implementation small enough to ship while preserving the
general engine shape.

## GPT Pro Review Status

Chrome/GPT Pro review was requested, but the Chrome extension connection did
not respond even though Chrome, the extension, and the native host appeared to
be installed. Per the Chrome troubleshooting flow, the next step is opening a
fresh Chrome window for the selected profile and retrying the extension
connection, which requires explicit approval.

Questions for GPT Pro once Chrome is available:

1. Does this plan keep Muzen provider-neutral enough for GitLab, GitHub,
   Perforce, local, and raw snapshot sources?
2. Is the Argus review-worker backend seam the right first integration point?
3. Are any Argus-specific concepts leaking into core contracts?
4. Is the source/change model too Git-centric?
5. What should be removed from V1 to keep the SDK simple?
6. What must be in V1 for Argus to get real value from Muzen?
7. What acceptance tests would prove Muzen did not become Argus-specific?
