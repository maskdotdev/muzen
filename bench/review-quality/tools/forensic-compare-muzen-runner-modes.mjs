#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const args = parseArgs(process.argv.slice(2));

if (args.help || !args.shared || !args.process) {
  usage();
  process.exit(args.help ? 0 : 1);
}

const sharedRoot = path.resolve(args.shared);
const processRoot = path.resolve(args.process);
const format = args.format ?? "json";

if (!["json", "markdown"].includes(format)) {
  throw new Error(`unsupported --format ${format}`);
}

const report = compareRoots(sharedRoot, processRoot);
const output =
  format === "markdown" ? renderMarkdown(report) : `${JSON.stringify(report, null, 2)}\n`;

if (args.output) {
  fs.writeFileSync(path.resolve(args.output), output);
} else {
  process.stdout.write(output);
}

function compareRoots(sharedRoot, processRoot) {
  const sharedSummary = readJson(path.join(sharedRoot, "summary.json"));
  const processSummary = readJson(path.join(processRoot, "summary.json"));
  const processByName = new Map(processSummary.results.map((result) => [result.name, result]));
  const cases = [];

  for (const sharedResult of sharedSummary.results) {
    const processResult = processByName.get(sharedResult.name);
    if (!processResult) {
      throw new Error(`missing process result for ${sharedResult.name}`);
    }
    const shared = readCase(sharedRoot, sharedResult.name);
    const process = readCase(processRoot, processResult.name);
    cases.push({
      name: sharedResult.name,
      shared,
      process,
      delta: caseDelta(shared, process),
    });
  }

  cases.sort((left, right) => left.name.localeCompare(right.name));

  return {
    generatedAtUtc: new Date().toISOString(),
    compared: {
      shared: sharedRoot,
      process: processRoot,
    },
    totals: {
      shared: sharedSummary.totals,
      process: processSummary.totals,
      delta: totalsDelta(sharedSummary.totals, processSummary.totals),
    },
    patterns: summarizePatterns(cases, sharedRoot, processRoot),
    missingTraceFields: missingTraceFields(),
    cases,
  };
}

function readCase(root, name) {
  const result = readJson(path.join(root, `${name}.json`));
  const auditPath = path.join(root, "traces", name, "audit-diagnostics.json");
  const audit = fs.existsSync(auditPath)
    ? readJson(auditPath).diagnostics
    : (result.audit?.diagnostics ?? {});
  const events = readJsonl(path.join(root, "traces", name, "review-events.jsonl"));
  const runStart = readOptionalJson(path.join(root, "jobs", name, "run-start.json"));
  const maxToolCalls = runStart?.sessions?.[0]?.budget?.maxToolCalls ?? 50;
  const primarySession = audit.sessions?.find(
    (session) => session.sessionId === "review-orchestrator",
  ) ?? {};
  const eventAnalysis = analyzeEvents(events, "review-orchestrator", maxToolCalls);
  const candidates = candidateSummary(audit, events);

  return {
    review: {
      status: result.review?.status ?? null,
      reviewValid: result.reviewValid ?? null,
      exitCode: result.exitCode ?? null,
      validationFailures: result.validationFailures?.length ?? 0,
      findings: result.review?.findings ?? 0,
      candidates: result.benchmark?.candidateCount ?? 0,
      rejectedCandidates: result.benchmark?.rejectedCandidateCount ?? 0,
      hits: result.benchmark?.hits?.length ?? 0,
      misses: result.benchmark?.misses?.length ?? 0,
      falsePositiveCount: result.benchmark?.falsePositiveCount ?? 0,
      fileReviews: result.review?.fileReviews ?? 0,
      verdicts: result.review?.fileReviewVerdicts ?? {},
      incompleteVerdicts: result.review?.fileReviewVerdicts?.needs_review ?? 0,
      modelCalls: result.review?.modelCalls ?? 0,
      toolCalls: result.review?.toolCalls ?? 0,
      totalTokens: result.review?.tokens?.totalTokens ?? 0,
      elapsedMs: result.review?.elapsedMs ?? null,
    },
    orchestrator: {
      ...pickSession(primarySession),
      maxToolCalls,
      exhaustedMaxToolCalls:
        (primarySession.toolCallsCompleted ?? 0) >= maxToolCalls ||
        (primarySession.toolCallsRequested ?? 0) >= maxToolCalls,
      finalizationCause: eventAnalysis.finalizationCause,
      finalTurn: eventAnalysis.finalTurn,
      toolCallsPerTurn: eventAnalysis.toolCallsPerTurn,
      compactions: eventAnalysis.compactions,
      repairs: eventAnalysis.repairs,
      latency: eventAnalysis.latency,
    },
    sessions: (audit.sessions ?? []).map(pickSession),
    toolCalls: {
      requestedByTool: audit.toolCalls?.requestedByTool ?? {},
      completedByTool: audit.toolCalls?.completedByTool ?? {},
      deniedByTool: audit.toolCalls?.deniedByTool ?? {},
      failureCodes: audit.toolCalls?.failureCodes ?? {},
      repairKinds: audit.toolCalls?.repairKinds ?? {},
    },
    candidates,
  };
}

function analyzeEvents(events, primarySessionId, maxToolCalls) {
  const modelStarts = new Map();
  const toolBatchStarts = new Map();
  const modelLatencies = [];
  const primaryModelLatencies = [];
  const toolLatencies = [];
  const primaryToolLatencies = [];
  const requestedByTurn = new Map();
  const prepared = [];
  const completed = [];
  const compactions = [];
  const schemaRepairs = [];
  const toolRepairs = [];

  for (const record of events) {
    const event = record.event ?? {};
    const eventName = Object.keys(event)[0];
    const sessionId = record.sessionId ?? event[eventName]?.sessionId ?? null;
    const turn = record.turn ?? event[eventName]?.turn ?? event[eventName]?.turnId ?? null;
    const timestampMs = parseTimestampMs(record.timestampUtc);
    const key = `${sessionId ?? ""}\u0000${turn ?? ""}`;

    if (eventName === "modelStarted") {
      modelStarts.set(key, timestampMs);
    } else if (eventName === "modelCompleted") {
      completed.push({
        sessionId,
        turn,
        toolCallCount: event.modelCompleted?.toolCallCount ?? null,
        timestampMs,
      });
      const startedMs = modelStarts.get(key);
      if (startedMs != null && timestampMs != null) {
        const latency = timestampMs - startedMs;
        modelLatencies.push(latency);
        if (sessionId === primarySessionId) primaryModelLatencies.push(latency);
      }
    } else if (eventName === "toolBatchStarted") {
      toolBatchStarts.set(key, timestampMs);
    } else if (eventName === "toolCallCompleted" || eventName === "toolCallDenied") {
      const startedMs = toolBatchStarts.get(key);
      if (startedMs != null && timestampMs != null) {
        const latency = timestampMs - startedMs;
        toolLatencies.push(latency);
        if (sessionId === primarySessionId) primaryToolLatencies.push(latency);
      }
    } else if (eventName === "agentTrace") {
      const trace = event.agentTrace ?? {};
      const traceKind = trace.traceKind;
      const details = trace.details ?? {};
      const traceSessionId = trace.sessionId ?? sessionId;
      const traceTurn = trace.turn ?? trace.turnId ?? turn;

      if (traceKind === "model_turn_prepared") {
        prepared.push({
          sessionId: traceSessionId,
          turn: traceTurn,
          finalTurn: details.finalTurn ?? false,
          toolCallsUsed: details.toolCallsUsed ?? details.builtinToolCallsUsed ?? null,
          maxToolCalls: details.maxToolCalls ?? null,
          maxTurns: details.maxTurns ?? null,
          exposedToolCount: exposedToolCount(trace.summary),
          estimatedPromptTokens: details.estimatedPromptTokens ?? null,
          transcriptItems: details.transcriptItems ?? null,
        });
      } else if (traceKind === "tool_calls_requested") {
        requestedByTurn.set(
          `${traceSessionId ?? ""}\u0000${traceTurn ?? ""}`,
          details.calls?.length ?? 0,
        );
      } else if (traceKind === "transcript_compacted") {
        compactions.push({
          sessionId: traceSessionId,
          turn: traceTurn,
          evictedToolResults: details.evictedToolResults ?? null,
          estimatedPromptTokensAfter: details.estimatedPromptTokensAfter ?? null,
          transcriptItemsAfter: details.transcriptItemsAfter ?? null,
        });
      } else if (traceKind === "schema_repair") {
        schemaRepairs.push({
          sessionId: traceSessionId,
          turn: traceTurn,
          attempt: details.attempt ?? null,
          maxAttempts: details.maxAttempts ?? null,
          sessionKind: details.sessionKind ?? null,
          estimatedPromptTokens: details.estimatedPromptTokens ?? null,
        });
      } else if (traceKind === "tool_call_repair") {
        toolRepairs.push({
          sessionId: traceSessionId,
          turn: traceTurn,
          toolId: details.toolId ?? null,
          errorCode: details.errorCode ?? null,
          repairAttempted: details.repairAttempted ?? null,
          repairAccepted: details.repairAccepted ?? null,
          reason: details.reason ?? null,
        });
      }
    }
  }

  const primaryPrepared = prepared.filter((turn) => turn.sessionId === primarySessionId);
  const primaryCompleted = completed.filter((turn) => turn.sessionId === primarySessionId);
  const lastPrepared = primaryPrepared.at(-1) ?? null;
  const lastCompleted = primaryCompleted.at(-1) ?? null;
  const primaryRequestedTurns = [...requestedByTurn.entries()]
    .filter(([key]) => key.startsWith(`${primarySessionId}\u0000`))
    .map(([, count]) => count);
  const primaryCompactions = compactions.filter(
    (compaction) => compaction.sessionId === primarySessionId,
  );
  const primarySchemaRepairs = schemaRepairs.filter(
    (repair) => repair.sessionId === primarySessionId,
  );
  const primaryToolRepairs = toolRepairs.filter((repair) => repair.sessionId === primarySessionId);

  return {
    finalizationCause: inferFinalizationCause(lastPrepared, lastCompleted, maxToolCalls),
    finalTurn: lastPrepared
      ? {
          turn: lastPrepared.turn,
          finalTurn: lastPrepared.finalTurn,
          modelCompletedToolCallCount: lastCompleted?.toolCallCount ?? null,
          toolCallsUsed: lastPrepared.toolCallsUsed,
          maxToolCalls: lastPrepared.maxToolCalls,
          exposedToolCount: lastPrepared.exposedToolCount,
          estimatedPromptTokens: lastPrepared.estimatedPromptTokens,
          transcriptItems: lastPrepared.transcriptItems,
          modelLatencyMs: lastCompleted
            ? latencyForTurn(primarySessionId, lastPrepared.turn, events)
            : null,
        }
      : null,
    toolCallsPerTurn: {
      requestedTurns: primaryRequestedTurns.length,
      average: round(mean(primaryRequestedTurns), 2),
      max: primaryRequestedTurns.length > 0 ? Math.max(...primaryRequestedTurns) : 0,
      distribution: histogram(primaryRequestedTurns),
    },
    compactions: {
      count: primaryCompactions.length,
      evictedToolResults: sum(primaryCompactions.map((item) => item.evictedToolResults ?? 0)),
      firstTurn: primaryCompactions[0]?.turn ?? null,
      lastTurn: primaryCompactions.at(-1)?.turn ?? null,
      promptTokensAfter: stats(primaryCompactions.map((item) => item.estimatedPromptTokensAfter)),
    },
    repairs: {
      schemaRepairAttempts: schemaRepairs.length,
      primarySchemaRepairAttempts: primarySchemaRepairs.length,
      validationSchemaRepairAttempts: schemaRepairs.length - primarySchemaRepairs.length,
      toolCallRepairs: toolRepairs.length,
      primaryToolCallRepairs: primaryToolRepairs.length,
      toolRepairErrorCodes: countObject(toolRepairs.map((repair) => repair.errorCode)),
    },
    latency: {
      modelMs: stats(modelLatencies),
      primaryModelMs: stats(primaryModelLatencies),
      toolMs: stats(toolLatencies),
      primaryToolMs: stats(primaryToolLatencies),
    },
  };
}

function latencyForTurn(sessionId, turn, events) {
  let startedMs = null;
  for (const record of events) {
    const event = record.event ?? {};
    if (record.sessionId !== sessionId || record.turn !== turn) continue;
    if (event.modelStarted) {
      startedMs = parseTimestampMs(record.timestampUtc);
    } else if (event.modelCompleted && startedMs != null) {
      const completedMs = parseTimestampMs(record.timestampUtc);
      return completedMs == null ? null : completedMs - startedMs;
    }
  }
  return null;
}

function inferFinalizationCause(lastPrepared, lastCompleted, configuredMaxToolCalls) {
  if (!lastPrepared) return "missing_model_turn";
  const maxToolCalls = lastPrepared.maxToolCalls ?? configuredMaxToolCalls;
  if (lastPrepared.finalTurn && (lastPrepared.toolCallsUsed ?? 0) >= maxToolCalls) {
    return "max_tool_calls_final_turn";
  }
  if (lastPrepared.finalTurn) return "runner_marked_final_turn";
  if ((lastCompleted?.toolCallCount ?? null) === 0) return "model_returned_no_tool_calls";
  return "unknown_after_tool_request";
}

function candidateSummary(audit, events) {
  const decisions = [];
  for (const record of events) {
    const trace = record.event?.agentTrace;
    if (trace?.traceKind !== "candidate_finding_decision") continue;
    decisions.push({
      candidateId: trace.details?.candidateId ?? null,
      decision: trace.details?.decision ?? null,
      reason: trace.details?.reason ?? null,
      phase: trace.details?.phase ?? null,
      validatorStatus: trace.details?.validatorStatus ?? null,
      validatorSessionId: trace.details?.validatorSessionId ?? null,
    });
  }
  return {
    decisions: decisions.length,
    accepted: decisions.filter((decision) => decision.decision === "accepted").length,
    rejected: decisions.filter((decision) => decision.decision === "rejected").length,
    reasons: countObject(decisions.map((decision) => decision.reason)),
    synthesis: audit.candidates?.synthesis ?? null,
    detail: decisions,
  };
}

function pickSession(session) {
  return {
    sessionId: session.sessionId ?? null,
    turns: session.turns ?? 0,
    modelTurnsPrepared: session.modelTurnsPrepared ?? 0,
    modelTurnsCompleted: session.modelTurnsCompleted ?? 0,
    toolCallsRequested: session.toolCallsRequested ?? 0,
    toolCallsCompleted: session.toolCallsCompleted ?? 0,
    toolCallsDenied: session.toolCallsDenied ?? 0,
    repairsAccepted: session.repairsAccepted ?? 0,
    repairsRejected: session.repairsRejected ?? 0,
    repairsNotAttempted: session.repairsNotAttempted ?? 0,
    candidateDecisions: session.candidateDecisions ?? 0,
    rejectedCandidates: session.rejectedCandidates ?? 0,
    transcriptCompactions: session.transcriptCompactions ?? 0,
  };
}

function caseDelta(shared, process) {
  return {
    modelCalls: shared.review.modelCalls - process.review.modelCalls,
    toolCalls: shared.review.toolCalls - process.review.toolCalls,
    totalTokens: shared.review.totalTokens - process.review.totalTokens,
    findings: shared.review.findings - process.review.findings,
    candidates: shared.review.candidates - process.review.candidates,
    hits: shared.review.hits - process.review.hits,
    misses: shared.review.misses - process.review.misses,
    falsePositiveCount: shared.review.falsePositiveCount - process.review.falsePositiveCount,
    incompleteVerdicts: shared.review.incompleteVerdicts - process.review.incompleteVerdicts,
    orchestratorToolCallsCompleted:
      shared.orchestrator.toolCallsCompleted - process.orchestrator.toolCallsCompleted,
    orchestratorTurns: shared.orchestrator.turns - process.orchestrator.turns,
    orchestratorTranscriptCompactions:
      shared.orchestrator.transcriptCompactions - process.orchestrator.transcriptCompactions,
    schemaRepairAttempts:
      shared.orchestrator.repairs.schemaRepairAttempts -
      process.orchestrator.repairs.schemaRepairAttempts,
    primaryModelP95Ms:
      nullableDelta(
        shared.orchestrator.latency.primaryModelMs.p95,
        process.orchestrator.latency.primaryModelMs.p95,
      ),
  };
}

function totalsDelta(shared, process) {
  return {
    modelCalls: shared.modelCalls - process.modelCalls,
    toolCalls: shared.toolCalls - process.toolCalls,
    totalTokens: shared.totalTokens - process.totalTokens,
    findings: shared.findings - process.findings,
    hits: shared.hits - process.hits,
    misses: shared.misses - process.misses,
    falsePositives: shared.falsePositives - process.falsePositives,
    reviewElapsedMs: shared.reviewElapsedMs - process.reviewElapsedMs,
  };
}

function summarizePatterns(cases, sharedRoot, processRoot) {
  const sharedExhausted = cases.filter(
    (item) => item.shared.orchestrator.finalizationCause === "max_tool_calls_final_turn",
  );
  const processExhausted = cases.filter(
    (item) => item.process.orchestrator.finalizationCause === "max_tool_calls_final_turn",
  );
  const sharedNoCandidate = cases.filter((item) => item.shared.review.candidates === 0);
  const processPublishedMore = cases.filter(
    (item) => item.process.candidates.accepted > item.shared.candidates.accepted,
  );
  const runnerTimelines = {
    shared: summarizeRunnerTimeline(sharedRoot),
    process: summarizeRunnerTimeline(processRoot),
  };
  const maxToolCallBudgets = [
    ...new Set(
      cases.flatMap((item) => [
        item.shared.orchestrator.maxToolCalls,
        item.process.orchestrator.maxToolCalls,
      ]),
    ),
  ]
    .filter(Number.isFinite)
    .sort((left, right) => left - right);
  const budgetLabel =
    maxToolCallBudgets.length === 1
      ? `${maxToolCallBudgets[0]}-tool`
      : "configured max-tool";

  return {
    maxToolCallBudgets,
    maxToolCallBudgetLabel: budgetLabel,
    sharedMaxToolFinalizations: sharedExhausted.length,
    processMaxToolFinalizations: processExhausted.length,
    sharedMaxToolCases: sharedExhausted.map((item) => item.name),
    processMaxToolCases: processExhausted.map((item) => item.name),
    sharedNoCandidateCases: sharedNoCandidate.map((item) => item.name),
    processAcceptedMoreCandidatesCases: processPublishedMore.map((item) => item.name),
    sharedPrimaryCompactions: sum(
      cases.map((item) => item.shared.orchestrator.transcriptCompactions),
    ),
    processPrimaryCompactions: sum(
      cases.map((item) => item.process.orchestrator.transcriptCompactions),
    ),
    sharedPrimarySchemaRepairs: sum(
      cases.map((item) => item.shared.orchestrator.repairs.primarySchemaRepairAttempts),
    ),
    processPrimarySchemaRepairs: sum(
      cases.map((item) => item.process.orchestrator.repairs.primarySchemaRepairAttempts),
    ),
    primaryModelLatencyP95Ms: {
      shared: stats(cases.map((item) => item.shared.orchestrator.latency.primaryModelMs.p95)).p50,
      process: stats(cases.map((item) => item.process.orchestrator.latency.primaryModelMs.p95))
        .p50,
    },
    runnerTimelines,
    interpretation:
      "Saved traces point first to prompt-budget/compaction and finalization behavior: shared primary orchestrators usually spend the full configured max-tool session budget before a no-tool final turn, while process primary orchestrators usually finalize or emit candidates earlier. The available traces do not show mixed runIds or artifact collisions and do not isolate provider latency from scheduler wait.",
  };
}

function summarizeRunnerTimeline(root) {
  const metricsFile = path.join(root, "metrics.jsonl");
  const samplesFile = path.join(root, "process-samples.jsonl");
  if (!fs.existsSync(metricsFile)) return null;
  const starts = new Map();
  const finishes = [];
  for (const record of readJsonl(metricsFile)) {
    if (record.event === "start") starts.set(record.name, record);
    if (record.event === "finish") finishes.push(record);
  }
  const samples = readJsonl(samplesFile);
  return {
    starts: starts.size,
    finishes: finishes.length,
    maxActive: samples.length ? Math.max(...samples.map((sample) => sample.active ?? 0)) : null,
    maxAggregateRssBytes: samples.length
      ? Math.max(...samples.map((sample) => sample.aggregateRssBytes ?? 0))
      : null,
    finishElapsedMs: stats(finishes.map((finish) => finish.elapsedMs)),
  };
}

function missingTraceFields() {
  return [
    "Explicit session finalization reason enum, e.g. model_no_tool_calls, max_tool_calls, max_turns, schema_repair_exhausted, validation_complete.",
    "Raw final model output metadata: parse success/failure, schema validation errors, and whether each schema repair produced valid JSON.",
    "Provider request lifecycle timing split into queued_at, request_started_at, first_token_at, completed_at, retry_count, and rate_limit/backoff_ms. Current timestamps only support event-to-event latency.",
    "Transcript compaction provenance: evicted toolCallIds/artifactIds/itemIds and retained evidence identifiers, not only evicted counts.",
    "Candidate lifecycle timestamps linking primary candidate emission to validation-session start/end; current candidate decisions are available only after validation completes.",
  ];
}

function renderMarkdown(report) {
  const lines = [];
  lines.push("# Muzen Shared vs Process C5 Forensics");
  lines.push("");
  lines.push(`Shared: \`${report.compared.shared}\``);
  lines.push(`Process: \`${report.compared.process}\``);
  lines.push("");
  lines.push("## Totals");
  lines.push("");
  lines.push(
    "| mode | model calls | tool calls | total tokens | findings | hits | misses | elapsed ms |",
  );
  lines.push("|---|---:|---:|---:|---:|---:|---:|---:|");
  for (const [label, totals] of [
    ["shared", report.totals.shared],
    ["process", report.totals.process],
    ["delta", report.totals.delta],
  ]) {
    lines.push(
      `| ${label} | ${totals.modelCalls} | ${totals.toolCalls} | ${totals.totalTokens} | ${totals.findings} | ${totals.hits} | ${totals.misses} | ${totals.reviewElapsedMs ?? ""} |`,
    );
  }
  lines.push("");
  lines.push("## Patterns");
  lines.push("");
  lines.push(
    `- Shared primary orchestrators finalized on the ${report.patterns.maxToolCallBudgetLabel} final turn in ${report.patterns.sharedMaxToolFinalizations}/${report.cases.length} cases; process did so in ${report.patterns.processMaxToolFinalizations}/${report.cases.length}.`,
  );
  lines.push(
    `- Primary transcript compactions: shared ${report.patterns.sharedPrimaryCompactions}, process ${report.patterns.processPrimaryCompactions}.`,
  );
  lines.push(
    `- Cases where process accepted more candidates: ${formatList(report.patterns.processAcceptedMoreCandidatesCases)}.`,
  );
  lines.push(`- Interpretation: ${report.patterns.interpretation}`);
  lines.push("");
  lines.push("## Cases");
  lines.push("");
  lines.push(
    "| case | shared finalization | process finalization | shared turns/tools/compactions | process turns/tools/compactions | candidates shared/process | findings shared/process | needs_review shared/process | p95 model ms shared/process |",
  );
  lines.push("|---|---|---|---:|---:|---:|---:|---:|---:|");
  for (const item of report.cases) {
    lines.push(
      `| ${item.name} | ${item.shared.orchestrator.finalizationCause} | ${item.process.orchestrator.finalizationCause} | ${item.shared.orchestrator.turns}/${item.shared.orchestrator.toolCallsCompleted}/${item.shared.orchestrator.transcriptCompactions} | ${item.process.orchestrator.turns}/${item.process.orchestrator.toolCallsCompleted}/${item.process.orchestrator.transcriptCompactions} | ${item.shared.review.candidates}/${item.process.review.candidates} | ${item.shared.review.findings}/${item.process.review.findings} | ${item.shared.review.incompleteVerdicts}/${item.process.review.incompleteVerdicts} | ${item.shared.orchestrator.latency.primaryModelMs.p95 ?? ""}/${item.process.orchestrator.latency.primaryModelMs.p95 ?? ""} |`,
    );
  }
  lines.push("");
  lines.push("## Missing Trace Fields");
  lines.push("");
  for (const field of report.missingTraceFields) lines.push(`- ${field}`);
  return `${lines.join("\n")}\n`;
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function readOptionalJson(file) {
  return fs.existsSync(file) ? readJson(file) : null;
}

function readJsonl(file) {
  if (!fs.existsSync(file)) return [];
  return fs
    .readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

function parseTimestampMs(timestamp) {
  if (!timestamp) return null;
  const numeric = /^(\d+(?:\.\d+)?)Z$/.exec(timestamp);
  if (numeric) return Number(numeric[1]) * 1000;
  const parsed = Date.parse(timestamp);
  return Number.isNaN(parsed) ? null : parsed;
}

function exposedToolCount(summary) {
  const match = /and (\d+) exposed tool\(s\)/.exec(summary ?? "");
  return match ? Number(match[1]) : null;
}

function stats(values) {
  const clean = values.filter((value) => Number.isFinite(value)).sort((left, right) => left - right);
  if (clean.length === 0) {
    return { count: 0, min: null, p50: null, p95: null, max: null, mean: null };
  }
  return {
    count: clean.length,
    min: round(clean[0], 2),
    p50: round(percentile(clean, 0.5), 2),
    p95: round(percentile(clean, 0.95), 2),
    max: round(clean.at(-1), 2),
    mean: round(mean(clean), 2),
  };
}

function percentile(sortedValues, percentileValue) {
  const index = Math.min(
    sortedValues.length - 1,
    Math.max(0, Math.ceil(sortedValues.length * percentileValue) - 1),
  );
  return sortedValues[index];
}

function mean(values) {
  const clean = values.filter((value) => Number.isFinite(value));
  return clean.length === 0 ? null : sum(clean) / clean.length;
}

function sum(values) {
  return values.reduce((total, value) => total + (Number.isFinite(value) ? value : 0), 0);
}

function histogram(values) {
  return Object.fromEntries(
    [...countMap(values).entries()].sort(([left], [right]) => Number(left) - Number(right)),
  );
}

function countObject(values) {
  return Object.fromEntries(countMap(values));
}

function countMap(values) {
  const counts = new Map();
  for (const value of values) {
    if (value == null) continue;
    counts.set(value, (counts.get(value) ?? 0) + 1);
  }
  return counts;
}

function nullableDelta(left, right) {
  return Number.isFinite(left) && Number.isFinite(right) ? round(left - right, 2) : null;
}

function round(value, digits) {
  return Number.isFinite(value) ? Number(value.toFixed(digits)) : value;
}

function formatList(values) {
  return values.length === 0 ? "none" : values.map((value) => `\`${value}\``).join(", ");
}

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
    } else if (["--shared", "--process", "--format", "--output"].includes(arg)) {
      parsed[arg.slice(2)] = argv[++index];
    } else {
      throw new Error(`unknown argument ${arg}`);
    }
  }
  return parsed;
}

function usage() {
  process.stderr.write(
    "Usage: forensic-compare-muzen-runner-modes.mjs --shared <dir> --process <dir> [--format json|markdown] [--output <path>]\n",
  );
}
