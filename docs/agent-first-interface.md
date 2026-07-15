# Muzen Agent-First Interface

Status: approved by Claude Fable/high on 2026-07-15

## Decision

Muzen is an agent runtime and central swarm host. The review product and every
review-named public type are removed from the target Interface.

The public domain has three nouns:

- **Muzen**: a connected local or remote runtime.
- **Agent Session**: one durable agent identity, transcript, configuration,
  and copy-on-write workspace overlay.
- **Run**: one bounded execution tree containing one or more root Agent
  Sessions and every child Agent Session spawned during that execution.

There is no separate Swarm type. A swarm is a Run with multiple roots or
children. `session.run(...)` is the one-root convenience form of
`muzen.startRun(...)`.

This gives the Agent Runtime Module a deep Interface: callers learn three
nouns while the Implementation hides scheduling, provider protocols, retries,
leases, fencing, transcript compaction, workspace sharing, artifacts, and
durability.

## Non-negotiable invariants

1. Local runner and remote HTTP Adapters have identical lifecycle, event,
   result, cancellation, error, workspace, model, tool, and durability
   semantics.
2. Session and Run records never contain raw credentials. They contain only
   opaque Secret References.
3. A workspace base is immutable and content-addressed. Every Agent Session
   writes only to its private copy-on-write overlay.
4. A child receives a point-in-time fork of its parent's overlay. Parent and
   child never share live mutable files.
5. All children are tracked by their Run. A Run does not become terminal while
   a child is running. Detached children are not part of v1.
6. Capability inheritance is intersection-only: a child cannot gain a model,
   tool, effect, budget, workspace permission, or provider resource its parent
   and Run policy did not grant.
7. Every Run has one durable, monotonically increasing event sequence across
   its entire tree. This is observation order, not a claim about wall-clock
   execution order.
8. Replaying events after sequence `N` is exclusive, ordered, and at-least-once.
   Consumers deduplicate by `(runId, sequence)`.
9. Control-plane failures are typed errors. Agent execution failures are
   durable statuses and terminal events; `wait()` does not convert them into
   transport exceptions.
10. Idempotency keys are distinct from resource IDs. The runtime generates
    IDs; callers may safely retry creates and commands with an idempotency key.
11. No callback function appears in a serializable Agent, Session, Run, Model,
    Workspace, or Tool Provider specification.
12. Unknown fields and unknown enum values are rejected at creation time.

## Public value types

The names below are semantic wire names. TypeScript uses camelCase, Python uses
snake_case, and Rust uses idiomatic names with explicit serde mappings. Their
wire representations are identical.

### IDs and references

```text
SessionId       opaque string
RunId           opaque string
ArtifactId      opaque string
SecretRef       opaque string
ModelProfileId  caller-local identifier within a Session specification
ToolProviderId  caller-local identifier within a Session specification
AgentName       caller-local identifier within a Run tree
IdempotencyKey  caller-generated opaque string
```

IDs are never parsed for ordering or hierarchy. Run tree position is carried
by `AgentPath`, an ordered list of child ordinals assigned durably at spawn.

### Agent definition

An Agent Definition is a reusable value, not a remotely managed resource.

```text
AgentDefinition {
  name: AgentName
  instructions: ContentBlock[]
  model: ModelProfileId
  tools: ToolGrant[]
  budget?: AgentBudget
  output?: OutputContract
  metadata?: object
}

AgentBudget {
  maxTurns: positive integer
  maxToolCalls: non-negative integer
  maxPromptTokens: positive integer
  maxOutputTokens: positive integer
}

OutputContract {
  schema: JSON Schema
  name?: string
}
```

`defineAgent(...)` in TypeScript and Python is a validation helper. It does not
perform I/O and does not introduce another runtime noun.

### Content

```text
ContentBlock =
  | { type: "text", text: string }
  | { type: "artifact", artifactId: ArtifactId }
  | { type: "image", mediaType: string, data: base64 string }

AgentInput {
  content: ContentBlock[]
}
```

SDKs accept a plain string and normalize it to one text block before crossing
the transport Seam.

### Model profiles and credentials

```text
ModelProfile {
  id: ModelProfileId
  provider: "openai_compatible" | "anthropic"
  protocol: "responses" | "chat_completions" | "messages"
  model: string
  baseUrl?: HTTPS URL
  credential: SecretRef
  maxInputTokens: positive integer
  maxOutputTokens: positive integer
  temperature?: finite number
  topP?: finite number in [0, 1]
}
```

Provider/protocol compatibility is validated before Session creation.
`baseUrl` is subject to runtime egress policy, DNS/IP checks, redirect policy,
and tenant allowlists. The remote Adapter accepts HTTPS endpoints only. The
local Adapter may permit loopback HTTP when explicitly enabled in connection
options.

Secret material enters through `putSecret` and leaves memory only through the
Credential Resolver Seam. `putSecret` returns an opaque Secret Reference.
`deleteSecret` is idempotent and causes future provider calls using that
reference to fail with the ExecutionError code `secret_unavailable`; it does
not rewrite historical records.

```text
PutSecretInput {
  value: base64 string
  idempotencyKey?: IdempotencyKey
}
```

### Tool providers and grants

```text
ToolProvider =
  | { id: ToolProviderId, kind: "builtin" }
  | {
      id: ToolProviderId
      kind: "mcp_http"
      url: HTTPS URL
      credential?: SecretRef
      headers?: map<string, string | { secret: SecretRef }>
    }

ToolGrant {
  provider: ToolProviderId
  tool: string
  effects: ToolEffect[]
  maxCalls?: positive integer
}

ToolEffect =
  | "workspace_read"
  | "workspace_write"
  | "network_read"
  | "network_write"
  | "artifact_write"
  | "agent_spawn"
  | "agent_message"
```

V1 has two real Adapters at the Tool Provider Seam: Muzen built-ins and MCP
over HTTP. In-process SDK callbacks and remote callback tunnels are not part of
the v1 Interface.

Header values that carry credentials MUST be supplied as
`{ secret: SecretRef }`. Literal values for `Authorization`,
`Proxy-Authorization`, `Cookie`, and `X-Api-Key` are rejected with
`invalid_input` at creation time; these names are matched case-insensitively.
Stored records retain only the Secret Reference. An `mcp_http` URL is subject
to the same runtime egress policy, DNS/IP checks, redirect policy, and tenant
allowlists as a model `baseUrl`, including HTTPS-only remote access and the
explicitly enabled local-loopback exception.

Workspace effects are valid only for built-in tools whose file access the
runtime brokers. An `mcp_http` Tool Grant requesting `workspace_read` or
`workspace_write` is rejected with `invalid_input`; an external MCP server
receives no workspace path, overlay handle, or ambient Muzen credential.
Network effects on MCP tools are declared authority and audit metadata; the
tenant allowlist defines which MCP servers are trusted to honor them.

### Workspaces

```text
WorkspaceSpec {
  base:
    | { kind: "path", root: absolute path }
    | { kind: "git", url: HTTPS or SSH URL, revision: full commit id,
        credential?: SecretRef }
    | { kind: "snapshot", id: string }
  limits?: {
    maxFiles?: positive integer
    maxFileBytes?: positive integer
    maxBaseBytes?: positive integer
    maxOverlayBytes?: positive integer
  }
}
```

`path` is supported only by embedded and local-runner Adapters and is rejected
by the remote Adapter. There is one workspace behavior: immutable base plus a
private overlay. Read-only agents simply do not receive a
`workspace_write` Tool Grant.

A `path` base is ingested into an immutable content-addressed snapshot when
the Session is created. Later changes to the source directory never affect the
Session or its Runs. The runtime does not retain the source path as a live
execution dependency.

A `git` base URL is subject to the same runtime egress policy, DNS/IP checks,
redirect policy, and tenant allowlists as a model `baseUrl` and an `mcp_http`
URL, applied to both HTTPS and SSH before any fetch. The remote Adapter fetches
only allowlisted Git endpoints. The local Adapter's explicitly enabled
loopback exception applies to Git bases identically.

Independent root Sessions sharing the same base receive independent empty
overlays. A child Session receives a copy-on-write fork of its parent's overlay
at its durable spawn sequence. Child changes leave the runtime as a patch
artifact and are never merged implicitly.

### Session specification

```text
SessionSpec {
  agent: AgentDefinition
  models: ModelProfile[]
  toolProviders: ToolProvider[]
  workspace: WorkspaceSpec
  sessionBudget?: SessionBudget
  metadata?: object
}

SessionBudget {
  maxRuns?: positive integer
  maxLifetimeTokens?: positive integer
  maxLifetimeToolCalls?: positive integer
}
```

Agent Definition model and tool references must resolve within the Session
specification. Definitions inherited by children remain immutable for the
duration of a Run.

Exhausting any SessionBudget rejects the next Session operation with the
control-plane code `resource_exhausted`; it never creates a partially admitted
Run.

### Run specification

```text
RunSpec {
  roots: RunRoot[]
  limits: RunLimits
  idempotencyKey?: IdempotencyKey
  metadata?: object
}

RunRoot =
  | { sessionId: SessionId, input: AgentInput }
  | { session: SessionSpec, input: AgentInput,
      idempotencyKey?: IdempotencyKey }

RunLimits {
  maxActiveAgents: positive integer
  maxAgents: positive integer
  maxDepth: non-negative integer
  maxInputBytes: positive integer
  maxTotalTokens?: positive integer
  maxTotalToolCalls?: non-negative integer
  deadlineMs?: positive integer
}
```

All roots belong to one Run, share Run limits, share cancellation, and emit to
one event log. `AgentSession.run(input, limits)` normalizes to one existing
Session root.

`maxInputBytes` limits decoded ContentBlock bytes cumulatively across root
inputs and accepted send/spawn commands. A single image or other block larger
than the remaining limit is rejected before its bytes enter the durable event
log.

When `deadlineMs` elapses, the runtime durably records cancellation intent
exactly as if `run.cancel` had been invoked and emits `run.cancel_requested`
with reason `deadline`; aggregation then follows the standard cancellation
rules. When `maxTotalTokens` or `maxTotalToolCalls` is exhausted, every
non-terminal Agent terminates with `budget_exhausted` and the Run aggregates
through the standard rules.

A Session may be a root of at most one active Run. `run.start` and
`session.run` naming a Session that is archived or already has an `activeRunId`
fail with `conflict`.

## Supporting lifecycle values

```text
ConnectionOptions =
  | { transport: "local_runner", executable?: absolute path,
      allowLoopbackHttp?: boolean }
  | { transport: "remote", baseUrl: HTTPS URL, bearerToken?: string }

CreateOptions { idempotencyKey?: IdempotencyKey }
CommandOptions { idempotencyKey?: IdempotencyKey }
SingleRunOptions {
  limits: RunLimits
  idempotencyKey?: IdempotencyKey
  metadata?: object
}
EventOptions { after?: non-negative integer }
CancelOptions { reason?: string, idempotencyKey?: IdempotencyKey }
MessagePage { after?: string, limit?: positive integer }
Page<T> { items: T[], next?: string }

Usage {
  inputTokens: non-negative integer
  outputTokens: non-negative integer
  toolCalls: non-negative integer
}

AgentMessage {
  id: string
  sessionId: SessionId
  role: "system" | "user" | "assistant" | "tool"
  content: ContentBlock[]
  createdAt: timestamp
}

ArtifactRef { id: ArtifactId, mediaType: string, bytes: non-negative integer }
Artifact { ref: ArtifactRef, data: byte stream }
CommandReceipt { sequence: positive integer }
```

A receipt is returned only for an accepted command. A rejected command fails
with a typed MuzenError and produces no receipt. `sequence` is the sequence of
the durable event recording the command: `message.accepted` for send and
`run.cancel_requested` for cancel.

`AgentInputLike` is an SDK-only union of the exact `AgentInput` wire value or a
plain string. SDKs normalize strings before transport. TypeScript may add an
`AbortSignal` to its event-iterator options; this is iterator disposal, not a
wire field. Unknown event types received from a newer server are surfaced as
opaque Agent Events with the original type and payload; SDKs never discard or
reject them.

Wire timestamps are RFC 3339 UTC strings with exactly millisecond precision.
An artifact ContentBlock must resolve to an artifact readable by the current
tenant at command acceptance; otherwise the command fails with `not_found`.

An SDK holds a remote `bearerToken` only for the lifetime of its connection. It
never persists or logs that bootstrap credential.

## Public lifecycle Interface

### TypeScript

```ts
export async function connect(options?: ConnectionOptions): Promise<Muzen>;

export interface Muzen extends AsyncDisposable {
  capabilities(): Promise<Capabilities>;
  putSecret(input: PutSecretInput): Promise<SecretRef>;
  deleteSecret(secret: SecretRef): Promise<void>;
  createSession(spec: SessionSpec, options?: CreateOptions): Promise<AgentSession>;
  getSession(id: SessionId): Promise<AgentSession>;
  startRun(spec: RunSpec): Promise<Run>;
  getRun(id: RunId): Promise<Run>;
  close(): Promise<void>;
}

export interface AgentSession {
  readonly id: SessionId;
  snapshot(): Promise<SessionSnapshot>;
  messages(options?: { after?: string; limit?: number }):
    Promise<Page<AgentMessage>>;
  run(input: AgentInputLike, options: SingleRunOptions): Promise<Run>;
  archive(options?: CommandOptions): Promise<void>;
}

export interface Run {
  readonly id: RunId;
  snapshot(): Promise<RunSnapshot>;
  events(options?: EventOptions): AsyncIterable<AgentEvent>;
  wait(): Promise<RunResult>;
  result(): Promise<RunResult | undefined>;
  send(command: SendCommand): Promise<CommandReceipt>;
  spawn(command: SpawnCommand): Promise<AgentSession>;
  cancel(options?: CancelOptions): Promise<CommandReceipt>;
  artifact(id: ArtifactId): Promise<Artifact>;
}
```

### Python

```python
async def connect(options: ConnectionOptions | None = None) -> Muzen: ...

class Muzen:
    async def capabilities(self) -> Capabilities: ...
    async def put_secret(self, input: PutSecretInput) -> SecretRef: ...
    async def delete_secret(self, secret: SecretRef) -> None: ...
    async def create_session(self, spec: SessionSpec, *, idempotency_key: str | None = None) -> AgentSession: ...
    async def get_session(self, session_id: SessionId) -> AgentSession: ...
    async def start_run(self, spec: RunSpec) -> Run: ...
    async def get_run(self, run_id: RunId) -> Run: ...
    async def close(self) -> None: ...

class AgentSession:
    id: SessionId
    async def snapshot(self) -> SessionSnapshot: ...
    async def messages(self, *, after: str | None = None, limit: int | None = None) -> Page[AgentMessage]: ...
    async def run(self, input: AgentInputLike, *, limits: RunLimits,
                  idempotency_key: str | None = None) -> Run: ...
    async def archive(self, *, idempotency_key: str | None = None) -> None: ...

class Run:
    id: RunId
    async def snapshot(self) -> RunSnapshot: ...
    def events(self, *, after: int | None = None) -> AsyncIterator[AgentEvent]: ...
    async def wait(self) -> RunResult: ...
    async def result(self) -> RunResult | None: ...
    async def send(self, command: SendCommand) -> CommandReceipt: ...
    async def spawn(self, command: SpawnCommand) -> AgentSession: ...
    async def cancel(self, *, reason: str | None = None,
                     idempotency_key: str | None = None) -> CommandReceipt: ...
    async def artifact(self, artifact_id: ArtifactId) -> Artifact: ...
```

### Rust

```rust
pub struct Muzen { /* private */ }
pub struct AgentSession { /* private */ }
pub struct Run { /* private */ }

impl Muzen {
    pub async fn capabilities(&self) -> Result<Capabilities, MuzenError>;
    pub async fn put_secret(&self, input: PutSecretInput) -> Result<SecretRef, MuzenError>;
    pub async fn delete_secret(&self, secret: &SecretRef) -> Result<(), MuzenError>;
    pub async fn create_session(&self, spec: SessionSpec, options: CreateOptions)
        -> Result<AgentSession, MuzenError>;
    pub async fn get_session(&self, id: &SessionId) -> Result<AgentSession, MuzenError>;
    pub async fn start_run(&self, spec: RunSpec) -> Result<Run, MuzenError>;
    pub async fn get_run(&self, id: &RunId) -> Result<Run, MuzenError>;
    pub async fn close(&self) -> Result<(), MuzenError>;
}

impl AgentSession {
    pub fn id(&self) -> &SessionId;
    pub async fn snapshot(&self) -> Result<SessionSnapshot, MuzenError>;
    pub async fn messages(&self, page: MessagePage) -> Result<Page<AgentMessage>, MuzenError>;
    pub async fn run(&self, input: AgentInput, options: SingleRunOptions)
        -> Result<Run, MuzenError>;
    pub async fn archive(&self, options: CommandOptions) -> Result<(), MuzenError>;
}

impl Run {
    pub fn id(&self) -> &RunId;
    pub async fn snapshot(&self) -> Result<RunSnapshot, MuzenError>;
    pub fn events(&self, options: EventOptions) -> impl Stream<Item = Result<AgentEvent, MuzenError>>;
    pub async fn wait(&self) -> Result<RunResult, MuzenError>;
    pub async fn result(&self) -> Result<Option<RunResult>, MuzenError>;
    pub async fn send(&self, command: SendCommand) -> Result<CommandReceipt, MuzenError>;
    pub async fn spawn(&self, command: SpawnCommand) -> Result<AgentSession, MuzenError>;
    pub async fn cancel(&self, options: CancelOptions) -> Result<CommandReceipt, MuzenError>;
    pub async fn artifact(&self, id: &ArtifactId) -> Result<Artifact, MuzenError>;
}
```

Rust constructors for embedded, local-runner, and remote Adapters live in
their Adapter crates. The lifecycle Interface above is transport-neutral.
Omitting `ConnectionOptions` selects `local_runner` with loopback HTTP
disabled in TypeScript and Python.
Python consumers stop an event stream through task cancellation or `aclose()`
on the iterator; both send `run.unsubscribe`. Rust Stream drop and TypeScript
iterator return/abort do the same.

## Commands

### Send

```text
SendCommand {
  sessionId: SessionId
  input: AgentInput
  delivery: "steer" | "follow_up"
  idempotencyKey?: IdempotencyKey
}
```

`steer` is accepted only while the target Agent Session is executing. It is
delivered after the current tool batch and before the next model request.
`follow_up` is accepted while executing or waiting and is delivered when the
target would otherwise become terminal, extending the same Run. Commands to a
terminal target fail with `conflict`. The target Session must be tracked by the
Run receiving the command; otherwise the command fails with `not_found`.

### Spawn

```text
SpawnCommand {
  parentSessionId: SessionId
  agent: AgentDefinition
  input: AgentInput
  idempotencyKey?: IdempotencyKey
}
```

The child inherits the parent's immutable model table, Tool Provider table,
workspace base, and a point-in-time fork of the parent's overlay. Its effective
budget and Tool Grants are the intersection of the supplied Agent Definition,
the parent remainder, and Run limits. Spawn fails before creating a Session if
any reference or grant exceeds the parent authority.

The built-in `agent.spawn` and `agent.message` tools execute these same commands
inside the runtime; there is one scheduler Implementation.

## Snapshots, statuses, and results

```text
SessionStatus = "open" | "archived"

AgentStatus =
  "queued" | "running" | "waiting" |
  "completed" | "failed" | "cancelled" | "budget_exhausted"

RunStatus =
  "queued" | "running" | "waiting" |
  "completed" | "partial" | "failed" | "cancelled"

SessionSnapshot {
  id: SessionId
  status: SessionStatus
  activeRunId?: RunId
  createdAt: timestamp
  updatedAt: timestamp
  metadata: object
}

RunSnapshot {
  id: RunId
  status: RunStatus
  roots: SessionId[]
  agents: AgentSnapshot[] ordered by AgentPath
  lastSequence: non-negative integer
  createdAt: timestamp
  updatedAt: timestamp
}

AgentSnapshot {
  sessionId: SessionId
  parentSessionId?: SessionId
  path: non-negative integer[]
  status: AgentStatus
  model: ModelProfileId
  usage: Usage
}

RunResult {
  runId: RunId
  status: terminal RunStatus
  outputs: AgentOutput[] ordered by AgentPath
  usage: Usage
  artifacts: ArtifactRef[]
  metadata: object
}

AgentOutput {
  sessionId: SessionId
  path: non-negative integer[]
  status: terminal AgentStatus
  output?: string | JSON value
  usage: Usage
  error?: ExecutionError
}
```

Aggregation is deterministic:

- `completed`: every root and tracked child completed.
- `partial`: at least one root completed and at least one tracked Agent did not
  complete.
- `cancelled`: Run cancellation was requested and no root completed.
- `failed`: no root completed and the Run was not cancelled.

`budget_exhausted` is an Agent status and contributes to `partial` or `failed`
through the rules above.

`waiting` for an Agent means it has no active model or tool work and is held
non-terminal solely because an accepted `follow_up` is queued for delivery.
Delivery returns it to `running`; additional follow-ups may be accepted in
this window. An Agent with no queued follow-up transitions directly to a
terminal status. A Run is `waiting` when at least one tracked Agent is
`waiting` and no tracked Agent is `queued` or `running`.

`RunResult.outputs` is intentionally unpaginated in v1 because the required
`RunLimits.maxAgents` bounds its cardinality. Raising the deployment's
practical maximum agent count requires a versioned paginated result Interface.

## Run event log

```text
AgentEvent {
  runId: RunId
  sequence: non-negative integer
  type: stable event type
  timestamp: timestamp
  sessionId?: SessionId
  payload: object
}
```

Stable v1 event types:

```text
run.queued
run.started
run.waiting
run.completed
run.partial
run.failed
run.cancel_requested
run.cancelled
agent.created
agent.started
agent.waiting
agent.completed
agent.failed
agent.cancelled
agent.budget_exhausted
message.accepted
model.started
model.completed
model.failed
tool.started
tool.completed
tool.failed
artifact.created
workspace.changed
trace
```

For every Run, sequence starts at 1 and increases by exactly one. Persistence
of the event and its corresponding state transition is atomic. Reconnect with
`after=N` first replays every persisted event with sequence greater than N,
then live-tails new events without a replay/live gap.

`trace` payload is explicitly unstable. Every other event payload has a
versioned schema and participates in protocol compatibility.

## Cancellation and Session archival

`Run.cancel` durably records cancellation intent and returns a receipt. It
propagates through the active child token tree and interrupts queued work,
model streams, limiter waits, tool calls, and workspace operations. The Run is
terminal only after terminal events have been persisted for every tracked
Agent.

Cancellation intent is recorded at most once per Run. Repeated cancel calls
return the receipt containing the original `run.cancel_requested` sequence,
whether or not the caller supplies an idempotency key.

`AgentSession.archive` is idempotent once the Session is inactive. If the
Session has an active Run, `archive` fails with `conflict`; the caller must
cancel the Run explicitly and wait for its terminal event before archiving the
Session. Successful archival durably records the Session as archived and
rejects future Runs. Historical messages, events, results, and artifacts
remain readable.

## Error Interface

```text
MuzenError {
  code: ErrorCode
  message: string
  retryable: boolean
  details?: object
}

ExecutionError {
  code:
    "model_error" |
    "tool_error" |
    "secret_unavailable" |
    "workspace_error" |
    "budget_exhausted" |
    "cancelled"
  message: string
  retryable: boolean
  details?: object
}

ErrorCode =
  "invalid_input" |
  "not_found" |
  "conflict" |
  "unauthenticated" |
  "permission_denied" |
  "resource_exhausted" |
  "unsupported" |
  "unavailable" |
  "deadline_exceeded" |
  "internal"
```

The wire always carries this shape. TypeScript exposes `MuzenError extends
Error`, Python exposes `MuzenError(Exception)`, and Rust exposes a non-exhaustive
error enum with accessors for code, retryability, and details.

Provider, model, and tool failures after a Run starts are `ExecutionError`
values in Agent outputs and terminal events. They do not make `wait()` throw.
Transport loss while waiting does make the current `wait()` call fail with
`unavailable`; callers may call it again and receive the durable result.

## Capabilities

```text
Capabilities {
  protocolVersion: string
  workspaceBases: ("path" | "git" | "snapshot")[]
  toolProviderKinds: ("builtin" | "mcp_http")[]
  modelProtocols: ("responses" | "chat_completions" | "messages")[]
  maxReplayBatch: positive integer
}
```

`maxReplayBatch` is the maximum number of events returned in one replay
response. Replay is resumable: clients continue with `after` set to the last
received sequence, and SDK event iterators perform this continuation
transparently. Events are retained for the life of the Run record, so
`after=N` reaches every later event across successive batches. Run-record
retention and deletion are deployment policy in v1 and are reported through
administrative configuration, not the Agent Runtime Interface.

Capabilities describe deployment limits, not semantic feature gaps. An
Adapter that cannot implement Agent Sessions, multi-root Runs, children,
events, cancellation, results, secrets, artifacts, or durability does not
implement this Interface.

## Runner JSON-RPC Interface

Requests:

```text
muzen.capabilities
secret.put
secret.delete
session.create
session.get
session.messages
session.archive
run.start
run.get
run.result
run.events
run.unsubscribe
run.send
run.spawn
run.cancel
artifact.read
```

`run.events` returns replayed events and installs a subscription atomically.
Subsequent events arrive as `run.event` notifications containing the same
`AgentEvent` wire shape. `run.unsubscribe` is sent when the SDK iterator closes.

`artifact.read` parameters are
`{ artifactId: ArtifactId, offset: non-negative integer, maxBytes: positive integer }`.
Its result is `{ data: base64 string, eof: boolean }`. SDK `Artifact.data`
issues successive ranged reads and never buffers more than one range.

Every mutating request accepts an idempotency key. JSON-RPC request IDs remain
transport correlation only and are never used for product idempotency.

## HTTP Interface

```text
GET    /v1/capabilities
POST   /v1/secrets
DELETE /v1/secrets/{secretRef}
POST   /v1/sessions
GET    /v1/sessions/{sessionId}
GET    /v1/sessions/{sessionId}/messages
POST   /v1/sessions/{sessionId}/archive
POST   /v1/sessions/{sessionId}/runs
POST   /v1/runs
GET    /v1/runs/{runId}
GET    /v1/runs/{runId}/result
GET    /v1/runs/{runId}/events
POST   /v1/runs/{runId}/send
POST   /v1/runs/{runId}/spawn
POST   /v1/runs/{runId}/cancel
GET    /v1/runs/{runId}/artifacts/{artifactId}
```

The HTTP Adapter streams artifact response bodies and supports `Range`
requests with semantics equivalent to JSON-RPC `offset` and `maxBytes`.
Partial responses use `206 Partial Content` with `Content-Range`.

`GET /events` is real SSE. `Last-Event-ID` and `after` are equivalent; if both
are present they must agree. SSE IDs are decimal Run sequences. Authentication
and tenant identity are resolved before route dispatch. Resource IDs never
select tenant scope by themselves.

Mutating HTTP requests carry `Idempotency-Key`. A key reused with a different
canonical request body returns `409 conflict`.

The response body always contains the full `MuzenError` shape. The normative
HTTP status mapping is:

```text
invalid_input       400
unauthenticated     401
permission_denied   403
not_found           404
conflict            409
resource_exhausted  429
internal            500
unsupported         501
unavailable         503
deadline_exceeded   504
```

Agent execution failures still return successful control-plane HTTP responses
containing failed Agent/Run statuses. `GET /v1/runs/{runId}/result` returns
`200` with `null` while the Run is non-terminal. An event SSE stream ends only
after delivering the terminal Run event. Servers may send SSE comment
keepalives; they never advance the Run sequence or imply completion.

## Adapter Seams

The target architecture has real Seams only where two Implementations exist:

1. **Runtime transport Seam**: local runner JSON-RPC and remote HTTP/SSE.
2. **Model provider Seam**: OpenAI-compatible and Anthropic.
3. **Tool Provider Seam**: built-in tools and MCP HTTP.
4. **Workspace materializer Seam**: local path, Git, and content-addressed
   snapshot.
5. **Credential Resolver Seam**: local encrypted/ephemeral secret store and
   remote tenant secret store.
6. **Durable store Seam**: in-memory test Adapter and SQLite production-v1
   Adapter. A shared-database Adapter is required before multi-host workers.

Scheduler, Agent Loop, transcript compaction, capability intersection, event
folding, and artifact semantics are private Implementation details, not Seams.

## SDK parity rules

1. Rust owns the wire schema and generates the protocol fixture.
2. TypeScript and Python consume the same fixture in tests.
3. Every operation above has the same input validation, state transition,
   idempotency, error code, event sequence, cancellation, and result semantics.
4. Language idioms may differ only in casing, iteration primitives, disposal,
   and exception carriers.
5. Both SDKs run the same conformance scenarios against local-runner and remote
   Adapters.
6. No SDK adds a method that cannot be expressed over both transports.
7. SDK convenience helpers normalize to the exact wire types and contain no
   execution policy.
8. Because `RunSpec.limits` is required on the wire, every SDK requires limits
   at every Run-starting call site; no SDK supplies default limits.

## Approval checklist

Fable/high must return an explicit verdict for every item:

1. Domain nouns and Session/Run separation.
2. Multi-root Run as the only Swarm representation.
3. Agent Definition and structured output.
4. Model profile and BYO endpoint contract.
5. Secret upload/reference/deletion contract.
6. Tool Provider and Tool Grant contract.
7. Immutable-base/copy-on-write workspace contract.
8. Dynamic spawn, steering, and follow-up commands.
9. Capability inheritance and budgets.
10. Status aggregation and partial success.
11. Run-scoped event ordering and replay.
12. Cancellation and Session archival.
13. Error taxonomy and execution-failure plane.
14. Runner JSON-RPC mapping.
15. HTTP/SSE mapping.
16. TypeScript SDK Interface.
17. Python SDK Interface.
18. Rust Interface.
19. Local/remote durability and semantic parity.
20. Removal of review-named public concepts without compatibility aliases.

Approval requires `approved` for all twenty items and no unresolved blocking
finding. Conditional approval is not approval.
