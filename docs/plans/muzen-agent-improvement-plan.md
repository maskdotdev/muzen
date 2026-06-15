# Muzen Agent Improvement Plan

Generated: 2026-06-13

Status: in progress.

Progress:

- Phase 0 trace foundation landed:
  - `RuntimeEvent::AgentTrace` and host-facing `ReviewEvent::AgentTrace`.
  - Runtime/review event JSONL round-trip coverage for agent trace events.
  - Planned-review trace emission for model turn preparation, exposed tools,
    transcript compaction, model outputs, requested tool calls, tool-batch
    policy planning, candidate finding decisions, and candidate synthesis
    summaries.
  - Direct-session trace emission for compaction, model turn preparation,
    model outputs, requested tool calls, and tool-batch policy planning.
  - Run-level resource samples at run/snapshot start and finish, plus
    per-turn peak RSS samples.
  - `reviewer::agent_trace::build_agent_trace_report` groups runtime event
    records by session and turn for local audit reports.
  - `muzen bench --trace-output-dir <dir>` writes `runtime-events.jsonl`,
    `agent-trace.json`, and `event-coverage.json` for local experiments.
  - `bench/review-quality/run-production-review.mjs --trace-output-dir <dir>`
    writes full streamed event JSONL, runtime/review event JSONL, grouped
    `agent-trace.json`, and `event-coverage.json` for real quality-corpus runs.
  - Review-quality result JSON now includes compact audit coverage and trace
    summaries even when raw trace artifacts are not requested.
  - The opencode profiling harness now captures `/event` SSE records into JSONL,
    captures post-run session message/sync replay snapshots, and summarizes
    event/message/tool-part types alongside memory phases.
  - `experiments/opencode-sdk-memory-profile/compare-muzen-opencode.mjs`
    produces a normalized side-by-side Muzen/opencode comparison artifact.
  - Current smoke evidence is summarized in
    `experiments/opencode-sdk-memory-profile/muzen-opencode-current-comparison.md`.
    The latest opencode replay smoke verified actual tool use (`glob`: 1,
    `read`: 2) even though `/event` itself did not expose semantic tool events.
  - `experiments/opencode-sdk-memory-profile/run-opencode-review-quality.mjs`
    runs opencode against the same anti-cheat fixture shape as Muzen, requiring
    replayed tool parts before the run can pass.
  - `experiments/opencode-sdk-memory-profile/run-opencode-anti-cheat-suite.mjs`
    can run every opencode anti-cheat negative control and summarize findings,
    RSS, prompt time, and replayed tool names.
  - First matched negative-control comparison landed for
    `safe-sms-retry-cleanup`: Muzen returned clean at ~18.7 MiB peak RSS;
    opencode returned clean with replayed `read`/`glob`/`grep` tool parts at
    477.4 MiB peak RSS.
  - Second opencode matched negative control landed for
    `safe-oauth-return-shape`: opencode returned clean with two replayed `read`
    tool parts at 443.9 MiB peak RSS.
  - First matched positive PR comparison landed for `cal-pr-14943`: Muzen and
    opencode both hit the golden SMS retry-cleanup defect with `gpt-5.4-mini`.
    Muzen peaked at ~45.0 MiB RSS with 346 combined runtime/review events and
    67 agent-trace events. opencode peaked at 576.0 MiB RSS with 1,878 SSE
    events and replayed tool parts (`read`: 9, `grep`: 5, `glob`: 1). The
    quality gap in this case is not exploration failure: Muzen found the issue,
    but also published a duplicate second finding, counted as one false
    positive by the scorer.
  - Second matched positive PR comparison landed for `cal-pr-8330`: the initial
    Muzen run missed all three golden issues with no candidates, while opencode
    found two of three with no false positives. The enhanced trace showed Muzen
    had inspected relevant files and queries but failed to promote suspicious
    TimeBoundary semantics into candidates. A prompt-only boundary/date guidance
    change improved Muzen to one of three; deterministic TimeBoundary candidate
    probes then improved the planned-review measured run to three of three
    goldens with one extra model-generated finding. That planned Muzen run
    peaked at ~36.5 MiB RSS with 584 combined runtime/review events and 105
    agent-trace events. opencode
    peaked at 597.3 MiB RSS with 1,752 SSE events and replayed tool parts
    (`read`: 11, `grep`: 16, `glob`: 2).
  - Audit trace now records human-readable redacted tool-call argument
    summaries in addition to byte counts and hashes: path, line range, query,
    and parseability. Successful tool completion events now include compact
    result details such as path, line range, search query, match counts,
    first-match location, cache status, and output bytes. The review-quality JS
    trace normalizer preserves those details in grouped `agent-trace.json`.
  - The default model is now `gpt-5.4-mini` for cheap OpenAI-backed review
    experiments.
  - TimeBoundary deterministic probes now cover working-hours checks that reuse
    slot start as slot end, zero-length date override checks that compare Dayjs
    wrapper identity, and selected-slot filtering that drops date override
    availability before `checkIfIsAvailable` receives date override context.
  - Candidate deduplication now merges overlapping same-evidence claims more
    aggressively, fixing the duplicate-publication shape observed on
    `cal-pr-14943` in the measured after-dedupe run.
  - Structured response contracts now exist for planned-review terminal JSON
    outputs: unit results, contract-pack results, final synthesis, and finding
    challenge verdicts. `SessionScope` carries an optional response format;
    OpenAI Chat Completions sends it as `response_format: {type: json_schema}`;
    OpenAI Responses sends it as `text.format`; `model_turn_prepared` trace
    events record whether structured output was requested, the schema name, the
    strict flag, and a schema digest.
  - Current structured-output boundary: terminal/final JSON turns are
    schema-bound in both planned review and direct sessions. Direct-session
    exploration turns keep tools enabled and explicitly withhold the final
    response schema; the tool-free final answer turn preserves the requested
    response format. If a direct session emits early text before the final
    structured turn, the runtime defers it, traces
    `early_text_deferred_for_structured_final`, and asks for the structured
    final response.
  - Provider-visible exploration aliases now exist for the default review tool
    registry: `read` -> `read_file`, `grep` -> `search_text`, `glob` ->
    `list_files`, `diff` -> `read_diff`, `imports` -> `list_imports`, and
    `tests` -> `find_tests_for_file`. Provider calls are mapped back through
    the alias table before execution, so internal tool IDs, tool metrics,
    artifacts, policy checks, and audit events remain stable.
  - Builtin model-facing tool descriptions now explain when to use `read`,
    `grep`, `glob`, `imports`, and `tests` for caller/helper/test/contract
    exploration. Planned-review follow-up prompts now mention the aliases
    rather than internal `search_text`/`list_imports` names where the model is
    expected to act.
  - Explicit risk playbooks now drive planned-review prompts and traces:
    `ReturnShape`, `CredentialOwnership`, `QueryScope`, `TimeBoundary`,
    `AuthScope`, and `RepeatedIntegration`. Unit prompts include a concrete
    playbook checklist, contract-pack prompts include the pack-specific
    playbook, final synthesis includes playbook names per unit, and
    `risk_playbooks_selected` trace events record selected playbooks plus
    reasons/seeds before the first model turn.
  - Lightweight explore workers now run inside planned-review high-risk units
    when the parent budget has enough slack. Workers use child `SessionScope`s,
    the same model router, the same shared `ToolEngine`/artifact store, and the
    parent unit's remaining tool budget. They return evidence summaries only;
    parent sessions merge worker evidence into the transcript and evidence
    tracker, while findings still come only from normal unit/pack/synthesis
    validation.
  - Explore-worker trace events now cover planned, started, completed, merged,
    and skipped states, with parent/worker ids, playbook names, checked paths,
    evidence artifact ids, and summary digests.
  - Tool-call recovery now performs a bounded pre-policy repair pass for
    provider-visible alias confusion, JSON-string path/query arguments,
    path-field aliases, camelCase/reversed line ranges, and empty/null/array
    arguments for empty-arg tools. Accepted repairs emit `tool_call_repair`
    trace events with original and canonical tool ids, canonical argument
    byte/hash summaries, redacted canonical argument summaries, and repair kind
    tags before the repaired calls go through the normal policy and
    authorization path. Recognizable but unsafe malformed builtin arguments are
    traced as attempted rejected repairs, while non-JSON malformed arguments,
    unknown tools, denied tools, path denials, and budget denials remain
    auditable as no-repair-attempt failures.
  - Review-quality trace reports now derive `audit-diagnostics.json` and embed
    the same diagnostics into production review result JSON. The diagnostics
    summarize requested/completed/denied tool calls, per-tool request and
    completion counts, accepted/rejected repairs, candidate decision counts,
    synthesis counts/rejection reasons, transcript compaction, explore-worker
    activity, resource samples, and per-session audit summaries.
  - `bench/review-quality/compare-corpus-runs.mjs` now creates a corpus matrix
    across arbitrary legs such as `blind`, `hinted`, `probe`, and `opencode`.
    It normalizes Muzen production-review results and opencode review-quality
    results into per-case quality, efficiency, tool-use, audit-coverage, and
    RSS-ratio summaries. The current generated matrix at
    `bench/results-review-quality/corpus-comparison-current-gpt-5.4-mini.json`
    covers 17 runs across the current matched positive/negative benchmark cases
    plus the generated audit-only fixture, with `--require-scenarios all`
    passing.
  - `bench/review-quality/build-current-corpus-comparison.mjs` now rebuilds the
    current matrix from the checked-in Muzen/opencode result artifacts listed
    in `bench/review-quality/current-corpus-runs.json`, regenerates the
    audit-only fixture, and runs `--require-scenarios all` by default so the
    current report is reproducible without copying a long comparator command.
  - The corpus matrix now adds conservative outcome and failure-cause
    classification from audit evidence: clean-control pass/fail, missed
    expected issue, false-positive-only runs, explored-with-no-candidates,
    partial recall/wrong findings, opencode misses after replayed tool use, and
    audit gaps such as missing candidate decision traces on older Muzen runs.
  - The corpus matrix now includes an `auditScenarioCoverage` gate that records
    whether the current artifact set covers clean results, missed defects,
    published findings, rejected candidates, denied tool calls, compacted
    transcripts, accepted tool repairs, rejected tool repairs, and
    no-repair-attempt tool failures. The current matrix proves scored
    clean/missed/published/rejected-candidate coverage and explicitly lists
    denied-tool, compacted-transcript, accepted-repair, rejected-repair, and
    no-repair-attempt scenarios as scored gaps while preserving audit-only
    evidence for them separately.
  - `compare-corpus-runs.mjs --require-scenarios core|all` now turns scenario
    coverage into a benchmark gate. The command writes the report either way
    and exits with code 2 when required audit scenarios are missing, so local
    experiments and CI can enforce coverage rather than manually inspecting the
    matrix.
  - Audit scenario coverage now records combined `evidence`, `scoredEvidence`,
    `auditOnlyEvidence`, `coveredByScored`, and `coveredByAuditOnly` for every
    scenario, plus top-level `scoredMissing`, `coveredOnlyByAuditOnly`, and
    `coveredByScored` arrays. This keeps the all-scenario gate useful while
    making it explicit which scenarios are proven only by audit-only fixtures
    or controlled probes versus real model-backed benchmark legs.
  - Scenario coverage now splits audit-only provenance into
    `controlledProbeEvidence` and `syntheticFixtureEvidence`, with matching
    coverage booleans. This makes the report show whether an audit edge is
    covered by an end-to-end controlled runner probe, by a generated synthetic
    trace fixture, or both.
  - The corpus comparator now has a separate
    `--require-scored-scenarios core|all|LIST` gate. The current-corpus wrapper
    requires combined `all` coverage and scored `core` coverage by default, so
    clean/missed/published outcomes must come from real benchmark runs while
    repair/denial/compaction edge mechanics can remain audit-only probe-backed
    until targeted scored runs exist.
  - The current corpus manifest now includes
    `probe-rejected-candidate:cal-pr-8330`, a scored Muzen run with
    `rejectedCandidateCount: 1`, moving the rejected-candidate scenario from
    fixture-only coverage to scored coverage. Remaining fixture-only scored
    gaps are denied tool calls, transcript compaction, accepted tool repair,
    rejected tool repair, and no-repair-attempt failures.
  - `bench/review-quality/tools/run-controlled-audit-probe.mjs` now runs
    audit-only controlled probes through `muzen-runner stdio` and a local
    OpenAI-compatible Responses server. The `tool-repair-denial` scenario
    proves denied tool calls, accepted tool-call repair, rejected repair
    attempts, and no-repair-attempt failures through the real parser, tool
    policy, event stream, and trace normalizer. The `compaction` scenario uses
    direct-session mode plus a tiny prompt budget to prove
    `transcript_compacted` from an end-to-end runner trace. These probes are
    marked `auditOnly: true` and `auditSource: "controlled-runner-probe"`, so
    the corpus matrix can use them as audit evidence without counting them as
    review quality, memory, token, or RSS-ratio evidence.
  - Controlled audit probes are self-validating: the probe command exits
    non-zero if the expected denied-tool, repair, no-repair, or compaction
    trace signals are absent. `build-current-corpus-comparison.mjs` regenerates
    both controlled probes by default before comparing the corpus, so the
    current all-scenario audit gate is reproducible rather than dependent on
    stale ignored result files.
  - The corpus comparator now has a
    `--require-controlled-probe-scenarios edge|all|LIST` gate. The
    current-corpus wrapper defaults this gate to `edge`, so denied tool calls,
    transcript compaction, accepted repairs, rejected repair attempts, and
    no-repair-attempt failures must be proven by controlled runner probes. The
    synthetic audit fixture alone can no longer satisfy the default edge
    mechanics gate.
  - `bench/review-quality/format-corpus-comparison.mjs` now formats corpus
    comparison JSON into a human-readable Markdown report with gate status,
    scored/controlled/synthetic scenario provenance, leg aggregates, case-level
    RSS ratios, outcomes, and failure causes. The current-corpus wrapper writes
    this Markdown report by default next to the JSON matrix.
  - The Markdown corpus report now includes a matched Muzen-vs-opencode section
    for cases where both runtimes have scored results. It chooses the best
    scored Muzen leg and best opencode leg per case and shows hit/miss/false
    positive counts, tool counts, peak RSS, RSS ratio, and combined failure
    causes side by side.
  - The corpus comparator now has a `--require-min-rss-ratio N` memory gate.
    The current-corpus wrapper defaults it to `10`, requiring every matched
    scored opencode/Muzen case to keep opencode peak RSS at least 10x the best
    Muzen peak RSS. This turns Muzen's memory advantage from a report-only
    observation into a benchmark invariant.
  - The corpus comparator also has a `--require-max-muzen-rss-mib N` absolute
    memory gate. The current-corpus wrapper defaults it to `64`, requiring
    every scored Muzen run in the matrix to stay at or below 64 MiB sampled
    peak RSS. This prevents the relative opencode/Muzen ratio from masking a
    Muzen-side memory regression.
  - The corpus comparator now has scored Muzen quality gates:
    `--require-muzen-best-hit-rate N`,
    `--require-muzen-max-best-false-positives N`, and
    `--require-muzen-max-clean-false-positives N`. The current-corpus wrapper
    defaults them to `1`, `1`, and `0`, so each positive case must have a best
    scored Muzen leg that hits every golden with at most one false positive, and
    each clean case must have a best scored Muzen leg with zero false positives.
    Historical blind/hinted misses can stay visible, but the current best Muzen
    path must satisfy the quality bar.
  - Historical generic-session benchmark experiments were retired with the
    single review path. Current review-quality production results are scored
    through planned review only; reusable loop mechanics are internal runtime
    infrastructure rather than a benchmark-visible runner mode.
  - The corpus comparator now rejects legacy or partial Muzen result files that
    lack a `muzen.review-quality-*` schema, benchmark data, audit coverage, or
    finite scored quality fields. Old direct-runner artifacts therefore cannot
    silently enter the matrix as null-quality/null-memory evidence.
  - `bench/review-quality/run-production-review.mjs` keeps prompt/tool budget
    controls for planned review. Generic loop probes should be added as
    internal runtime tests, not as a second production-review mode.
  - Planned-review synthesis now emits an explicit
    `candidate_finding_decision` trace with `decision: "none"` and
    `reason: "no_candidate_findings"` when synthesis produces zero candidates.
    Future clean or no-candidate missed-defect runs are therefore auditable as
    an explicit synthesis decision rather than an absence of candidate events.
  - Review-quality audit diagnostics now distinguish accepted tool-call
    repairs, rejected repair attempts, and malformed/no-repair-attempt tool
    calls. The rejected-repair corpus gate now means a bounded repair was
    attempted and declined, not merely that a malformed tool call failed.
  - `bench/review-quality/fixtures/full-audit-scenarios.json` is an explicit
    `auditOnly` corpus-matrix fixture generated by
    `bench/review-quality/tools/generate-full-audit-fixture.mjs` from synthetic
    runtime event frames passed through the same trace normalizer as real runs.
    It proves the reporting gate can observe rejected candidates, denied tools,
    transcript compaction, accepted repairs, rejected repair attempts, and
    no-repair-attempt failures without treating the fixture as quality, memory,
    or model-cost evidence. The corpus comparator now preserves audit coverage
    from `auditOnly` runs while excluding them from scored quality, token,
    tool-call, RSS, and RSS-ratio aggregates.
  - `bench/review-quality/tools/smoke-audit-only-comparator.mjs` now creates
    temporary scored clean/missed/published runs plus a freshly generated
    audit-only fixture, runs the real comparator with `--require-scenarios all`,
    and asserts that audit-only runs satisfy audit coverage without contributing
    quality, token, tool-call, RSS, or RSS-ratio metrics. The `all` gate now
    includes the third repair bucket, `noRepairAttempt`, so accepted repairs,
    rejected repair attempts, and unattempted repairs are independently
    auditable.
  - Runtime test coverage now backs several audit scenarios directly:
    `tool_batch_runner_applies_policy_denials_and_preserves_model_order`
    asserts the policy-denied no-repair trace, `processor_emits_host_visible_denied_tool_events`
    asserts host-visible `ToolCallDenied` emission from denied results, and
    `planned_runtime_traces_transcript_compaction` forces a multi-turn planned
    review through prompt-budget compaction and asserts the
    `transcript_compacted` trace.
  - Full `cargo test --lib` passed after the Rust trace slice; `muzen-runner`
    release rebuild passed; JS trace/comparison tooling passes syntax checks;
    synthetic trace extraction/artifact and audit-only comparator smoke tests
    passed.
- Remaining Phase 0 work:
  - expand matched Muzen/opencode corpus cases beyond the first two clean
    negative-control fixtures, duplicate-publication positive fixture,
    missed-defect positive fixture, and rejected-candidate probe fixture to
    include denied-tool and compacted-transcript cases as real benchmark legs
  - use the existing `bench/review-quality` corpus plus anti-cheat controls to
    cover clean, missed-defect, published-finding, rejected-candidate,
    denied-tool, and compacted-transcript cases
  - compare Muzen direct sessions against the corpus; planned review,
    blind/hinted/probe Muzen legs, and opencode exploration-only runs now have
    a shared matrix format
  - replace controlled audit-only probe coverage with real scored corpus legs
    where practical: denied tool, transcript compaction, accepted repair,
    rejected repair, and no-repair-attempt scenarios are all matrix-testable
    and end-to-end runner-testable now, but not proven by real model-backed
    benchmark runs
  - use the trace artifacts to identify whether misses are exploration,
    synthesis, evidence-gating, budget, or tool-ergonomics failures; current
    matched positives point at duplicate candidate publication on `cal-pr-14943`
    and exploration/synthesis weakness on `cal-pr-8330`

## Goal

Improve Muzen's review-agent quality while preserving the properties that make
Muzen materially cheaper than opencode for concurrent review work:

- one local macOS process
- many concurrent review sessions over one materialized snapshot
- read-only exploration by default
- low resident memory
- explicit budgets, evidence, artifacts, and publishability gates

The target is not to embed opencode or recreate its application runtime. The
target is to port the parts that make opencode a stronger exploration agent:
clear exploration strategy, familiar tool ergonomics, subtask-style
investigation, robust tool-call repair, and better evidence synthesis.

## Source-Of-Truth Findings

### Muzen Strengths To Keep

Muzen is already shaped like the desired runtime:

- `Run`/planned-review execution owns concurrency, budgets, cancellation,
  model routing, tools, events, and report aggregation.
- `ReviewSessionSpec` and `SessionScope` isolate role, objective, model
  profile, tool grants, and budget per session.
- The tool registry is review-focused instead of general-purpose.
- Transcript compaction and prompt-budget enforcement are first-class runtime
  policy, not an afterthought.
- The planned-review path already has richer review instructions than the
  original bench objective.
- Direct sessions remain a generic lower-level primitive; planned review should
  be the productized reviewer.

These should remain the center of gravity. The memory win comes from the Rust
runtime, bounded tools, snapshot-scoped reads/searches, and small tool surface.

### Opencode Lessons To Port

Opencode's advantage is not mainly the SDK or server. Its advantage is agent
behavior:

- familiar exploration tools: `read`, `glob`, `grep`
- explicit plan/explore workflow instructions
- easy delegation through task/explore subagents
- detailed tool descriptions that teach when and how to use each tool
- broad autonomous exploration before final answer
- repair-friendly model/tool loop

Muzen should adopt those behaviors without adopting opencode's heavy runtime
graph: server routes, UI/share/revert services, broad registry, MCP/LSP/plugin
layers, and general shell/image/command features.

## Non-Goals

- Do not make opencode a production dependency.
- Do not add a second compatibility path for old weak planned-review behavior.
- Do not add shell, write, image, MCP, LSP, or browser capabilities to the
  default reviewer.
- Do not weaken evidence gates to make the model appear more confident.
- Do not optimize for one-off single-session ergonomics at the expense of
  concurrent session memory.

## Current Gaps

1. **Tool ergonomics have improved, but need broader corpus proof.**
   Provider-visible `read`, `grep`, `glob`, `diff`, `imports`, and `tests`
   aliases now exist and map back to stable internal tool ids. The remaining
   gap is broader scored evidence that aliases improve exploration quality
   without creating scope bypasses or repair ambiguity.

2. **Exploration strategy is measurable, but direct-session misses still need
   sharper causal attribution.**
   Trace artifacts now show model turns, tool exposure, requested/completed
   tools, repairs, resource samples, compaction, structured final turns, and
   candidate decisions. The remaining gap is explaining direct-session
   false-positive and missed-recall causes at the same fidelity as planned
   synthesis.

3. **Subtask exploration exists in planned review, but direct sessions are
   still coarse fanout.**
   Planned review now has lightweight explore workers with trace events.
   Direct-session benchmarks can run concurrent risk-specialized sessions, but
   they do not yet expose the same parent/worker evidence merge model or
   runtime-native candidate decisions.

4. **Tool and playbook descriptions need more evidence-driven iteration.**
   Tool descriptions and risk playbooks are now explicit. The remaining gap is
   empirical: `cal-pr-8330` shows deterministic probes can close recall, but
   direct prompt/playbook pressure still over-publishes adjacent timezone and
   busy-check claims.

5. **Evidence contracts are stricter, but not yet one unified runtime surface.**
   Planned terminal turns and direct-session final turns now request provider
   structured outputs. Direct-session extraction records before/after behavior,
   related evidence, scope broadening, and restrictive-predicate contract
   evidence, but those decisions are still benchmark-layer records rather than
   first-class runtime candidate events.

6. **Failure modes are not reported at the right level.**
   A clean result is different from insufficient exploration. The report should
   make budget exhaustion, missing related evidence, unreadable files, and
   sampled coverage obvious.

7. **Candidate deduplication is improved but still semantic.**
   The duplicate-publication shape on `cal-pr-14943` is fixed in the current
   direct-session evidence. Direct-session dedupe now requires overlapping
   locations plus semantic similarity, which avoids dropping distinct same-block
   issues. The remaining risk is that semantic duplicates and semantically
   adjacent false positives still need stronger adjudication than token
   similarity.

8. **Exploration can happen without producing candidates.**
   On the initial `cal-pr-8330` run, Muzen inspected relevant files and queries,
   including slot tests, slot helpers, and override-related searches, but
   synthesized all review units as clean and produced zero candidates.
   opencode's grep-heavy exploration over the same repo found two of three
   known issues. The follow-up fix proves the failure was primarily
   promote/synthesis thresholding for boundary/date idioms: deterministic
   TimeBoundary probes converted already-visible changed semantics into
   candidate findings without increasing the memory profile.

9. **Event logs are operational, not yet fully explanatory.**
   Muzen already has `RuntimeEvent`, `ReviewEvent`, streamable runner events,
   and JSONL export/load helpers. That is a good base, but it is not enough to
   audit whether the agent's exploration made sense. The trace also needs
   decision-level events: tool exposure, model-visible prompt shape, planned
   exploration obligations, emitted tool calls, repaired calls, denied calls,
   transcript compaction, evidence readiness, candidate finding publication or
   rejection, worker fanout, and budget pressure.

10. **False-positive handling needs semantic adjudication, not only pattern
    suppression.**
    The current one-session direct `cal-pr-8330` run hits 3/3 with zero false
    positives, while the three-session direct run hits 3/3 with four unmatched
    model-generated findings. The remaining extra claims are mostly timezone
    and date-override adjacency claims from the model. These should be manually
    adjudicated as real additional defects or overreach, then encoded as
    regression fixtures instead of suppressed only by wording patterns.

## Phase 0 - Audit Trace Before Quality Changes

Purpose: make every later comparison explainable. We should be able to replay a
run and answer: what did the agent see, what tools could it use, what did it
choose, what evidence did it gather, why did it stop, and why were findings
published or rejected?

Existing baseline:

- Runtime events already cover run/session/model/tool/artifact/finding
  lifecycle.
- Review events already map runtime events into public host-facing records.
- Runtime and review event records already have JSONL export/load helpers.
- The runner protocol already streams `event.runtime` and `event.review`.

Required additions:

1. Add an audit-grade event stream profile for local experiments. [done]
   It should be append-only JSONL with stable sequence numbers, timestamps,
   run/session/turn/tool/finding ids, and redacted payload summaries.
2. Add events for tool exposure:
   - tools exposed this turn
   - provider-visible alias -> internal `ToolId` mapping
   - tools hidden by capability or exposure policy
   - schema/description version digest [partial: exposed tool names/count and
     schema digest are recorded; hidden tools are not yet recorded]
3. Add events for model turn shape:
   - system/objective digest and byte/token estimate
   - transcript item count before call
   - compacted/evicted transcript items
   - exposed tool count
   - output type: text, tool calls, refusal/error
   - tool call ids and names requested by the model [done]
4. Add events for tool-call recovery:
   - malformed call detected
   - repair attempted
   - repair accepted or denied
   - final canonical arguments digest [done for accepted bounded repairs;
     rejected/no-attempt failures preserve original argument summaries]
5. Add events for exploration reasoning state:
   - planned obligations for the unit
   - expected evidence classes
   - evidence classes satisfied
   - missing evidence classes at finish
   - coverage status per assigned file [pending]
6. Add events for candidate findings:
   - candidate created
   - candidate challenged
   - candidate published
   - candidate rejected with structured reason
   - evidence refs attached [partial: candidate decisions and synthesis
     summaries are recorded; no-candidate synthesis now emits an explicit
     `decision=none` trace]
7. Add events for future explore workers:
   - worker planned [done]
   - worker started [done]
   - worker completed [done]
   - worker merged into parent evidence [done]
   - worker skipped due to budget/concurrency [done]
8. Add events for resource pressure:
   - memory sample at run/session intervals
   - prompt budget pressure
   - tool budget pressure
   - model retry/backoff
   - queue wait under session/tool/model semaphores [partial: peak RSS and
     per-turn prompt budget pressure are recorded]
9. Build an `agent-trace` report that groups the raw log by session and turn:
   - turn-by-turn narrative
   - tool-call table
   - evidence coverage table
   - candidate finding decision table
   - unexplored expected evidence
   - memory/token/tool-call timeline [partial: grouped JSON report exists;
     derived corpus audit diagnostics now summarize tool-call, repair,
     candidate, compaction, worker, and resource signals; narrative/table
     rendering still belongs in the corpus report]

Trace rules:

- Do not store raw prompts or raw file contents by default.
- Store redacted summaries, content hashes, byte counts, token estimates, and
  artifact refs.
- Allow an explicit local-only raw trace mode for experiments when the caller
  grants raw artifact/prompt access.
- Event payloads must be schema-versioned and round-trip through JSONL tests.
- Every event must be attributable to a run/session/turn where applicable.

Exit criteria:

- A single local command can run a review and produce:
  - raw audit JSONL [done via `muzen bench --trace-output-dir <dir>`]
  - grouped session/turn report [done]
  - event coverage summary [done]
- A production review-quality run can produce the same audit artifacts from the
  streamable runner events. [done via
  `bench/review-quality/run-production-review.mjs --trace-output-dir <dir>`]
- An opencode profile run can capture streamed server events and resource
  phases into comparable artifacts. [done via
  `profile-opencode-sessions.mjs --event-log-output <path>`]
- The trace can explain at least one clean result, one missed-defect result, one
  published finding, one rejected candidate, one denied tool call, and one
  compacted transcript. [partial: clean, published-finding, missed-defect, and
  rejected-candidate Muzen results are covered by scored runs; the
  published-finding case exposed duplicate publication and the missed-defect
  case exposed exploration/synthesis weakness. The corpus matrix now detects
  scenario coverage automatically. Denied-tool, compacted-transcript,
  accepted-repair, rejected-repair, and no-repair-attempt cases are covered by
  controlled audit-only runner probes, but still require targeted scored
  model-backed corpus runs before they count as quality evidence]

## Phase 1 - Baseline The Quality Gap

Purpose: establish where Muzen loses before changing behavior.

Tasks:

1. Create a small review-quality corpus with real defects and clean controls.
   Include cross-file contract bugs, changed predicate bugs, caller/callee
   return-shape bugs, authorization/scope bugs, test-contract bugs, and
   intentionally clean refactors.
2. Run the same corpus through:
   - Muzen planned review
   - Muzen direct sessions with current review prompts
   - opencode exploration-only reference runs
3. Capture per-run diagnostics:
   - findings found/missed
   - false positives
   - files read
   - searches run
   - related/test/import tools used
   - turns/tool calls consumed
   - prompt/output tokens
   - wall time
   - peak RSS
   - why each candidate was published or rejected [partial: candidate decision
     and synthesis counts/reasons are summarized from audit traces]
4. Add a compact result format under `bench/review-quality/` so comparisons are
   reproducible. [partial: result JSON embeds audit diagnostics,
   `summarize-results.mjs` prints repair/candidate/compaction/worker columns,
   and `compare-corpus-runs.mjs` builds multi-leg corpus matrices]

Exit criteria:

- At least 10 representative fixtures.
- One command can run Muzen against the corpus with deterministic reporting.
- The report uses the audit trace to separate exploration failure from synthesis
  failure. [partial: corpus matrix now labels no-candidate synthesis misses,
  partial-recall/wrong-finding misses, false-positive-only runs, opencode
  replay-backed misses, and audit gaps; richer narrative classification is
  still pending. Required audit scenario coverage can now be gated with
  `--require-scenarios core|all`]

## Phase 2 - Provider-Visible Tool Ergonomics

Purpose: make Muzen's exploration tools easier for models to use without
changing the internal capability model.

Tasks:

1. Add provider-visible aliases for the default review tools:
   - `read` -> bounded file or file-range read [done as `read_file`]
   - `grep` -> `search_text` [done]
   - `glob` -> `list_files` [done]
   - `diff` -> `read_diff` [done]
   - `imports` -> `list_imports` [done]
   - `tests` -> `find_tests_for_file` [done]
2. Keep internal `ToolId` values stable and audit-friendly.
3. Reject alias collisions at registry compilation time. [done]
4. Rewrite exposed descriptions around model behavior: [partial]
   - what question the tool answers
   - when to call it
   - what to call before/after it
   - output limits and evidence expectations
5. A/B test original names versus aliases on the Phase 0 corpus.

Exit criteria:

- Alias mode improves tool selection or finding recall without increasing false
  positives materially.
- Internal events, metrics, and artifacts still use stable Muzen tool identity.
- The audit trace records both provider-visible aliases and internal tool ids.

## Phase 3 - Exploration Playbook And Loop Policy

Purpose: move opencode-style exploration discipline into Muzen's planned-review
path.

Tasks:

1. Convert the system prompt into explicit exploration stages:
   - inspect assigned diff
   - identify changed symbols and invariants
   - search callers/consumers/tests
   - compare base/head where behavior changed
   - synthesize only after evidence is sufficient [partial via explicit
     playbooks and missing-evidence remediation]
2. Add tool-choice heuristics to the prompt and policy:
   - use `grep` before broad reads when searching for call sites [partial]
   - use `imports`/`tests` for contract and behavior changes
   - read base content when the claim depends on before/after semantics
   - do not finish clean if high-risk assigned files have no related evidence
     [done for contract evidence gates]
3. Add explicit risk-class playbooks:
   - `TimeBoundary` [done]
   - `ReturnShape` [done]
   - `CredentialOwnership` [done]
   - `QueryScope` [done]
   - `AuthScope` [done]
   - `RepeatedIntegration` [done]
4. Add explicit insufficiency states:
   - clean
   - issue found
   - sampled
   - insufficient due to budget
   - insufficient due to missing evidence
5. Extend diagnostics so every session reports:
   - evidence coverage
   - missing expected evidence
   - exhausted limits
   - whether the final answer was evidence-ready

Exit criteria:

- Fewer premature clean results on the Phase 0 corpus.
- Reports distinguish "no bug found" from "not enough investigation."

## Phase 4 - Lightweight Explore Workers

Purpose: add the useful part of opencode's task/explore pattern without adding
opencode's heavy session/runtime graph.

Tasks:

1. Add an internal `ExploreWorker` concept for planned review only. [done]
2. Each worker receives:
   - a narrow objective [done]
   - file/path scope [done through child `SessionScope` instructions]
   - read-only exploration aliases [done through shared registry]
   - small turn/tool budget [done: two turns, eight tool calls max]
   - required evidence summary output [done via structured worker schema]
3. Use workers for high-risk units:
   - caller search [partial: playbook-guided]
   - test-contract search [partial: playbook-guided]
   - base/head behavior comparison [partial: prompt-guided]
   - comparable implementation search [partial: playbook-guided]
4. Merge worker outputs as evidence artifacts, not as final findings. [done]
5. Keep worker scheduling under the existing `max_active_sessions` and model
   limiter controls. [done: workers are nested under the active unit session and
   share the model router/tool engine instead of creating a new runtime]

Exit criteria:

- High-risk fixtures show better recall on cross-file bugs.
- Worker fanout does not break memory targets or deterministic report ordering.
- Parent and worker traces can be correlated by ids.

## Phase 5 - Tool-Call Repair And Recovery

Purpose: reduce avoidable failed turns.

Tasks:

1. Add repair for common model mistakes:
   - alias confusion [done for registered provider-visible aliases]
   - invalid path shape [done for JSON string path and `file`/`filepath`/
     `filename` field aliases]
   - line range off by one or reversed [done for camelCase/snake_case fields,
     single `line`, and reversed ranges]
   - malformed JSON arguments [done for JSON strings/objects that can be
     canonicalized without guessing; arbitrary non-JSON remains unrepaired]
   - unknown-but-close tool names [pending automatic repair; trace detection
     exists]
2. Feed repaired calls through the same authorization path. [done for bounded
   pre-policy repairs]
3. Emit diagnostics for repaired versus denied calls. [done for bounded
   accepted repairs plus rejected/no-repair-attempt traces]
4. Add retry guidance when a tool result is truncated or evicted from prompt
   budget.

Exit criteria:

- Lower denied-tool rate on live model runs.
- No authorization bypass from repair logic.

## Phase 6 - Evidence-Centered Synthesis

Purpose: improve final finding quality and reduce speculative issues.

Tasks:

1. Require each candidate finding to name:
   - changed code evidence
   - related contract/caller/test evidence when relevant
   - concrete input/state
   - concrete wrong output/effect
   - before/after behavior
2. Use provider-level structured outputs for JSON-producing review turns rather
   than prompt-only JSON instructions. [partial: terminal unit, contract-pack,
   synthesis, challenge, and direct-session final turns request strict JSON
   schemas from OpenAI; tool-enabled exploration turns intentionally withhold
   the final schema]
3. Preserve rejected candidate findings with rejection reason:
   - no changed-code evidence
   - no related contract evidence
   - intended behavior
   - speculative
   - duplicate
   - contradicted by challenge
4. Keep challenge passes tool-enabled for high-confidence publication.
5. Make final reports show coverage and rejected-candidate summaries.

Exit criteria:

- False positives decline on clean controls.
- Published findings are easier to audit from artifacts alone.

## Phase 7 - Benchmark Gates

Purpose: prevent quality work from eroding Muzen's main advantage.

Required gates:

1. Quality gate:
   - recall on known-defect fixtures
   - false-positive rate on clean fixtures
   - publishability pass rate
2. Efficiency gate:
   - peak RSS at 10/50/100 concurrent sessions
   - wall time
   - model calls
   - tool calls
   - token usage
3. Safety gate:
   - no default write/shell/network capability
   - no scope bypass through aliases or repair
   - deterministic artifact/report ordering
4. Regression gate:
   - existing swarm/concurrency tests remain green
   - planned-review tests cover clean, insufficient, and issue-found outcomes
5. Audit gate:
   - every model turn has exposure, model-shape, and outcome events
   - every tool call has requested, canonicalized, completed/denied events
   - every finding candidate has published/rejected decision events
   - JSONL round-trip fixture coverage exists for every audit event variant

Exit criteria:

- The plan is not considered successful unless Muzen improves review quality
  while staying materially below opencode's memory profile.

## Suggested Implementation Order

1. Phase 0 audit trace.
2. Phase 1 baseline corpus and diagnostics.
3. Phase 2 tool aliases/descriptions behind a single experimental flag.
4. Phase 3 prompt/playbook and insufficiency reporting.
5. Phase 6 evidence-centered synthesis improvements.
6. Phase 4 lightweight explore workers for high-risk units.
7. Phase 5 repair/recovery once aliases and workers expose real failure data.
8. Phase 7 turns the above into permanent benchmark gates.

## First Concrete Slice

The smallest high-signal slice is:

1. Add the local audit trace profile and grouped `agent-trace` report.
2. Build the Phase 1 corpus runner.
3. Add alias-only provider exposure for `read`/`grep`/`glob`/`diff`.
4. A/B test aliases plus richer tool descriptions against the same corpus.
5. Produce a report with quality, token, tool-call, event-coverage, and RSS
   deltas.

This gives an answer to the most important uncertainty: whether opencode's
quality advantage comes mostly from tool ergonomics and prompting, or whether
Muzen needs a deeper explore-worker architecture.
