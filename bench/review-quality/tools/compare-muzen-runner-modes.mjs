#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import {
  compactOptionalInstrumentation,
  optionalInstrumentationSummary,
  primarySession,
  readJson,
  readTraceBundle,
  runMaxToolCalls,
} from "./runner-mode-diagnostics.mjs";

const args = parseArgs(process.argv.slice(2));

if (args.help || !args.shared || !args.process) {
  usage();
  process.exit(args.help ? 0 : 1);
}

const sharedDir = path.resolve(args.shared);
const processDir = path.resolve(args.process);
const report = compareRunnerModes(sharedDir, processDir);
const serialized = `${JSON.stringify(report, null, 2)}\n`;

if (args.output) {
  fs.writeFileSync(path.resolve(args.output), serialized);
} else {
  process.stdout.write(serialized);
}

function compareRunnerModes(sharedRoot, processRoot) {
  const sharedSummary = readJson(path.join(sharedRoot, "summary.json"));
  const processSummary = readJson(path.join(processRoot, "summary.json"));
  const processByName = new Map(processSummary.results.map((result) => [result.name, result]));
  const cases = [];

  for (const sharedResult of sharedSummary.results) {
    const processResult = processByName.get(sharedResult.name);
    if (!processResult) {
      throw new Error(`missing process result for ${sharedResult.name}`);
    }
    const sharedReport = readJson(path.join(sharedRoot, `${sharedResult.name}.json`));
    const processReport = readJson(path.join(processRoot, `${sharedResult.name}.json`));
    const sharedCase = compactCase(sharedRoot, sharedResult.name, sharedReport);
    const processCase = compactCase(processRoot, sharedResult.name, processReport);
    cases.push({
      name: sharedResult.name,
      shared: sharedCase,
      process: processCase,
      delta: {
        modelCalls: sharedCase.modelCalls - processCase.modelCalls,
        toolCalls: sharedCase.toolCalls - processCase.toolCalls,
        totalTokens: sharedCase.totalTokens - processCase.totalTokens,
        modelProviderRequestMs:
          sharedCase.modelProviderRequestMs - processCase.modelProviderRequestMs,
        modelRetryBackoffMs:
          sharedCase.modelRetryBackoffMs - processCase.modelRetryBackoffMs,
        modelLimiterWaitMs:
          sharedCase.modelLimiterWaitMs - processCase.modelLimiterWaitMs,
        maxModelLimiterWaitMs:
          sharedCase.maxModelLimiterWaitMs - processCase.maxModelLimiterWaitMs,
        modelTimeoutErrors:
          sharedCase.modelTimeoutErrors - processCase.modelTimeoutErrors,
        modelRetryableProviderErrors:
          sharedCase.modelRetryableProviderErrors -
          processCase.modelRetryableProviderErrors,
        modelNonRetryableProviderErrors:
          sharedCase.modelNonRetryableProviderErrors -
          processCase.modelNonRetryableProviderErrors,
        findings: sharedCase.findings - processCase.findings,
        candidates: sharedCase.candidates - processCase.candidates,
        hits: sharedCase.hits - processCase.hits,
        misses: sharedCase.misses - processCase.misses,
        falsePositiveCount: sharedCase.falsePositiveCount - processCase.falsePositiveCount,
        orchestratorToolCallsCompleted:
          sharedCase.orchestrator.toolCallsCompleted -
          processCase.orchestrator.toolCallsCompleted,
        orchestratorTranscriptCompactions:
          sharedCase.orchestrator.transcriptCompactions -
          processCase.orchestrator.transcriptCompactions,
      },
    });
  }

  cases.sort((left, right) => left.name.localeCompare(right.name));
  const sharedTotals = effectiveTotals(sharedSummary.totals, cases.map((item) => item.shared));
  const processTotals = effectiveTotals(processSummary.totals, cases.map((item) => item.process));
  return {
    generatedAtUtc: new Date().toISOString(),
    compared: {
      shared: sharedRoot,
      process: processRoot,
    },
    totals: {
      shared: sharedTotals,
      process: processTotals,
      delta: {
        modelCalls: sharedTotals.modelCalls - processTotals.modelCalls,
        toolCalls: sharedTotals.toolCalls - processTotals.toolCalls,
        totalTokens: sharedTotals.totalTokens - processTotals.totalTokens,
        modelProviderRequestMs: sharedTotals.modelProviderRequestMs - processTotals.modelProviderRequestMs,
        modelRetryBackoffMs: sharedTotals.modelRetryBackoffMs - processTotals.modelRetryBackoffMs,
        modelLimiterWaitMs: sharedTotals.modelLimiterWaitMs - processTotals.modelLimiterWaitMs,
        modelTimeoutErrors: sharedTotals.modelTimeoutErrors - processTotals.modelTimeoutErrors,
        modelRetryableProviderErrors:
          sharedTotals.modelRetryableProviderErrors - processTotals.modelRetryableProviderErrors,
        modelNonRetryableProviderErrors:
          sharedTotals.modelNonRetryableProviderErrors - processTotals.modelNonRetryableProviderErrors,
        findings: sharedTotals.findings - processTotals.findings,
        hits: sharedTotals.hits - processTotals.hits,
        misses: sharedTotals.misses - processTotals.misses,
        falsePositives: sharedTotals.falsePositives - processTotals.falsePositives,
      },
    },
    cases,
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

function compactCase(root, name, report) {
  const traceBundle = readTraceBundle(root, name, report);
  const maxToolCalls = runMaxToolCalls(traceBundle.runStart);
  const orchestrator = primarySession(traceBundle.audit);
  const instrumentation = optionalInstrumentationSummary({
    audit: traceBundle.audit,
    events: traceBundle.events,
  });
  const compactInstrumentation = compactOptionalInstrumentation(instrumentation);
  return {
    status: report.review?.status ?? null,
    sessions: report.review?.sessions ?? null,
    completedSessions: report.review?.completedSessions ?? null,
    modelCalls: report.review?.modelCalls ?? 0,
    toolCalls: report.review?.toolCalls ?? 0,
    totalTokens: report.review?.tokens?.totalTokens ?? 0,
    modelProviderRequestMs:
      report.review?.modelMetrics?.providerRequestMs ?? compactInstrumentation.providerRequestMs ?? 0,
    modelRetryBackoffMs:
      report.review?.modelMetrics?.retryBackoffMs ?? compactInstrumentation.backoffMs ?? 0,
    modelLimiterWaitMs: report.review?.modelMetrics?.limiterWaitMs ?? 0,
    maxModelLimiterWaitMs: report.review?.modelMetrics?.maxLimiterWaitMs ?? 0,
    modelTimeoutErrors: report.review?.modelMetrics?.timeoutErrors ?? 0,
    modelRetryableProviderErrors: report.review?.modelMetrics?.retryableProviderErrors ?? 0,
    modelNonRetryableProviderErrors:
      report.review?.modelMetrics?.nonRetryableProviderErrors ?? 0,
    findings: report.review?.findings ?? 0,
    candidates: report.benchmark?.candidateCount ?? 0,
    rejectedCandidates: report.benchmark?.rejectedCandidateCount ?? 0,
    hits: report.benchmark?.hits?.length ?? 0,
    misses: report.benchmark?.misses?.length ?? 0,
    falsePositiveCount: report.benchmark?.falsePositiveCount ?? 0,
    validationFailures: report.validationFailures ?? [],
    modelFailedEvents: countNeedle(
      path.join(root, "traces", name, "runtime-events.jsonl"),
      '"modelFailed"',
    ),
    stderrErrorIndicators: countRegex(
      path.join(root, "logs", `${name}.stderr.log`),
      /error|timeout|429|rate.?limit|retry|cancel/gi,
    ),
    orchestrator: {
      turns: orchestrator.turns ?? 0,
      modelTurnsPrepared: orchestrator.modelTurnsPrepared ?? 0,
      modelTurnsCompleted: orchestrator.modelTurnsCompleted ?? 0,
      toolCallsRequested: orchestrator.toolCallsRequested ?? 0,
      toolCallsCompleted: orchestrator.toolCallsCompleted ?? 0,
      toolCallsDenied: orchestrator.toolCallsDenied ?? 0,
      transcriptCompactions: orchestrator.transcriptCompactions ?? 0,
      evictedToolResults: orchestrator.evictedToolResults ?? 0,
      evictedItemCounts: orchestrator.evictedItemCounts ?? {},
      candidatesEmitted: orchestrator.candidatesEmitted ?? 0,
      candidateValidationsStarted: orchestrator.candidateValidationsStarted ?? 0,
      candidateValidationsCompleted: orchestrator.candidateValidationsCompleted ?? 0,
      candidateDecisions: orchestrator.candidateDecisions ?? 0,
      acceptedCandidates: orchestrator.acceptedCandidates ?? 0,
      rejectedCandidates: orchestrator.rejectedCandidates ?? 0,
      publicationSkipped: orchestrator.publicationSkipped ?? 0,
      publicationSkippedBudgetExhausted:
        orchestrator.publicationSkippedBudgetExhausted ?? 0,
      exhaustedMaxToolCalls:
        (orchestrator.toolCallsCompleted ?? 0) >= maxToolCalls ||
        (orchestrator.toolCallsRequested ?? 0) >= maxToolCalls,
    },
    instrumentation: compactInstrumentation,
  };
}

function countNeedle(file, needle) {
  if (!fs.existsSync(file)) return 0;
  return fs.readFileSync(file, "utf8").split(needle).length - 1;
}

function countRegex(file, pattern) {
  if (!fs.existsSync(file)) return 0;
  return fs.readFileSync(file, "utf8").match(pattern)?.length ?? 0;
}

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
    } else if (arg === "--shared" || arg === "--process" || arg === "--output") {
      parsed[arg.slice(2)] = argv[++index];
    } else {
      throw new Error(`unknown argument ${arg}`);
    }
  }
  return parsed;
}

function usage() {
  process.stderr.write(`Usage: compare-muzen-runner-modes.mjs --shared <dir> --process <dir> [--output <path>]\n`);
}
