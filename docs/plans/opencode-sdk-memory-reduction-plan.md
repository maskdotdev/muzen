# OpenCode SDK Memory Audit And Reduction Plan

Date: 2026-06-15

This is an exploration artifact only. It does not implement OpenCode changes.

## Executive Summary

The 600 MiB result is not caused by ten lightweight SDK client objects. It is caused by one warmed OpenCode server process running session prompt requests through a broad CLI/server/route/project/prompt graph.

The current-source no-reply sweep is the clearest proof. On OpenCode commit `5d0f86606ac30690f79f0a6a9f41a1f49fe95d0b`, one shared server with ten OpenCode sessions was already 481.4 MiB RSS at `opencode serve`, 489.9 MiB after first project bootstrap, and 492.2 MiB after all ten idle sessions. Ten concurrent `session.prompt({ noReply: true })` calls raised the same process to 632.0 MiB RSS, and it retained 632.3 MiB after session deletion. There were zero child processes.

That means the current 600 MiB class result does not require model streaming. The no-reply branch still enters `SessionPrompt.layer`, which materializes provider, processor, compaction, plugin, command, MCP, LSP, tool registry, image, child-process spawner, summary, system prompt, LLM, event, runtime flag, and database services before `if (input.noReply === true) return message`.

There are two different "session" shapes:

- `createOpencode()` starts a whole `opencode serve` child process and creates an HTTP client for it. Ten calls duplicate the entire server baseline.
- `createOpencodeClient()` plus `client.session.create()` creates ten OpenCode session records inside one already-running server. That is the measured shape that reaches about 600 MiB with active prompts.

The least-change path is to keep one shared server per repo/worktree, avoid `createOpencode()` per logical review session, run read-only sessions through stricter configuration, and add a narrow OpenCode-side server/prompt profile that does not import or initialize write/edit/shell/LSP/plugin/share/LLM paths for no-reply or read-only review.

## Reproduction

The minimal sweep wrapper is:

```sh
bun experiments/opencode-sdk-memory-audit/benchmark-opencode-sdk-memory.mjs \
  --counts 1,2,5,10 \
  --workload no-reply \
  --opencode-repo /Users/mask/code/opencode \
  --repo /Users/mask/code/muzen \
  --output-dir experiments/opencode-sdk-memory-audit/results/no-reply
```

For the real 600 MiB class workload, run the same sweep with model replies:

```sh
OPENAI_API_KEY=$OPENAI_API_KEY \
bun experiments/opencode-sdk-memory-audit/benchmark-opencode-sdk-memory.mjs \
  --counts 1,2,5,10 \
  --workload prompt \
  --opencode-repo /Users/mask/code/opencode \
  --repo /Users/mask/code/muzen \
  --output-dir experiments/opencode-sdk-memory-audit/results/prompt \
  --prompt 'Review slice {index}. Inspect source with read/search tools only. Do not edit. Return one concrete resource finding.'
```

The wrapper delegates to `experiments/opencode-sdk-memory-profile/profile-opencode-sessions.mjs`, which starts one source-launched OpenCode server, routes one SDK client to it, creates N sessions, captures process-tree RSS/fds/threads/children at each phase, and writes per-run JSON, event JSONL, replay JSON, `summary.json`, and `summary.md`.

This audit ran the no-reply command above against `/Users/mask/code/opencode` at commit `5d0f86606ac30690f79f0a6a9f41a1f49fe95d0b`. Results are in `experiments/opencode-sdk-memory-audit/results/no-reply-current/`.

To capture heap profiles from the OpenCode server process:

```sh
bun experiments/opencode-sdk-memory-audit/benchmark-opencode-sdk-memory.mjs \
  --counts 1,2,5,10 \
  --workload prompt \
  --heap-profile-dir experiments/opencode-sdk-memory-audit/results/heap \
  --output-dir experiments/opencode-sdk-memory-audit/results/prompt-heap
```

The base profiler starts the server with Bun heap profiling flags and emits markdown heap profiles on server exit. For interactive heap snapshots, start a single profiled server manually with the inspector:

```sh
OPENCODE_CONFIG_CONTENT='{"logLevel":"ERROR","lsp":false,"formatter":false,"snapshot":false,"share":"disabled","mcp":{},"plugin":[]}' \
bun --inspect=127.0.0.1:9229 \
  --cwd /Users/mask/code/opencode/packages/opencode \
  --conditions=browser src/index.ts serve --hostname=127.0.0.1 --port=4297
```

Then connect Chrome DevTools to `127.0.0.1:9229` and take Memory heap snapshots before bootstrap, after `all_sessions_created`, during prompt concurrency, after `all_prompts_complete`, and after `sessions_deleted`.

To capture RSS, child processes, file descriptors, VM regions, and native allocations while the harness is running:

```sh
PID=<opencode-server-pid-from-json>
ps -o pid,ppid,rss,vsz,%cpu,command -p "$PID"
pgrep -P "$PID" | xargs -r ps -o pid,ppid,rss,vsz,%cpu,command -p
lsof -nP -p "$PID" | wc -l
vmmap -summary "$PID" > experiments/opencode-sdk-memory-audit/results/vmmap-$PID.txt
sample "$PID" 10 -file experiments/opencode-sdk-memory-audit/results/sample-$PID.txt
xctrace record --template 'Allocations' --attach "$PID" --time-limit 30s --output experiments/opencode-sdk-memory-audit/results/alloc-$PID.trace
```

## Measured Memory Table

Current-source no-reply results:

| Sessions | Server start RSS | Project bootstrap RSS | Idle sessions RSS | No-reply prompts RSS | Retained/deleted RSS | Peak RSS | Children |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 484.9 MiB | 510.6 MiB | 511.4 MiB | 627.7 MiB | 627.7 MiB | 627.7 MiB | 0 |
| 2 | 470.3 MiB | 479.7 MiB | 480.8 MiB | 616.0 MiB | 616.2 MiB | 616.2 MiB | 0 |
| 5 | 466.4 MiB | 476.0 MiB | 477.4 MiB | 627.3 MiB | 627.5 MiB | 627.5 MiB | 0 |
| 10 | 481.4 MiB | 489.9 MiB | 492.2 MiB | 632.0 MiB | 632.3 MiB | 632.3 MiB | 0 |

Per-process memory at ten no-reply prompts:

| PID | PPID | RSS | VSZ | Threads | Command |
| ---: | ---: | ---: | ---: | ---: | --- |
| 59948 | 59947 | 632.0 MiB | 504061.5 MiB | 26 | `bun --cwd /Users/mask/code/opencode/packages/opencode --conditions=browser src/index.ts serve --hostname=127.0.0.1 --port=4200` |

The ten-session run's topology reported `sessionsShareProcess: true` and `childProcessCount: 0`. The 632 MiB number is therefore resident memory inside the shared Bun/OpenCode process, not duplicated child workers.

Older real prompt results under `experiments/opencode-sdk-memory-profile/results/` corroborate the same shape:

| Sessions | Server start RSS | Project bootstrap RSS | Idle sessions RSS | Prompt complete RSS | Retained/deleted RSS | Peak RSS | Children |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 355.0-361.7 MiB | 367.4-380.6 MiB | 368.1-381.6 MiB | 468.9-493.2 MiB | 469.3-493.4 MiB | n/a | 0-2 transient |
| 4 | 371.7 MiB | 389.1 MiB | 390.5 MiB | 564.6 MiB | 564.6 MiB | n/a | 2 transient |
| 8 | 355.5 MiB | 375.1 MiB | 378.8 MiB | 599.4 MiB | 599.4 MiB | n/a | 2 transient |
| 10 | 371.2 MiB | 396.1 MiB | 401.4 MiB | 492.8 MiB | 500.6 MiB | 635.9 MiB | 0 |

Idle session cost in the current ten-session run was only 2.3 MiB total after project bootstrap. No-reply prompt execution added about 139.8 MiB and retained it. Older real-prompt runs show the same retained high-water behavior with model/tool execution layered on top.

## Architecture Trace

### SDK Server Creation

`/Users/mask/code/opencode/packages/sdk/js/src/index.ts:8` exports `createOpencode(options)`. It calls `createOpencodeServer(options)` and then `createOpencodeClient({ baseUrl: server.url })`.

`/Users/mask/code/opencode/packages/sdk/js/src/server.ts:22` builds `serve --hostname=... --port=...` and launches `opencode` via `cross-spawn` at line 35. It passes `OPENCODE_CONFIG_CONTENT` at line 38 and waits for stdout beginning with `opencode server listening` at line 56.

`/Users/mask/code/opencode/packages/sdk/js/src/client.ts:33` creates only a generated HTTP client, installs a fetch wrapper, and optionally injects `x-opencode-directory`. This path does not spawn a process.

Conclusion: `createOpencode()` per logical session duplicates a full server. `createOpencodeClient()` against one server shares the large process.

### CLI And Server Startup

`/Users/mask/code/opencode/packages/opencode/src/index.ts:1` imports every CLI command module before yargs selects the requested command. Even `opencode serve` pays for run/generate/account/provider/agent/upgrade/uninstall/model/UI/debug/stats/MCP/GitHub/import/export/attach/TUI/ACP/web/PR/session/db/plugin command imports.

`/Users/mask/code/opencode/packages/opencode/src/cli/cmd/serve.ts:7` handles the serve command by importing `../../server/server`, calling `Server.listen(opts)`, and then waiting forever.

`/Users/mask/code/opencode/packages/opencode/src/server/routes/instance/httpapi/server.ts:1` imports the full server route and handler graph. Its app group at `server.ts:203` includes Npm, FSUtil, Database, Auth, Account, Config, Git, Ripgrep, Storage, Snapshot, Plugin, ModelsDev, Provider, Agent, Skill, Session, SessionProcessor, SessionPrompt, LLM, LSP, MCP, Command, ToolRegistry, Format, Project, Worktree, share, event, and pty services.

`/Users/mask/code/opencode/packages/opencode/src/server/server.ts:82` starts the listener, builds HTTP/WebSocket routing, and keeps a listener scope. `server.ts:123` builds the listener layer with a fresh memo map, while route services use the process-global memo map from `/Users/mask/code/opencode/packages/core/src/effect/memo-map.ts:3`.

Impact: this explains the 466-511 MiB fixed/current baseline before useful review work. The older checkout baseline was 355-390 MiB, so the current route/import graph has grown materially.

### Instance And Project Bootstrap

`/Users/mask/code/opencode/packages/opencode/src/project/instance-store.ts:37` creates a server-local `Map<string, Entry>` cache. `load()` at line 108 reuses one bootstrapped instance per directory inside the same server process.

`/Users/mask/code/opencode/packages/opencode/src/project/bootstrap.ts:32` runs project bootstrap. It materializes config, plugin, LSP, share, format, VCS, snapshot, and project services. Even with `lsp:false`, `snapshot:false`, `formatter:false`, `share:"disabled"`, empty plugin, and empty MCP config, the service graph is still broad.

Impact: project bootstrap added about 8.5-25.7 MiB in the current no-reply sweep. It is shared by all sessions for the same directory in one server, but duplicated across separate servers.

### Session Creation

`/Users/mask/code/opencode/packages/opencode/src/session/session.ts:541` creates a session `Info` record and publishes a durable created event. `session.ts:648` removes sessions and cancels background jobs, but it does not tear down warmed process-wide modules, provider SDKs, project services, or runtime caches.

Impact: ten idle sessions added only 2.3 MiB after bootstrap in the current sweep. Session records are not the primary source of the 600 MiB RSS.

### Prompt Execution

`/Users/mask/code/opencode/packages/opencode/src/session/prompt.ts:97` materializes the prompt service graph: status/session/agent/provider/processor/compaction/plugin/command/config/permission/fs/MCP/LSP/tool registry/truncate/image/child process spawner/instruction/run state/revert/summary/system prompt/LLM/events/runtime flags/database.

`prompt.ts:1145` loads compacted message history each loop. `prompt.ts:1279` resolves tools each turn. `prompt.ts:1325` converts messages, system prompt, environment, and instructions into model messages. `prompt.ts:1336` calls the processor.

The no-reply branch is at `prompt.ts:1122`, after `SessionPrompt.layer` has yielded the heavy dependencies at `prompt.ts:100-126` and after `createUserMessage(input)` at `prompt.ts:1110`.

`/Users/mask/code/opencode/packages/opencode/src/session/processor.ts:110` creates per-run processor state including snapshot tracking, toolcall maps, text buffers, reasoning maps, and abort state. Cleanup at `processor.ts:846` flushes current text/reasoning/toolcalls, but already-warmed module/provider/runtime memory remains resident.

Impact: current no-reply prompts added 116-150 MiB after idle and retained it. Two older real prompts added about 125 MiB after idle; ten concurrent real prompts peaked at 635.9 MiB.

### Tool Registry

`/Users/mask/code/opencode/packages/opencode/src/tool/registry.ts:1` imports all built-in tools at module load time. `registry.ts:86` materializes services for invalid/task/read/question/todo/LSP/plan/webfetch/websearch/shell/glob/write/edit/grep/patch/skill/agent. `registry.ts:198` initializes every built-in before later filtering.

`/Users/mask/code/opencode/packages/opencode/src/session/tools.ts:74` converts resolved tools to AI SDK tool objects and JSON schemas per provider turn.

Impact: permission denial prevents use, but it does not avoid the import/init cost. This is a high-leverage OpenCode change after the server startup and no-reply prompt split.

### Provider And LLM

`/Users/mask/code/opencode/packages/opencode/src/provider/provider.ts:1300` stores providers, language models, model loaders, vars loaders, SDK modules, and discovery loaders in maps. `provider.ts:1730` imports provider SDKs and caches them. `provider.ts:1784` caches language models by provider/model key.

`/Users/mask/code/opencode/packages/opencode/src/session/llm.ts:280` calls AI SDK `streamText(...)`. `llm/request.ts:56` prepares system/messages/tools/params/headers for each run.

Impact: provider clients and language models are shared inside one server, but duplicated across server processes. Stream request objects and converted messages are per-turn; the retained RSS looks like warmed SDK/modules/runtime arenas more than a session leak.

### LSP, Watchers, Shells, Workers

`/Users/mask/code/opencode/packages/opencode/src/lsp/lsp.ts:153` short-circuits when `cfg.lsp` is false. `lsp/server.ts` contains many language-server spawn definitions, but no LSP server should spawn in the reduced profile.

`/Users/mask/code/opencode/packages/core/src/filesystem/watcher.ts:63` returns a no-op watcher when `OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER` is set, and subscriptions are additionally gated at line 100 by `OPENCODE_EXPERIMENTAL_FILEWATCHER`.

The measured ten-session prompt run had zero child processes at the prompt-complete phase. Older 2/4/8 runs sampled transient `/usr/bin/afplay /System/Library/Sounds/Ping.aiff` children; these were not persistent OpenCode workers.

Impact: LSP/watchers/shells can inflate memory in normal interactive use, but they do not explain the reduced 600 MiB class result.

## Memory Model

Fixed baseline:

- 466-511 MiB RSS for current source-launched `opencode serve` before meaningful work.
- Main cause: broad CLI import floor plus server route/service import graph.

Project baseline:

- +8.5-25.7 MiB after first directory-routed request in the current sweep.
- Shared across all sessions for the same directory inside one server.

Idle session increment:

- About +0.23 MiB/session in the current ten-session run, +2.3 MiB total for ten sessions.
- Session metadata is durable/event-backed and small.

Prompt increment:

- Current no-reply prompt endpoint: +116.3 MiB for one session, +135.2 MiB for two sessions, +149.9 MiB for five sessions, +139.8 MiB for ten sessions.
- Older real two-session prompt path: +125.1 MiB after idle.
- Older ten real concurrent prompts: peak 635.9 MiB, then retained near 500 MiB.

Retained memory after completion:

- Deleting sessions did not reduce RSS. It removed records/background jobs, but the warmed Bun process, imported modules, Effect layers, provider SDKs, tool registry, AI SDK, JSON schemas, and runtime/native allocations remained resident. In the current ten-session no-reply run, RSS went from 632.0 MiB at prompt completion to 632.3 MiB after deletion.

Leak assessment:

- No evidence that each session leaks a full runtime or full transcript cache.
- Evidence of sticky high-water RSS after prompt work. This can be allocator/GC/module/native retention rather than a leak, and should be verified with the heap and allocation commands above.
- Event pubsubs are unbounded in `/Users/mask/code/opencode/packages/core/src/event.ts:181`, but benchmark evidence did not show slow-subscriber buildup as the primary cause.

## Hypotheses

| Hypothesis | Result | Evidence |
| --- | --- | --- |
| Each session duplicates runtime/server state. | False for `client.session.create()` in one server; true for `createOpencode()` per logical session. | `InstanceStore` caches by directory in one process; ten idle sessions added only 2.3 MiB. SDK `createOpencode()` spawns `opencode serve`. |
| Each session retains full transcript/context/tool state. | Mostly false as a retained-memory explanation. | Message history is durable in DB and hydrated per turn by `message-v2.ts`; idle sessions are tiny. The current no-reply run reached 632 MiB before model transcript streaming existed. |
| Repo scanning/indexing/globbing/caches are repeated. | Per server/directory yes, per session mostly no. | `InstanceStore.load()` reuses a directory instance in one server; separate servers repeat config/skill/tool/project bootstrap. |
| Watchers/LSP/shell/processes inflate RSS. | Disproved for reduced run; possible in full interactive configs. | `lsp:false`, watcher disabled, ten-session run child count zero. |
| Provider clients/streams/request buffers are retained. | Provider graph yes, stream buffers no as primary cause. | Current no-reply run does not call model streaming, but `SessionPrompt.layer` still yields Provider and LLM services. Provider SDK/language model caches are retained per server. |
| GC behavior causes high RSS even if heap is smaller. | Likely, needs heap/native proof. | `--smol` saved only about 3 MiB in prior runs; RSS remained high after deletion. Heap snapshots and `vmmap` should separate JS retained heap from native/module/allocator RSS. |

## Source-Level Findings Ranked By Impact

1. Broad server startup imports.
   - Source: `/Users/mask/code/opencode/packages/opencode/src/index.ts:1`, `/Users/mask/code/opencode/packages/opencode/src/cli/cmd/serve.ts:7`, `/Users/mask/code/opencode/packages/opencode/src/server/routes/instance/httpapi/server.ts:1`, `server.ts:203`.
   - Allocation cause: top-level imports for all command families before serve is selected, then a route module that imports and groups the full interactive server service graph.
   - Likely impact: high. Current server start is 466-511 MiB. Old experimental `serve-lite` saved about 80 MiB before the current route graph growth; a current direct/lazy route entrypoint should save more but needs measurement.

2. `SessionPrompt.layer` materializes the full prompt graph before `noReply` returns.
   - Source: `/Users/mask/code/opencode/packages/opencode/src/session/prompt.ts:97`, `prompt.ts:100`, `prompt.ts:1122`, `prompt.ts:1693`.
   - Allocation cause: no-reply prompts still load provider, processor, compaction, plugin, command, MCP, LSP, tool registry, image, process spawner, summary, system prompt, LLM, events, and database.
   - Likely impact: about 116-150 MiB in the current no-reply sweep.

3. Tool registry imports and initializes all built-ins before read-only filtering.
   - Source: `/Users/mask/code/opencode/packages/opencode/src/tool/registry.ts:1`, `registry.ts:86`, `registry.ts:198`, `/Users/mask/code/opencode/packages/opencode/src/session/tools.ts:74`.
   - Allocation cause: shell/edit/write/patch/web/task/LSP/tool schemas and dependencies are warmed for read-only prompts.
   - Likely impact: about 30 MiB saved in old allowlist trial, about 98 MiB combined with server-only entrypoint.

4. Real model prompt processing adds per-turn state on top of no-reply warmup.
   - Source: `/Users/mask/code/opencode/packages/opencode/src/session/prompt.ts:97`, `prompt.ts:1279`, `prompt.ts:1325`, `prompt.ts:1336`, `/Users/mask/code/opencode/packages/opencode/src/session/processor.ts:110`.
   - Allocation cause: compaction, command, MCP, LSP, image, process spawning, revert, summary, system prompt, processor, tool schema, and LLM services are all present.
   - Likely impact: 50-125 MiB during first real model prompts depending on concurrency and warm state, after the no-reply graph is already warm.

5. Project bootstrap materializes disabled services.
   - Source: `/Users/mask/code/opencode/packages/opencode/src/project/bootstrap.ts:23`, `bootstrap.ts:32`.
   - Allocation cause: LSP/share/format/snapshot/VCS/project/plugin services are initialized together.
   - Likely impact: 8.5-25.7 MiB in current reduced profiles.

6. Provider SDK and language model caches are server-local.
   - Source: `/Users/mask/code/opencode/packages/opencode/src/provider/provider.ts:1300`, `provider.ts:1730`, `provider.ts:1784`.
   - Allocation cause: dynamic provider imports, SDK modules, language model wrappers, model metadata.
   - Likely impact: shared inside one server, multiplied by per-session servers.

## Recommendations

### Immediate SDK Usage Mitigations

- Use one `opencode serve` process per repo/worktree and one `createOpencodeClient()` for all logical sessions.
  Expected savings versus ten `createOpencode()` calls: roughly 9x the fixed 466-511 MiB current source server baseline.

- Keep session concurrency at 4-8 until prompt-path memory is narrowed.
  Expected savings versus unbounded fanout: avoids prompt-time peaks above the warmed 500-700 MiB envelope.

- Use a read-only primary agent and deny `write`, `edit`, `patch`, `todowrite`, `shell`, repo clone/overview, and subagent task use unless explicitly needed.
  Expected savings today: limited RSS savings because the registry still imports tools, but it avoids process/tool execution and reduces risk.

- Disable LSP, snapshots, share, formatter, MCP, default plugins, external skills, and file watcher for review-only benchmark runs.
  Expected savings today: reduces children/fds/work; baseline RSS savings are modest unless OpenCode stops materializing these services.

- Treat session deletion as data cleanup, not memory reclamation.
  Expected savings: none in RSS; prevents durable store/event buildup.

### Least-Change OpenCode Fix Plan

1. Add a server-only entrypoint.
   - Goal: let SDK `createOpencodeServer` launch a direct server module instead of the full CLI index.
   - Minimal change: add a small source/binary entrypoint that imports logging/config/server only, then calls `Server.listen`. Keep the existing CLI path separate.
   - Expected savings: at least the old 70-90 MiB `serve-lite` savings; likely more if paired with lazy route imports.

2. Split no-reply/user-message creation from the full prompt service.
   - Goal: `session.prompt({ noReply: true })` should create the user message without Provider, LLM, ToolRegistry, MCP, LSP, image, command, compaction, processor, or process-spawner services.
   - Minimal change: move `createUserMessage` into a narrower service or handler path that depends only on Session, Config/Permission where required, FSUtil/Image only if file/image parts are present, Event bridge, RuntimeFlags, and Database.
   - Expected savings: 100-140 MiB for the no-reply benchmark, based on the current 116-150 MiB prompt jump.

3. Add an early tool allowlist to `ToolRegistry`.
   - Goal: skip import/init/schema work for tools not allowed by the session agent/profile.
   - Minimal change: thread a config-level read-only allowlist into registry construction before `Tool.init`.
   - Do not implement benchmark-specific names; use a product-level `tools.enabled` or `server.profile: "review-readonly"` contract.
   - Expected savings: about 25-35 MiB alone, about 95-105 MiB combined with server-only entrypoint in old trials.

4. Narrow bootstrap for read-only review.
   - Goal: avoid materializing LSP/share/snapshot/formatter/file watcher/plugin services when explicitly disabled.
   - Minimal change: replace no-op configured services with lazy layers or conditional branches in `project/bootstrap.ts`.
   - Expected savings: 10-30 MiB plus fewer fds/native handles.

5. Make model-reply prompt dependencies conditional.
   - Goal: read-only prompts should not load image, shell/process spawner, revert, write/edit/patch, MCP, or LSP unless tools require them.
   - Minimal change: split `SessionPrompt.layer` into common + capability-specific layers.
   - Expected savings: 30-80 MiB depending on tool set and model path.

6. Add explicit memory telemetry to the SDK/server.
   - Goal: measure `rss`, JS heap, external memory, child process RSS, fds, and active sessions at phase boundaries.
   - Minimal change: expose a debug endpoint or CLI heap command usable by benchmark harnesses.
   - Expected savings: none directly; prevents regressions.

### Lightweight Read-Only Multi-Session Review Agent Design

- One OpenCode server per worktree.
- One SDK client per server.
- One parent review session per PR slice, not one server per slice.
- A product-level `review-readonly` profile:
  - tools: `read`, `glob`, `grep` only by default;
  - optional `webfetch`/`websearch` behind explicit enablement;
  - no shell, no edit/write/patch, no task/subagent, no LSP, no MCP, no snapshots, no share.
- Preload shared provider/model once, then run bounded concurrent prompts.
- Store final findings outside OpenCode session history if the review product does not need replayable chat transcripts.

Expected memory after least-change OpenCode work:

- Fixed server baseline: 466-511 MiB down to roughly 330-400 MiB.
- Warm ten-session no-reply/read-only prompt workload: 616-632 MiB down to roughly 380-480 MiB.
- Per-logical-session incremental retained memory: stay near sub-MiB to 1 MiB for idle records.
- Per-session server design: avoid entirely; otherwise it remains multi-GB for ten sessions.
