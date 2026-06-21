#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

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
        modelLimiterWaitMs:
          sharedCase.modelLimiterWaitMs - processCase.modelLimiterWaitMs,
        maxModelLimiterWaitMs:
          sharedCase.maxModelLimiterWaitMs - processCase.maxModelLimiterWaitMs,
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
  return {
    generatedAtUtc: new Date().toISOString(),
    compared: {
      shared: sharedRoot,
      process: processRoot,
    },
    totals: {
      shared: sharedSummary.totals,
      process: processSummary.totals,
      delta: {
        modelCalls: sharedSummary.totals.modelCalls - processSummary.totals.modelCalls,
        toolCalls: sharedSummary.totals.toolCalls - processSummary.totals.toolCalls,
        totalTokens: sharedSummary.totals.totalTokens - processSummary.totals.totalTokens,
        modelLimiterWaitMs:
          (sharedSummary.totals.modelLimiterWaitMs ?? 0) -
          (processSummary.totals.modelLimiterWaitMs ?? 0),
        findings: sharedSummary.totals.findings - processSummary.totals.findings,
        hits: sharedSummary.totals.hits - processSummary.totals.hits,
        misses: sharedSummary.totals.misses - processSummary.totals.misses,
        falsePositives:
          sharedSummary.totals.falsePositives - processSummary.totals.falsePositives,
      },
    },
    cases,
  };
}

function compactCase(root, name, report) {
  const maxToolCalls = caseMaxToolCalls(root, name);
  const orchestrator = orchestratorDiagnostics(report);
  return {
    status: report.review?.status ?? null,
    sessions: report.review?.sessions ?? null,
    completedSessions: report.review?.completedSessions ?? null,
    modelCalls: report.review?.modelCalls ?? 0,
    toolCalls: report.review?.toolCalls ?? 0,
    totalTokens: report.review?.tokens?.totalTokens ?? 0,
    modelLimiterWaitMs: report.review?.modelMetrics?.limiterWaitMs ?? 0,
    maxModelLimiterWaitMs: report.review?.modelMetrics?.maxLimiterWaitMs ?? 0,
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
      candidateDecisions: orchestrator.candidateDecisions ?? 0,
      rejectedCandidates: orchestrator.rejectedCandidates ?? 0,
      exhaustedMaxToolCalls:
        (orchestrator.toolCallsCompleted ?? 0) >= maxToolCalls ||
        (orchestrator.toolCallsRequested ?? 0) >= maxToolCalls,
    },
  };
}

function orchestratorDiagnostics(report) {
  return (
    report.audit?.diagnostics?.sessions?.find(
      (session) => session.sessionId === "review-orchestrator",
    ) ?? {}
  );
}

function caseMaxToolCalls(root, name) {
  const startPath = path.join(root, "jobs", name, "run-start.json");
  if (!fs.existsSync(startPath)) return 50;
  const start = readJson(startPath);
  return start.sessions?.[0]?.budget?.maxToolCalls ?? 50;
}

function countNeedle(file, needle) {
  if (!fs.existsSync(file)) return 0;
  return fs.readFileSync(file, "utf8").split(needle).length - 1;
}

function countRegex(file, pattern) {
  if (!fs.existsSync(file)) return 0;
  return fs.readFileSync(file, "utf8").match(pattern)?.length ?? 0;
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
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
