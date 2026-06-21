#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import {
  compactOptionalInstrumentation,
  optionalInstrumentationSummary,
  primarySession,
  readJson,
  readJsonl,
  readTraceBundle,
  runMaxToolCalls,
  parseTimestampMs,
} from "./runner-mode-diagnostics.mjs";

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
  const sharedTotals = effectiveTotals(sharedSummary.totals, cases.map((item) => item.shared.review));
  const processTotals = effectiveTotals(
    processSummary.totals,
    cases.map((item) => item.process.review),
  );

  return {
    generatedAtUtc: new Date().toISOString(),
    compared: {
      shared: sharedRoot,
      process: processRoot,
    },
    totals: {
      shared: sharedTotals,
      process: processTotals,
      delta: totalsDelta(sharedTotals, processTotals),
    },
    patterns: summarizePatterns(cases, sharedRoot, processRoot),
    missingTraceFields: missingTraceFields(),
    cases,
  };
}

function readCase(root, name) {
  const result = readJson(path.join(root, `${name}.json`));
  const traceBundle = readTraceBundle(root, name, result);
  const audit = traceBundle.audit;
  const events = traceBundle.events;
  const maxToolCalls = runMaxToolCalls(traceBundle.runStart);
  const primary = primarySession(audit);
  const eventAnalysis = analyzeEvents(events, "review-orchestrator", maxToolCalls);
  const completionDiagnostics = result.review?.completionDiagnostics ?? [];
  const primaryCompletionDiagnostic =
    completionDiagnostics.find((diagnostic) => diagnostic.sessionId === "review-orchestrator") ??
    eventAnalysis.finalizationDiagnostic;
  const instrumentation = optionalInstrumentationSummary({
    audit,
    events,
  });
  const compactInstrumentation = compactOptionalInstrumentation(instrumentation);
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
      modelMetrics: result.review?.modelMetrics ?? {},
      modelProviderRequestMs:
        result.review?.modelMetrics?.providerRequestMs ??
        compactInstrumentation.providerRequestMs ??
        0,
      modelRetryBackoffMs:
        result.review?.modelMetrics?.retryBackoffMs ?? compactInstrumentation.backoffMs ?? 0,
      modelLimiterWaitMs: result.review?.modelMetrics?.limiterWaitMs ?? 0,
      modelTimeoutErrors: result.review?.modelMetrics?.timeoutErrors ?? 0,
      modelRetryableProviderErrors:
        result.review?.modelMetrics?.retryableProviderErrors ?? 0,
      modelNonRetryableProviderErrors:
        result.review?.modelMetrics?.nonRetryableProviderErrors ?? 0,
      elapsedMs: result.review?.elapsedMs ?? null,
    },
    orchestrator: {
      ...pickSession(primary),
      maxToolCalls,
      exhaustedMaxToolCalls:
        (primary.toolCallsCompleted ?? 0) >= maxToolCalls ||
        (primary.toolCallsRequested ?? 0) >= maxToolCalls,
      finalizationCause:
        primaryCompletionDiagnostic?.finalizationReason ??
        instrumentation.finalization.explicitReason ??
        eventAnalysis.finalizationCause,
      finalOutput: primaryCompletionDiagnostic?.finalOutput ?? null,
      explicitFinalizationReason:
        instrumentation.finalization.explicitReason ??
        primaryCompletionDiagnostic?.finalizationReason ??
        null,
      explicitFinalizationSource: instrumentation.finalization.source,
      finalTurn: eventAnalysis.finalTurn,
      toolCallsPerTurn: eventAnalysis.toolCallsPerTurn,
      compactions: eventAnalysis.compactions,
      compactionDetails: eventAnalysis.compactionDetails,
      repairs: eventAnalysis.repairs,
      latency: eventAnalysis.latency,
      instrumentation,
    },
    sessions: (audit.sessions ?? []).map((session) =>
      pickSession(
        session,
        completionDiagnostics.find((diagnostic) => diagnostic.sessionId === session.sessionId),
      ),
    ),
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
  const finalizations = [];

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
          finalTurnReason: details.finalTurnReason ?? null,
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
        const nextTurn = details.nextModelTurnId ?? traceTurn;
        compactions.push({
          sessionId: traceSessionId,
          turn: traceTurn,
          nextModelTurnId: nextTurn,
          evictedToolResults: details.evictedToolResults ?? null,
          evictedItemCounts: details.evictedItemCounts ?? {},
          estimatedPromptTokensBefore: details.estimatedPromptTokensBefore ?? null,
          estimatedPromptTokensAfter: details.estimatedPromptTokensAfter ?? null,
          transcriptItemsBefore: details.transcriptItemsBefore ?? null,
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
      } else if (traceKind === "session_finalization") {
        finalizations.push({
          sessionId: traceSessionId,
          status: details.status ?? null,
          completed: details.completed ?? null,
          finalizationReason: details.finalizationReason ?? null,
          finalOutput: details.finalOutput ?? null,
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
  for (const compaction of compactions) {
    compaction.nextTurnRequestedToolCalls =
      requestedByTurn.get(`${compaction.sessionId ?? ""}\u0000${compaction.nextModelTurnId ?? ""}`) ??
      null;
    compaction.nextTurnRequestedAnotherTool =
      compaction.nextTurnRequestedToolCalls == null
        ? null
        : compaction.nextTurnRequestedToolCalls > 0;
  }
  const primarySchemaRepairs = schemaRepairs.filter(
    (repair) => repair.sessionId === primarySessionId,
  );
  const primaryToolRepairs = toolRepairs.filter((repair) => repair.sessionId === primarySessionId);
  const finalizationDiagnostic =
    finalizations.find((finalization) => finalization.sessionId === primarySessionId) ?? null;

  return {
    finalizationCause:
      finalizationDiagnostic?.finalizationReason ??
      inferFinalizationCause(lastPrepared, lastCompleted, maxToolCalls),
    finalizationDiagnostic,
    finalTurn: lastPrepared
      ? {
          turn: lastPrepared.turn,
          finalTurn: lastPrepared.finalTurn,
          finalTurnReason: lastPrepared.finalTurnReason,
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
      evictedItemCounts: sumEvictedItemCounts(primaryCompactions),
      nextTurnRequestedAnotherTool: countObject(
        primaryCompactions.map((item) =>
          item.nextTurnRequestedAnotherTool == null
            ? null
            : String(item.nextTurnRequestedAnotherTool),
        ),
      ),
      firstTurn: primaryCompactions[0]?.turn ?? null,
      lastTurn: primaryCompactions.at(-1)?.turn ?? null,
      promptTokensBefore: stats(
        primaryCompactions.map((item) => item.estimatedPromptTokensBefore),
      ),
      promptTokensAfter: stats(primaryCompactions.map((item) => item.estimatedPromptTokensAfter)),
    },
    compactionDetails: primaryCompactions,
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
  if (lastPrepared.finalTurnReason) return lastPrepared.finalTurnReason;
  const maxToolCalls = lastPrepared.maxToolCalls ?? configuredMaxToolCalls;
  if (lastPrepared.finalTurn && (lastPrepared.toolCallsUsed ?? 0) >= maxToolCalls) {
    return "max_tool_calls_final_turn";
  }
  if (lastPrepared.finalTurn) return "runner_marked_final_turn";
  if ((lastCompleted?.toolCallCount ?? null) === 0) return "model_returned_no_tool_calls";
  return "unknown_after_tool_request";
}

function candidateSummary(audit, events) {
  const emitted = [];
  const validations = [];
  const decisions = [];
  const skipped = [];
  const seenLifecycleEvents = new Set();
  for (const record of events) {
    const trace = record.event?.agentTrace;
    if (!trace) continue;
    if (!trace.traceKind?.startsWith("candidate_")) continue;
    const lifecycleKey = JSON.stringify({
      traceKind: trace.traceKind,
      details: trace.details ?? {},
    });
    if (seenLifecycleEvents.has(lifecycleKey)) continue;
    seenLifecycleEvents.add(lifecycleKey);
    if (trace.traceKind === "candidate_finding_emitted") {
      emitted.push({
        candidateId: trace.details?.candidateId ?? null,
        index: trace.details?.index ?? null,
        path: trace.details?.path ?? null,
        evidenceArtifacts: Array.isArray(trace.details?.evidenceArtifactIds)
          ? trace.details.evidenceArtifactIds.length
          : 0,
        orchestratorStatus: trace.details?.orchestratorStatus ?? null,
        orchestratorExhaustedToolBudget:
          trace.details?.orchestratorExhaustedToolBudget ?? null,
      });
    } else if (
      trace.traceKind === "candidate_validation_started" ||
      trace.traceKind === "candidate_validation_completed"
    ) {
      validations.push({
        candidateId: trace.details?.candidateId ?? null,
        phase: trace.traceKind.replace("candidate_validation_", ""),
        status: trace.details?.status ?? null,
        validatorSessionId: trace.details?.validatorSessionId ?? null,
        artifactId: trace.details?.artifactId ?? null,
      });
    } else if (trace.traceKind === "candidate_finding_decision") {
      decisions.push({
        candidateId: trace.details?.candidateId ?? null,
        decision: trace.details?.decision ?? null,
        reason: trace.details?.reason ?? null,
        phase: trace.details?.phase ?? null,
        validatorStatus: trace.details?.validatorStatus ?? null,
        validatorSessionId: trace.details?.validatorSessionId ?? null,
        publicationSkippedBudgetExhausted:
          trace.details?.publicationSkippedBudgetExhausted ?? false,
      });
    } else if (trace.traceKind === "candidate_publication_skipped") {
      skipped.push({
        reason: trace.details?.reason ?? null,
        orchestratorStatus: trace.details?.orchestratorStatus ?? null,
        publicationSkippedBudgetExhausted:
          trace.details?.publicationSkippedBudgetExhausted ?? false,
      });
    }
  }
  return {
    emitted: emitted.length,
    validationsStarted: validations.filter((validation) => validation.phase === "started").length,
    validationsCompleted: validations.filter((validation) => validation.phase === "completed")
      .length,
    decisions: decisions.length,
    accepted: decisions.filter((decision) => decision.decision === "accepted").length,
    rejected: decisions.filter((decision) => decision.decision === "rejected").length,
    reasons: countObject(decisions.map((decision) => decision.reason)),
    rejectionReasons: countObject(
      decisions
        .filter((decision) => decision.decision === "rejected")
        .map((decision) => decision.reason),
    ),
    validatorStatuses: countObject(decisions.map((decision) => decision.validatorStatus)),
    publicationSkippedBudgetExhausted: decisions.filter(
      (decision) => decision.publicationSkippedBudgetExhausted,
    ).length + skipped.filter((item) => item.publicationSkippedBudgetExhausted).length,
    publicationSkipped: skipped.length,
    synthesis: audit.candidates?.synthesis ?? null,
    emittedDetail: emitted,
    validationDetail: validations,
    skippedDetail: skipped,
    detail: decisions,
  };
}

function pickSession(session, completionDiagnostic = null) {
  return {
    sessionId: session.sessionId ?? null,
    finalizationReason: completionDiagnostic?.finalizationReason ?? null,
    finalOutput: completionDiagnostic?.finalOutput ?? null,
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
    modelProviderRequestMs:
      shared.review.modelProviderRequestMs - process.review.modelProviderRequestMs,
    modelRetryBackoffMs:
      shared.review.modelRetryBackoffMs - process.review.modelRetryBackoffMs,
    modelLimiterWaitMs:
      shared.review.modelLimiterWaitMs - process.review.modelLimiterWaitMs,
    modelTimeoutErrors:
      shared.review.modelTimeoutErrors - process.review.modelTimeoutErrors,
    modelRetryableProviderErrors:
      shared.review.modelRetryableProviderErrors -
      process.review.modelRetryableProviderErrors,
    modelNonRetryableProviderErrors:
      shared.review.modelNonRetryableProviderErrors -
      process.review.modelNonRetryableProviderErrors,
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

function effectiveTotals(summaryTotals, cases) {
  const totals = { ...summaryTotals };
  for (const field of [
    "modelProviderRequestMs",
    "modelRetryBackoffMs",
    "modelLimiterWaitMs",
    "modelTimeoutErrors",
    "modelRetryableProviderErrors",
    "modelNonRetryableProviderErrors",
  ]) {
    const caseTotal = cases.reduce((sum, item) => sum + (item[field] ?? 0), 0);
    if (caseTotal > 0 && !totals[field]) {
      totals[field] = caseTotal;
    }
  }
  return totals;
}

function totalsDelta(shared, process) {
  return {
    modelCalls: shared.modelCalls - process.modelCalls,
    toolCalls: shared.toolCalls - process.toolCalls,
    totalTokens: shared.totalTokens - process.totalTokens,
    modelProviderRequestMs:
      (shared.modelProviderRequestMs ?? 0) - (process.modelProviderRequestMs ?? 0),
    modelRetryBackoffMs:
      (shared.modelRetryBackoffMs ?? 0) - (process.modelRetryBackoffMs ?? 0),
    modelLimiterWaitMs:
      (shared.modelLimiterWaitMs ?? 0) - (process.modelLimiterWaitMs ?? 0),
    modelTimeoutErrors:
      (shared.modelTimeoutErrors ?? 0) - (process.modelTimeoutErrors ?? 0),
    modelRetryableProviderErrors:
      (shared.modelRetryableProviderErrors ?? 0) -
      (process.modelRetryableProviderErrors ?? 0),
    modelNonRetryableProviderErrors:
      (shared.modelNonRetryableProviderErrors ?? 0) -
      (process.modelNonRetryableProviderErrors ?? 0),
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
  const sharedPrimaryCompactions = sum(
    cases.map((item) => item.shared.orchestrator.transcriptCompactions),
  );
  const processPrimaryCompactions = sum(
    cases.map((item) => item.process.orchestrator.transcriptCompactions),
  );

  return {
    maxToolCallBudgets,
    maxToolCallBudgetLabel: budgetLabel,
    sharedMaxToolFinalizations: sharedExhausted.length,
    processMaxToolFinalizations: processExhausted.length,
    sharedMaxToolCases: sharedExhausted.map((item) => item.name),
    processMaxToolCases: processExhausted.map((item) => item.name),
    sharedNoCandidateCases: sharedNoCandidate.map((item) => item.name),
    processAcceptedMoreCandidatesCases: processPublishedMore.map((item) => item.name),
    sharedPrimaryCompactions,
    processPrimaryCompactions,
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
    instrumentationAvailability: {
      shared: availabilityCounts(cases.map((item) => item.shared.orchestrator.instrumentation)),
      process: availabilityCounts(cases.map((item) => item.process.orchestrator.instrumentation)),
    },
    runnerTimelines,
    interpretation: summarizeInterpretation({
      sharedMaxToolFinalizations: sharedExhausted.length,
      processMaxToolFinalizations: processExhausted.length,
      sharedPrimaryCompactions,
      processPrimaryCompactions,
      sharedAcceptedCandidates: sum(cases.map((item) => item.shared.candidates.accepted)),
      processAcceptedCandidates: sum(cases.map((item) => item.process.candidates.accepted)),
      sharedFindings: sum(cases.map((item) => item.shared.review.findings)),
      processFindings: sum(cases.map((item) => item.process.review.findings)),
    }),
  };
}

function summarizeInterpretation(patterns) {
  const observations = [];
  if (patterns.sharedMaxToolFinalizations || patterns.processMaxToolFinalizations) {
    observations.push(
      `max-tool finalization appeared in shared/process ${patterns.sharedMaxToolFinalizations}/${patterns.processMaxToolFinalizations} cases`,
    );
  } else {
    observations.push("neither mode exhausted the configured max-tool final turn");
  }
  observations.push(
    `primary transcript compactions were shared/process ${patterns.sharedPrimaryCompactions}/${patterns.processPrimaryCompactions}`,
  );
  observations.push(
    `accepted candidate counts were shared/process ${patterns.sharedAcceptedCandidates}/${patterns.processAcceptedCandidates}`,
  );
  observations.push(
    `published findings were shared/process ${patterns.sharedFindings}/${patterns.processFindings}`,
  );
  observations.push(
    "the available traces do not show mixed runIds or artifact collisions; model-turn traces include queued/completed timing but not first-token stream timing",
  );
  return `Saved traces point to runner-mode differences in exploration depth, compaction, and candidate publication: ${observations.join("; ")}.`;
}

function availabilityCounts(instrumentation) {
  const keys = [
    "explicitFinalizationReason",
    "providerTimingSplit",
    "providerRetryOrBackoff",
    "transcriptCompactionIds",
    "candidateLifecycleTimestamps",
  ];
  return Object.fromEntries(
    keys.map((key) => [
      key,
      instrumentation.filter((item) => item.availability?.[key]).length,
    ]),
  );
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
    "First-token stream milestones. Model-turn diagnostics include queued/completed timing plus aggregate provider duration, limiter wait, retry count, retry backoff, and failure classes.",
    "Schema validation error details and per-repair output metadata. Current final output diagnostics report parse/schema booleans, repair count, and accepted/rejected status.",
    "Transcript compaction provenance: exact evicted toolCallIds/artifactIds/itemIds and retained evidence identifiers. Current instrumentation records counts by kind and turn linkage.",
    "Explicit candidate validation durations. Candidate lifecycle traces now carry event timestamps and emitted candidate title/claim/behavior snapshots, but do not compute duration fields.",
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
    "| mode | model calls | tool calls | total tokens | provider ms | backoff ms | limiter ms | findings | hits | misses | elapsed ms |",
  );
  lines.push("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
  for (const [label, totals] of [
    ["shared", report.totals.shared],
    ["process", report.totals.process],
    ["delta", report.totals.delta],
  ]) {
    lines.push(
      `| ${label} | ${totals.modelCalls} | ${totals.toolCalls} | ${totals.totalTokens} | ${totals.modelProviderRequestMs ?? 0} | ${totals.modelRetryBackoffMs ?? 0} | ${totals.modelLimiterWaitMs ?? 0} | ${totals.findings} | ${totals.hits} | ${totals.misses} | ${totals.reviewElapsedMs ?? ""} |`,
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
    `- Candidate lifecycle: shared emitted/validated/accepted ${sum(report.cases.map((item) => item.shared.candidates.emitted))}/${sum(report.cases.map((item) => item.shared.candidates.validationsCompleted))}/${sum(report.cases.map((item) => item.shared.candidates.accepted))}; process ${sum(report.cases.map((item) => item.process.candidates.emitted))}/${sum(report.cases.map((item) => item.process.candidates.validationsCompleted))}/${sum(report.cases.map((item) => item.process.candidates.accepted))}.`,
  );
  const sharedRejectionReasons = formatCountObject(
    mergeCountObjects(report.cases.map((item) => item.shared.candidates.rejectionReasons)),
  );
  const processRejectionReasons = formatCountObject(
    mergeCountObjects(report.cases.map((item) => item.process.candidates.rejectionReasons)),
  );
  lines.push(
    `- Candidate rejections: shared ${sum(report.cases.map((item) => item.shared.candidates.rejected))} (${sharedRejectionReasons}); process ${sum(report.cases.map((item) => item.process.candidates.rejected))} (${processRejectionReasons}).`,
  );
  lines.push(
    `- Cases where process accepted more candidates: ${formatList(report.patterns.processAcceptedMoreCandidatesCases)}.`,
  );
  lines.push(
    `- Optional instrumentation availability: explicit finalization shared/process ${report.patterns.instrumentationAvailability.shared.explicitFinalizationReason}/${report.patterns.instrumentationAvailability.process.explicitFinalizationReason}; provider timing split ${report.patterns.instrumentationAvailability.shared.providerTimingSplit}/${report.patterns.instrumentationAvailability.process.providerTimingSplit}; compaction IDs ${report.patterns.instrumentationAvailability.shared.transcriptCompactionIds}/${report.patterns.instrumentationAvailability.process.transcriptCompactionIds}.`,
  );
  lines.push(`- Interpretation: ${report.patterns.interpretation}`);
  lines.push("");
  lines.push("## Cases");
  lines.push("");
  lines.push(
    "| case | shared finalization | process finalization | shared turns/tools/compactions | process turns/tools/compactions | candidates shared/process | accepted/rejected shared | accepted/rejected process | rejection reasons shared/process | findings shared/process | needs_review shared/process | p95 model ms shared/process |",
  );
  lines.push("|---|---|---|---:|---:|---:|---:|---:|---|---:|---:|---:|");
  for (const item of report.cases) {
    lines.push(
      `| ${item.name} | ${item.shared.orchestrator.finalizationCause} | ${item.process.orchestrator.finalizationCause} | ${item.shared.orchestrator.turns}/${item.shared.orchestrator.toolCallsCompleted}/${item.shared.orchestrator.transcriptCompactions} | ${item.process.orchestrator.turns}/${item.process.orchestrator.toolCallsCompleted}/${item.process.orchestrator.transcriptCompactions} | ${item.shared.review.candidates}/${item.process.review.candidates} | ${item.shared.candidates.accepted}/${item.shared.candidates.rejected} | ${item.process.candidates.accepted}/${item.process.candidates.rejected} | ${formatCountObject(item.shared.candidates.rejectionReasons)}/${formatCountObject(item.process.candidates.rejectionReasons)} | ${item.shared.review.findings}/${item.process.review.findings} | ${item.shared.review.incompleteVerdicts}/${item.process.review.incompleteVerdicts} | ${item.shared.orchestrator.latency.primaryModelMs.p95 ?? ""}/${item.process.orchestrator.latency.primaryModelMs.p95 ?? ""} |`,
    );
  }
  lines.push("");
  lines.push("## Missing Trace Fields");
  lines.push("");
  for (const field of report.missingTraceFields) lines.push(`- ${field}`);
  return `${lines.join("\n")}\n`;
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

function sumEvictedItemCounts(compactions) {
  const totals = {};
  for (const compaction of compactions) {
    for (const [kind, count] of Object.entries(compaction.evictedItemCounts ?? {})) {
      totals[kind] = (totals[kind] ?? 0) + (Number.isFinite(count) ? count : 0);
    }
  }
  return totals;
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

function mergeCountObjects(objects) {
  const merged = {};
  for (const object of objects) {
    for (const [key, count] of Object.entries(object ?? {})) {
      merged[key] = (merged[key] ?? 0) + count;
    }
  }
  return merged;
}

function formatCountObject(object) {
  const entries = Object.entries(object ?? {}).filter(([, count]) => count > 0);
  if (entries.length === 0) return "none";
  return entries
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([key, count]) => `${key}:${count}`)
    .join(", ");
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
