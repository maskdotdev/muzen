#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { startFakeResponsesServer } from "./fake-responses-model.mjs";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  usage();
  process.exit(0);
}

const outputDir = path.resolve(args.outputDir || `/tmp/muzen-fake-runner-repro-${timestamp()}`);
const fixtureRoot = path.join(outputDir, "fixtures");
const worktreeRoot = path.join(fixtureRoot, "worktrees");
const goldenDir = path.join(fixtureRoot, "goldens");
const caseSource = path.join(fixtureRoot, "summary.json");
const runnerPath = path.resolve(args.runnerPath || "target/release/muzen-runner");
const caseCount = positiveInt(args.cases || "5", "--cases");
const concurrency = positiveInt(args.concurrency || "5", "--concurrency");
const sessions = nonnegativeInt(args.sessions || "1", "--sessions");
const maxActive = positiveInt(args.maxActive || "1", "--max-active");
const maxToolCalls = positiveInt(args.maxToolCalls || "6", "--max-tool-calls");
const maxTurns = positiveInt(args.maxTurns || String(maxToolCalls + 4), "--max-turns");
const toolsBeforeFinal =
  args.toolsBeforeFinal === "infinite"
    ? Number.POSITIVE_INFINITY
    : positiveInt(args.toolsBeforeFinal || "999999", "--tools-before-final");
const latencyMs = nonnegativeInt(args.latencyMs || "0", "--latency-ms");
const jitterMs = nonnegativeInt(args.jitterMs || "0", "--jitter-ms");
const maxConcurrent = positiveInt(args.maxConcurrent || "64", "--max-concurrent");
const invalidFinalAttempts = nonnegativeInt(args.invalidFinalAttempts || "0", "--invalid-final-attempts");
const httpErrorEvery = nonnegativeInt(args.httpErrorEvery || "0", "--http-error-every");
const toolName = args.toolName || "diff";
const sharedFinalMode = args.sharedFinalMode || args.finalMode || "clean";
const processFinalMode = args.processFinalMode || args.finalMode || sharedFinalMode;
const sharedValidationStatus = args.sharedValidationStatus || args.validationStatus || "supported";
const processValidationStatus =
  args.processValidationStatus || args.validationStatus || sharedValidationStatus;

fs.mkdirSync(outputDir, { recursive: true });
fs.rmSync(fixtureRoot, { recursive: true, force: true });
fs.mkdirSync(worktreeRoot, { recursive: true });
fs.mkdirSync(goldenDir, { recursive: true });
createFixtures({ caseCount, worktreeRoot, goldenDir, caseSource });

const fakeModel = await startFakeResponsesServer({
  latencyMs,
  jitterMs,
  maxConcurrent,
  toolsBeforeFinal,
  invalidFinalAttempts,
  httpErrorEvery,
  toolName,
  finalMode: sharedFinalMode,
  validationStatus: sharedValidationStatus,
  logPath: path.join(outputDir, "fake-model.jsonl"),
});

try {
  const sharedDir = path.join(outputDir, "shared");
  const processDir = path.join(outputDir, "process");
  const common = [
    "bench/review-quality/tools/run-muzen-martian-concurrent.mjs",
    "--case-source",
    caseSource,
    "--golden-dir",
    goldenDir,
    "--worktree-root",
    worktreeRoot,
    "--runner-path",
    runnerPath,
    "--concurrency",
    String(concurrency),
    "--limit",
    String(caseCount),
    "--sessions",
    String(sessions),
    "--max-active",
    String(maxActive),
    "--max-turns",
    String(maxTurns),
    "--max-tool-calls",
    String(maxToolCalls),
    "--model",
    "fake-responses-model",
    "--skip-semantic",
    "true",
    "--progress",
    args.progress || "false",
    "--sample-interval-ms",
    args.sampleIntervalMs || "1000",
  ];
  const env = {
    ...process.env,
    OPENAI_BASE_URL: fakeModel.baseUrl,
    OPENAI_API_KEY: "fake",
  };
  fakeModel.configure({
    finalMode: sharedFinalMode,
    validationStatus: sharedValidationStatus,
    runLabel: "shared",
  });
  await runCheckedAsync("node", [...common, "--runner-mode", "shared", "--output-dir", sharedDir], env);
  fakeModel.reset();
  fakeModel.configure({
    finalMode: processFinalMode,
    validationStatus: processValidationStatus,
    runLabel: "process",
  });
  await runCheckedAsync("node", [...common, "--runner-mode", "process", "--output-dir", processDir], env);

  const comparePath = path.join(outputDir, "runner-mode-compare.json");
  await runCheckedAsync(
    "node",
    [
      "bench/review-quality/tools/compare-muzen-runner-modes.mjs",
      "--shared",
      sharedDir,
      "--process",
      processDir,
      "--output",
      comparePath,
    ],
    process.env,
  );
  const compare = readJson(comparePath);
  const fakeModelLogPath = path.join(outputDir, "fake-model.jsonl");
  const summary = reproductionSummary({
    outputDir,
    sharedDir,
    processDir,
    comparePath,
    fakeModelLogPath,
    fakeModelBaseUrl: fakeModel.baseUrl,
    caseCount,
    concurrency,
    sessions,
    maxActive,
    maxToolCalls,
    maxTurns,
    toolsBeforeFinal: Number.isFinite(toolsBeforeFinal) ? toolsBeforeFinal : "infinite",
    latencyMs,
    jitterMs,
    maxConcurrent,
    invalidFinalAttempts,
    httpErrorEvery,
    toolName,
    sharedFinalMode,
    processFinalMode,
    sharedValidationStatus,
    processValidationStatus,
    compare,
  });
  const summaryPath = path.join(outputDir, "reproduction-summary.json");
  fs.writeFileSync(summaryPath, `${JSON.stringify(summary, null, 2)}\n`);
  process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`);
} finally {
  await fakeModel.close();
}

function reproductionSummary({
  outputDir,
  sharedDir,
  processDir,
  comparePath,
  fakeModelLogPath,
  fakeModelBaseUrl,
  caseCount,
  concurrency,
  sessions,
  maxActive,
  maxToolCalls,
  maxTurns,
  toolsBeforeFinal,
  latencyMs,
  jitterMs,
  maxConcurrent,
  invalidFinalAttempts,
  httpErrorEvery,
  toolName,
  sharedFinalMode,
  processFinalMode,
  sharedValidationStatus,
  processValidationStatus,
  compare,
}) {
  const sharedExhausted = compare.cases.filter((entry) => entry.shared.orchestrator.exhaustedMaxToolCalls);
  const processExhausted = compare.cases.filter((entry) => entry.process.orchestrator.exhaustedMaxToolCalls);
  const fakeModelMetrics = summarizeFakeModelLog(fakeModelLogPath);
  const release = {
    shared: summarizeRunMetrics(path.join(sharedDir, "metrics.jsonl")),
    process: summarizeRunMetrics(path.join(processDir, "metrics.jsonl")),
  };
  const admission = {
    shared: summarizeRunAdmission(sharedDir),
    process: summarizeRunAdmission(processDir),
  };
  const isolation = {
    shared: summarizeRunIsolation(sharedDir),
    process: summarizeRunIsolation(processDir),
  };
  const timing = summarizeTiming({ compare, fakeModelMetrics });
  const harnessOverhead = summarizeHarnessOverhead({ compare, admission });
  return {
    schemaVersion: "muzen.fake-runner-mode-repro.v1",
    generatedAtUtc: new Date().toISOString(),
    outputDir,
    sharedDir,
    processDir,
    comparePath,
    fakeModelLogPath,
    fakeModelBaseUrl,
    config: {
      caseCount,
      concurrency,
      sessions,
      maxActive,
      maxToolCalls,
      maxTurns,
      toolsBeforeFinal,
      latencyMs,
      jitterMs,
      maxConcurrent,
      invalidFinalAttempts,
      httpErrorEvery,
      toolName,
      sharedFinalMode,
      processFinalMode,
      sharedValidationStatus,
      processValidationStatus,
    },
    exhaustedMaxToolCalls: {
      shared: sharedExhausted.length,
      process: processExhausted.length,
      sharedOnly: sharedExhausted.filter(
        (entry) => !compare.cases.find((candidate) => candidate.name === entry.name)?.process.orchestrator.exhaustedMaxToolCalls,
      ).length,
    },
    reproducedObservedShape:
      sharedExhausted.length > processExhausted.length && sharedExhausted.length > 0,
    timing,
    harnessOverhead,
    observedRuns: summarizeObservedRuns(compare.cases),
    fakeModel: fakeModelMetrics,
    release,
    admission,
    isolation,
    totals: compare.totals,
    cases: compare.cases.map((entry) => ({
      name: entry.name,
      shared: entry.shared.orchestrator,
      process: entry.process.orchestrator,
      delta: entry.delta,
    })),
  };
}

function summarizeHarnessOverhead({ compare, admission }) {
  const shared = summarizeHarnessOverheadMode(compare.cases, "shared", admission.shared);
  const process = summarizeHarnessOverheadMode(compare.cases, "process", admission.process);
  return {
    shared,
    process,
    deltaSharedMinusProcess: harnessOverheadDelta(shared, process),
  };
}

function summarizeHarnessOverheadMode(cases, mode, admission) {
  const admissionByName = new Map((admission.detail ?? []).map((run) => [run.name, run]));
  const rows = cases.map((entry) => {
    const caseSummary = entry[mode] ?? {};
    const admitted = admissionByName.get(entry.name) ?? {};
    const parentElapsedMs = admitted.elapsedMs ?? null;
    const reviewElapsedMs = numberOrNull(caseSummary.reviewElapsedMs);
    const benchmarkElapsedMs = numberOrNull(caseSummary.benchmarkElapsedMs);
    return {
      name: entry.name,
      parentElapsedMs,
      benchmarkElapsedMs,
      reviewElapsedMs,
      parentMinusBenchmarkMs: nullableDelta(parentElapsedMs, benchmarkElapsedMs),
      parentMinusReviewMs: nullableDelta(parentElapsedMs, reviewElapsedMs),
      benchmarkMinusReviewMs: nullableDelta(benchmarkElapsedMs, reviewElapsedMs),
    };
  });
  return {
    cases: rows.length,
    parentElapsedMs: stats(rows.map((row) => row.parentElapsedMs)),
    benchmarkElapsedMs: stats(rows.map((row) => row.benchmarkElapsedMs)),
    reviewElapsedMs: stats(rows.map((row) => row.reviewElapsedMs)),
    parentMinusBenchmarkMs: stats(rows.map((row) => row.parentMinusBenchmarkMs)),
    parentMinusReviewMs: stats(rows.map((row) => row.parentMinusReviewMs)),
    benchmarkMinusReviewMs: stats(rows.map((row) => row.benchmarkMinusReviewMs)),
    detail: rows,
  };
}

function harnessOverheadDelta(shared, process) {
  return {
    parentElapsedMeanMs: nullableDelta(shared.parentElapsedMs.mean, process.parentElapsedMs.mean),
    benchmarkElapsedMeanMs: nullableDelta(
      shared.benchmarkElapsedMs.mean,
      process.benchmarkElapsedMs.mean,
    ),
    reviewElapsedMeanMs: nullableDelta(shared.reviewElapsedMs.mean, process.reviewElapsedMs.mean),
    parentMinusBenchmarkMeanMs: nullableDelta(
      shared.parentMinusBenchmarkMs.mean,
      process.parentMinusBenchmarkMs.mean,
    ),
    parentMinusReviewMeanMs: nullableDelta(
      shared.parentMinusReviewMs.mean,
      process.parentMinusReviewMs.mean,
    ),
    benchmarkMinusReviewMeanMs: nullableDelta(
      shared.benchmarkMinusReviewMs.mean,
      process.benchmarkMinusReviewMs.mean,
    ),
  };
}

function summarizeObservedRuns(cases) {
  return {
    shared: summarizeObservedMode(cases, "shared"),
    process: summarizeObservedMode(cases, "process"),
  };
}

function summarizeObservedMode(cases, mode) {
  const values = cases.map((entry) => entry[mode] ?? {});
  return {
    cases: values.length,
    sessions: stats(values.map((entry) => entry.sessions)),
    completedSessions: stats(values.map((entry) => entry.completedSessions)),
    completionDiagnostics: stats(values.map((entry) => entry.completionDiagnostics)),
    completionMaxToolCalls: stats(values.map((entry) => entry.completionMaxToolCalls)),
    completionToolCallsUsed: stats(values.map((entry) => entry.completionToolCallsUsed)),
    completionExhaustedToolBudget: stats(
      values.map((entry) => entry.completionExhaustedToolBudget),
    ),
    modelCalls: stats(values.map((entry) => entry.modelCalls)),
    toolCalls: stats(values.map((entry) => entry.toolCalls)),
  };
}

function summarizeRunAdmission(root) {
  const records = readJsonl(path.join(root, "metrics.jsonl"));
  const events = [];
  const runs = new Map();
  for (const record of records) {
    if (record.event !== "start" && record.event !== "finish") continue;
    const atMs = Date.parse(record.atUtc);
    if (!Number.isFinite(atMs) || !record.runId) continue;
    const existing = runs.get(record.runId) ?? {
      name: record.name,
      runId: record.runId,
      startMs: null,
      finishMs: null,
      elapsedMs: null,
      code: null,
    };
    if (record.event === "start") {
      existing.startMs = atMs;
    } else {
      existing.finishMs = atMs;
      existing.elapsedMs = record.elapsedMs ?? null;
      existing.code = record.code ?? null;
    }
    runs.set(record.runId, existing);
    events.push({ event: record.event, atMs, runId: record.runId });
  }
  events.sort(
    (left, right) =>
      left.atMs - right.atMs || (left.event === "start" ? -1 : 1),
  );
  let active = 0;
  let maxActiveRuns = 0;
  for (const event of events) {
    active += event.event === "start" ? 1 : -1;
    maxActiveRuns = Math.max(maxActiveRuns, active);
  }
  const runValues = [...runs.values()].sort(
    (left, right) =>
      numberOrZero(left.startMs) - numberOrZero(right.startMs) ||
      String(left.name).localeCompare(String(right.name)),
  );
  const startTimes = runValues.map((run) => run.startMs).filter(Number.isFinite).sort(numericSort);
  const finishTimes = runValues.map((run) => run.finishMs).filter(Number.isFinite).sort(numericSort);
  const firstStartMs = startTimes[0] ?? null;
  const lastStartMs = startTimes.at(-1) ?? null;
  const firstFinishMs = finishTimes[0] ?? null;
  const lastFinishMs = finishTimes.at(-1) ?? null;
  return {
    runs: runValues.length,
    maxActiveRuns,
    startWindowMs: nullableDelta(lastStartMs, firstStartMs),
    finishWindowMs: nullableDelta(lastFinishMs, firstFinishMs),
    elapsedMs: stats(runValues.map((run) => run.elapsedMs)),
    detail: runValues.map((run) => ({
      name: run.name,
      runId: run.runId,
      startOffsetMs: nullableDelta(run.startMs, firstStartMs),
      finishOffsetMs: nullableDelta(run.finishMs, firstStartMs),
      elapsedMs: run.elapsedMs,
      code: run.code,
    })),
  };
}

function summarizeTiming({ compare, fakeModelMetrics }) {
  const sharedTotals = timingSnapshot(compare.totals.shared ?? {});
  const processTotals = timingSnapshot(compare.totals.process ?? {});
  return {
    modeTotals: {
      shared: sharedTotals,
      process: processTotals,
      deltaSharedMinusProcess: timingDelta(sharedTotals, processTotals),
    },
    fakeProvider: {
      shared: fakeProviderTiming(fakeModelMetrics.byRunLabel?.shared),
      process: fakeProviderTiming(fakeModelMetrics.byRunLabel?.process),
      deltaSharedMinusProcess: fakeProviderTimingDelta(
        fakeModelMetrics.byRunLabel?.shared,
        fakeModelMetrics.byRunLabel?.process,
      ),
    },
    cases: compare.cases.map((entry) => {
      const shared = timingSnapshot(entry.shared ?? {});
      const process = timingSnapshot(entry.process ?? {});
      return {
        name: entry.name,
        shared,
        process,
        deltaSharedMinusProcess: timingDelta(shared, process),
      };
    }),
  };
}

function timingSnapshot(source) {
  return {
    reviewElapsedMs: numberOrNull(source.reviewElapsedMs),
    benchmarkElapsedMs: numberOrNull(source.benchmarkElapsedMs),
    modelProviderRequestMs: numberOrNull(source.modelProviderRequestMs),
    modelRetryBackoffMs: numberOrNull(source.modelRetryBackoffMs),
    modelLimiterWaitMs: numberOrNull(source.modelLimiterWaitMs),
    maxModelLimiterWaitMs: numberOrNull(source.maxModelLimiterWaitMs),
  };
}

function timingDelta(left, right) {
  return Object.fromEntries(
    Object.keys(left).map((field) => [field, nullableDelta(left[field], right[field])]),
  );
}

function fakeProviderTiming(summary) {
  return {
    requests: summary?.requests ?? 0,
    queuedMs: summary?.queuedMs ?? stats([]),
    elapsedMs: summary?.elapsedMs ?? stats([]),
    requestShape: summary?.requestShape ?? summarizeRequestShape([]),
  };
}

function fakeProviderTimingDelta(left, right) {
  return {
    queuedMs: statsDelta(left?.queuedMs, right?.queuedMs),
    elapsedMs: statsDelta(left?.elapsedMs, right?.elapsedMs),
  };
}

function statsDelta(left, right) {
  return {
    min: nullableDelta(left?.min, right?.min),
    p50: nullableDelta(left?.p50, right?.p50),
    p95: nullableDelta(left?.p95, right?.p95),
    max: nullableDelta(left?.max, right?.max),
    mean: nullableDelta(left?.mean, right?.mean),
  };
}

function summarizeRunMetrics(metricsPath) {
  const records = readJsonl(metricsPath);
  return {
    starts: records.filter((record) => record.event === "start").length,
    finishes: records.filter((record) => record.event === "finish").length,
    releases: records.filter((record) => record.event === "release").length,
    releaseErrors: records.filter((record) => record.event === "release_error").length,
    failedFinishes: records.filter((record) => record.event === "finish" && record.code !== 0)
      .length,
  };
}

function summarizeRunIsolation(root) {
  const metrics = readJsonl(path.join(root, "metrics.jsonl"));
  const summary = readJson(path.join(root, "summary.json"));
  const tracesByName = new Map(
    (summary.results ?? []).map((result) => [result.name, result.traceDir]),
  );
  const starts = metrics.filter((record) => record.event === "start");
  const orphanFrames = metrics.filter((record) => record.event === "orphan_frame");
  const runIds = starts.map((record) => record.runId).filter(Boolean);
  const duplicateRunIds = runIds.length - new Set(runIds).size;
  const cases = starts.map((start) => inspectRunArtifacts(start, tracesByName.get(start.name)));
  return {
    cases: cases.length,
    duplicateRunIds,
    orphanFrames: orphanFrames.length,
    missingFrameFiles: cases.filter((item) => item.frameFileMissing).length,
    frameRecords: sum(cases.map((item) => item.frameRecords)),
    frameMissingRunIds: sum(cases.map((item) => item.frameMissingRunIds)),
    unexpectedFrameRunIds: sum(cases.map((item) => item.unexpectedFrameRunIds)),
    missingTraceFiles: sum(cases.map((item) => item.missingTraceFiles)),
    traceRecords: sum(cases.map((item) => item.traceRecords)),
    traceMissingRunIds: sum(cases.map((item) => item.traceMissingRunIds)),
    unexpectedTraceRunIds: sum(cases.map((item) => item.unexpectedTraceRunIds)),
    detail: cases,
  };
}

function inspectRunArtifacts(start, traceDir) {
  const expectedRunId = start.runId ?? null;
  const frameInspection = inspectJsonlRunIds(start.framesPath, expectedRunId, frameRunId);
  const runtimeInspection = inspectJsonlRunIds(
    traceDir ? path.join(traceDir, "runtime-events.jsonl") : null,
    expectedRunId,
    eventRunId,
  );
  const reviewInspection = inspectJsonlRunIds(
    traceDir ? path.join(traceDir, "review-events.jsonl") : null,
    expectedRunId,
    eventRunId,
  );
  return {
    name: start.name,
    runId: expectedRunId,
    frameFileMissing: frameInspection.missing,
    frameRecords: frameInspection.records,
    frameMissingRunIds: frameInspection.missingRunIds,
    unexpectedFrameRunIds: frameInspection.unexpectedRunIds,
    missingTraceFiles: Number(runtimeInspection.missing) + Number(reviewInspection.missing),
    traceRecords: runtimeInspection.records + reviewInspection.records,
    traceMissingRunIds: runtimeInspection.missingRunIds + reviewInspection.missingRunIds,
    unexpectedTraceRunIds:
      runtimeInspection.unexpectedRunIds + reviewInspection.unexpectedRunIds,
  };
}

function inspectJsonlRunIds(file, expectedRunId, extractRunId) {
  if (!file || !fs.existsSync(file)) {
    return { missing: true, records: 0, missingRunIds: 0, unexpectedRunIds: 0 };
  }
  const records = readJsonl(file);
  let missingRunIds = 0;
  let unexpectedRunIds = 0;
  for (const record of records) {
    const runId = extractRunId(record);
    if (!runId) {
      missingRunIds += 1;
    } else if (expectedRunId && runId !== expectedRunId) {
      unexpectedRunIds += 1;
    }
  }
  return {
    missing: false,
    records: records.length,
    missingRunIds,
    unexpectedRunIds,
  };
}

function frameRunId(frame) {
  return (
    frame.params?.runId ??
    frame.params?.context?.runId ??
    frame.result?.runId ??
    frame.error?.data?.runId ??
    null
  );
}

function eventRunId(record) {
  return (
    record.context?.runId ??
    record.runId ??
    record.event?.context?.runId ??
    record.event?.runId ??
    null
  );
}

function summarizeFakeModelLog(logPath) {
  const records = readJsonl(logPath);
  const conversationKeys = [
    ...new Set(records.map((record) => record.conversationKey).filter(Boolean)),
  ].sort();
  const byRunLabel = {};
  for (const runLabel of [...new Set(records.map((record) => record.runLabel || "default"))].sort()) {
    byRunLabel[runLabel] = summarizeFakeModelRecords(
      records.filter((record) => (record.runLabel || "default") === runLabel),
    );
  }
  return {
    ...summarizeFakeModelRecords(records),
    conversationCount: conversationKeys.length,
    invalidFinalsByConversation: countObject(
      records
        .filter((record) => record.decision === "invalid_final_text")
        .map((record) => record.conversationKey),
    ),
    byRunLabel,
  };
}

function summarizeFakeModelRecords(records) {
  return {
    requests: records.length,
    decisions: countObject(records.map((record) => record.decision)),
    statuses: countObject(records.map((record) => record.status)),
    maxActiveAtStart: records.length
      ? Math.max(...records.map((record) => record.activeAtStart ?? 0))
      : null,
    queuedMs: stats(records.map((record) => record.queuedMs)),
    elapsedMs: stats(records.map((record) => record.elapsedMs)),
    requestShape: summarizeRequestShape(records),
  };
}

function summarizeRequestShape(records) {
  const ordered = records
    .map((record, index) => ({
      ...record,
      timestampMs: Date.parse(record.atUtc),
      originalIndex: index,
    }))
    .filter((record) => Number.isFinite(record.timestampMs))
    .sort(
      (left, right) =>
        left.timestampMs - right.timestampMs || left.originalIndex - right.originalIndex,
    );
  const firstTimestampMs = ordered[0]?.timestampMs ?? null;
  const lastTimestampMs = ordered.at(-1)?.timestampMs ?? null;
  const conversationIndexes = new Map();
  const requestOrder = ordered.map((record) => {
    const conversationKey = record.conversationKey || "unknown";
    if (!conversationIndexes.has(conversationKey)) {
      conversationIndexes.set(conversationKey, conversationIndexes.size + 1);
    }
    return {
      sequence: record.sequence,
      offsetMs: firstTimestampMs === null ? null : record.timestampMs - firstTimestampMs,
      conversationIndex: conversationIndexes.get(conversationKey),
      decision: record.decision,
      queuedMs: record.queuedMs ?? 0,
      elapsedMs: record.elapsedMs ?? 0,
      functionOutputs: record.functionOutputs ?? 0,
    };
  });
  const conversationCount = conversationIndexes.size;
  return {
    conversationCount,
    completionWindowMs:
      firstTimestampMs === null || lastTimestampMs === null
        ? null
        : lastTimestampMs - firstTimestampMs,
    firstWave: summarizeRequestWave(requestOrder.slice(0, conversationCount)),
    lastWave: summarizeRequestWave(requestOrder.slice(-conversationCount)),
    requestOrder,
  };
}

function summarizeRequestWave(records) {
  return {
    requests: records.length,
    uniqueConversations: new Set(records.map((record) => record.conversationIndex)).size,
    decisions: countObject(records.map((record) => record.decision)),
    queuedMs: stats(records.map((record) => record.queuedMs)),
    elapsedMs: stats(records.map((record) => record.elapsedMs)),
  };
}

function createFixtures({ caseCount, worktreeRoot, goldenDir, caseSource }) {
  const results = [];
  for (let index = 1; index <= caseCount; index += 1) {
    const owner = "fake";
    const repo = `runner-repro-${index}`;
    const number = index;
    const baseName = `${owner}-${repo}-pull-${number}`;
    const worktree = path.join(worktreeRoot, `${owner}-${repo}-pr-${number}`);
    fs.mkdirSync(path.join(worktree, "src"), { recursive: true });
    runChecked("git", ["init", "-q"], process.env, worktree);
    runChecked("git", ["config", "user.name", "Muzen Fake Repro"], process.env, worktree);
    runChecked("git", ["config", "user.email", "muzen-fake-repro@example.invalid"], process.env, worktree);
    fs.writeFileSync(path.join(worktree, "src", "example.txt"), `base value ${index}\n`);
    runChecked("git", ["add", "src/example.txt"], process.env, worktree);
    runChecked("git", ["commit", "-q", "-m", "base"], process.env, worktree);
    runChecked("git", ["branch", `hagent-martian/pr-${number}-base`], process.env, worktree);
    fs.writeFileSync(path.join(worktree, "src", "example.txt"), `base value ${index}\nhead value ${index}\n`);
    runChecked("git", ["add", "src/example.txt"], process.env, worktree);
    runChecked("git", ["commit", "-q", "-m", "head"], process.env, worktree);
    fs.writeFileSync(path.join(goldenDir, `${baseName}.json`), `${JSON.stringify({ issues: [] }, null, 2)}\n`);
    results.push({
      prUrl: `https://github.com/${owner}/${repo}/pull/${number}`,
      title: `Synthetic runner repro ${number}`,
    });
  }
  fs.writeFileSync(caseSource, `${JSON.stringify({ results }, null, 2)}\n`);
}

function runChecked(command, commandArgs, env, cwd = process.cwd()) {
  const result = spawnSync(command, commandArgs, {
    cwd,
    env,
    encoding: "utf8",
    maxBuffer: 1024 * 1024 * 64,
  });
  if (result.status !== 0) {
    throw new Error(
      `${command} ${commandArgs.join(" ")} failed with ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  return result;
}

function runCheckedAsync(command, commandArgs, env, cwd = process.cwd()) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, commandArgs, {
      cwd,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const stdout = [];
    const stderr = [];
    child.stdout.on("data", (chunk) => stdout.push(chunk.toString("utf8")));
    child.stderr.on("data", (chunk) => stderr.push(chunk.toString("utf8")));
    child.on("error", reject);
    child.on("exit", (code, signal) => {
      if (code === 0) {
        resolve({ stdout: stdout.join(""), stderr: stderr.join("") });
        return;
      }
      reject(
        new Error(
          `${command} ${commandArgs.join(" ")} failed with code=${code} signal=${signal}\nstdout:\n${stdout.join("")}\nstderr:\n${stderr.join("")}`,
        ),
      );
    });
  });
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function readJsonl(file) {
  if (!fs.existsSync(file)) return [];
  return fs
    .readFileSync(file, "utf8")
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line));
}

function stats(values) {
  const clean = values.filter((value) => Number.isFinite(value)).sort((left, right) => left - right);
  if (clean.length === 0) {
    return { count: 0, min: null, p50: null, p95: null, max: null, mean: null };
  }
  return {
    count: clean.length,
    min: clean[0],
    p50: percentile(clean, 0.5),
    p95: percentile(clean, 0.95),
    max: clean.at(-1),
    mean: clean.reduce((total, value) => total + value, 0) / clean.length,
  };
}

function percentile(sortedValues, percentileValue) {
  const index = Math.min(
    sortedValues.length - 1,
    Math.max(0, Math.ceil(sortedValues.length * percentileValue) - 1),
  );
  return sortedValues[index];
}

function countObject(values) {
  const counts = new Map();
  for (const value of values) {
    if (value == null) continue;
    counts.set(value, (counts.get(value) ?? 0) + 1);
  }
  return Object.fromEntries([...counts.entries()].sort(([left], [right]) => String(left).localeCompare(String(right))));
}

function sum(values) {
  return values.reduce((total, value) => total + (Number.isFinite(value) ? value : 0), 0);
}

function numericSort(left, right) {
  return left - right;
}

function numberOrZero(value) {
  return Number.isFinite(value) ? value : 0;
}

function numberOrNull(value) {
  return Number.isFinite(value) ? value : null;
}

function nullableDelta(left, right) {
  if (!Number.isFinite(left) && !Number.isFinite(right)) return null;
  return numberOrZero(left) - numberOrZero(right);
}

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
      continue;
    }
    if (!arg.startsWith("--")) throw new Error(`unexpected argument: ${arg}`);
    const key = arg.slice(2).replace(/-([a-z])/g, (_, char) => char.toUpperCase());
    const value = argv[++index];
    if (value == null || value.startsWith("--")) throw new Error(`missing value for ${arg}`);
    parsed[key] = value;
  }
  return parsed;
}

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 1) throw new Error(`${name} must be a positive integer`);
  return parsed;
}

function nonnegativeInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) throw new Error(`${name} must be a non-negative integer`);
  return parsed;
}

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\..+/, "Z");
}

function usage() {
  process.stderr.write(`Usage: run-fake-runner-mode-repro.mjs [--runner-path target/release/muzen-runner] [--output-dir /tmp/repro] [--cases 5] [--concurrency 5] [--sessions 1] [--max-active 1] [--max-tool-calls 6] [--tools-before-final N|infinite] [--latency-ms N] [--max-concurrent N] [--final-mode clean|candidate] [--shared-final-mode clean|candidate] [--process-final-mode clean|candidate] [--validation-status supported|refuted|insufficient|needs_more_evidence] [--shared-validation-status supported|refuted|insufficient|needs_more_evidence] [--process-validation-status supported|refuted|insufficient|needs_more_evidence]\n`);
}
