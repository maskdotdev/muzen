#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  usage();
  process.exit(0);
}

const outputDir = path.resolve(
  args.outputDir || `/tmp/muzen-fake-runner-mode-sweep-${timestamp()}`,
);
const runnerPath = path.resolve(args.runnerPath || "target/release/muzen-runner");
const concurrencies = listOfPositiveInts(args.concurrency || "1,2,3,5,8", "--concurrency");
const cases = positiveInt(args.cases || String(Math.max(...concurrencies)), "--cases");
const sessions = nonnegativeInt(args.sessions || "1", "--sessions");
const maxActive = positiveInt(args.maxActive || "1", "--max-active");
const fixtureExtraLines = nonnegativeInt(args.fixtureExtraLines || "0", "--fixture-extra-lines");
const fixtureLineBytes = positiveInt(args.fixtureLineBytes || "80", "--fixture-line-bytes");
const latencyMs = nonnegativeInt(args.latencyMs || "25", "--latency-ms");
const jitterMs = nonnegativeInt(args.jitterMs || "0", "--jitter-ms");
const maxConcurrent = positiveInt(args.maxConcurrent || "1", "--max-concurrent");
const maxToolCalls = positiveInt(args.maxToolCalls || "6", "--max-tool-calls");
const maxTurns = positiveInt(args.maxTurns || "10", "--max-turns");
const toolsBeforeFinal = args.toolsBeforeFinal || "1";
const finalMode = args.finalMode || "clean";
const sharedFinalMode = args.sharedFinalMode || finalMode;
const processFinalMode = args.processFinalMode || finalMode;
const validationStatus = args.validationStatus || "supported";
const sharedValidationStatus = args.sharedValidationStatus || validationStatus;
const processValidationStatus = args.processValidationStatus || validationStatus;
const invalidFinalAttempts = nonnegativeInt(
  args.invalidFinalAttempts || "0",
  "--invalid-final-attempts",
);
const httpErrorEvery = nonnegativeInt(args.httpErrorEvery || "0", "--http-error-every");
const httpErrorAttemptsPerRequest = nonnegativeInt(
  args.httpErrorAttemptsPerRequest || "0",
  "--http-error-attempts-per-request",
);
const toolName = args.toolName || "diff";
const viaCodexProxy = booleanArg(args.viaCodexProxy || "false", "--via-codex-proxy");
const postPrepareCooldownMs = nonnegativeInt(
  args.postPrepareCooldownMs || "3000",
  "--post-prepare-cooldown-ms",
);
const failOnRegression = booleanArg(args.failOnRegression || "true", "--fail-on-regression");

if (!fs.existsSync(runnerPath)) {
  fail(`runner not found at ${runnerPath}; run: cargo build --release --bin muzen-runner`);
}

fs.mkdirSync(outputDir, { recursive: true });
const runs = [];
for (const concurrency of concurrencies) {
  const runDir = path.join(outputDir, `c${concurrency}`);
  const result = spawnSync(
    "node",
    [
      "bench/review-quality/tools/run-fake-runner-mode-repro.mjs",
      "--runner-path",
      runnerPath,
      "--output-dir",
      runDir,
      "--cases",
      String(cases),
      "--concurrency",
      String(concurrency),
      "--sessions",
      String(sessions),
      "--max-active",
      String(maxActive),
      "--fixture-extra-lines",
      String(fixtureExtraLines),
      "--fixture-line-bytes",
      String(fixtureLineBytes),
      "--max-tool-calls",
      String(maxToolCalls),
      "--max-turns",
      String(maxTurns),
      "--tools-before-final",
      toolsBeforeFinal,
      "--shared-final-mode",
      sharedFinalMode,
      "--process-final-mode",
      processFinalMode,
      "--shared-validation-status",
      sharedValidationStatus,
      "--process-validation-status",
      processValidationStatus,
      "--invalid-final-attempts",
      String(invalidFinalAttempts),
      "--http-error-every",
      String(httpErrorEvery),
      "--http-error-attempts-per-request",
      String(httpErrorAttemptsPerRequest),
      "--tool-name",
      toolName,
      "--via-codex-proxy",
      String(viaCodexProxy),
      "--latency-ms",
      String(latencyMs),
      "--jitter-ms",
      String(jitterMs),
      "--max-concurrent",
      String(maxConcurrent),
      "--post-prepare-cooldown-ms",
      String(postPrepareCooldownMs),
      "--progress",
      "false",
    ],
    {
      cwd: process.cwd(),
      env: process.env,
      encoding: "utf8",
      maxBuffer: 1024 * 1024 * 128,
    },
  );
  if (result.status !== 0) {
    fail(
      `fake runner-mode sweep failed at concurrency=${concurrency} status=${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  let summary;
  try {
    summary = JSON.parse(result.stdout);
  } catch (error) {
    fail(
      `fake runner-mode sweep emitted invalid JSON at concurrency=${concurrency}: ${error.message}\nstdout:\n${result.stdout}`,
    );
  }
  runs.push(compactRun(summary, { concurrency, outputDir: runDir }));
}

const report = {
  schemaVersion: "muzen.fake-runner-mode-sweep.v1",
  generatedAtUtc: new Date().toISOString(),
  outputDir,
  runnerPath,
  config: {
    concurrencies,
    cases,
    sessions,
    maxActive,
    fixtureExtraLines,
    fixtureLineBytes,
    latencyMs,
    jitterMs,
    maxConcurrent,
    maxToolCalls,
    maxTurns,
    toolsBeforeFinal,
    sharedFinalMode,
    processFinalMode,
    sharedValidationStatus,
    processValidationStatus,
    invalidFinalAttempts,
    httpErrorEvery,
    httpErrorAttemptsPerRequest,
    toolName,
    viaCodexProxy,
    postPrepareCooldownMs,
    failOnRegression,
  },
  regressions: summarizeRegressions(runs),
  runs,
};

fs.writeFileSync(path.join(outputDir, "sweep-summary.json"), `${JSON.stringify(report, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
if (failOnRegression && hasBlockingRegressions(report.regressions)) {
  fail(`fake runner-mode sweep found regressions; see ${path.join(outputDir, "sweep-summary.json")}`);
}

function compactRun(summary, { concurrency, outputDir }) {
  const totals = summary.totals ?? {};
  const timing = summary.timing ?? {};
  const harness = summary.harnessOverhead ?? {};
  const sharedHarness = withoutDetail(harness.shared);
  const processHarness = withoutDetail(harness.process);
  const sharedQueue = timing.fakeProvider?.shared?.queuedMs ?? null;
  const processQueue = timing.fakeProvider?.process?.queuedMs ?? null;
  return {
    concurrency,
    outputDir,
    totals: {
      shared: compactTotals(totals.shared),
      process: compactTotals(totals.process),
      delta: compactTotals(totals.delta),
    },
    modeTotals: timing.modeTotals ?? null,
    fakeProviderQueuedMs: {
      shared: sharedQueue,
      process: processQueue,
      meanDeltaSharedMinusProcess: nullableDelta(sharedQueue?.mean, processQueue?.mean),
      p95DeltaSharedMinusProcess: nullableDelta(sharedQueue?.p95, processQueue?.p95),
    },
    harnessOverhead: {
      shared: sharedHarness,
      process: processHarness,
      deltaSharedMinusProcess: harness.deltaSharedMinusProcess ?? null,
    },
    release: summary.release,
    isolation: summary.isolation,
    exhaustedMaxToolCalls: summary.exhaustedMaxToolCalls,
    observedRuns: summary.observedRuns,
    fakeModel: compactFakeModel(summary.fakeModel),
    codexProxy: summary.codexProxy,
    parity: {
      modelCalls: totals.shared?.modelCalls === totals.process?.modelCalls,
      toolCalls: totals.shared?.toolCalls === totals.process?.toolCalls,
      totalTokens: totals.shared?.totalTokens === totals.process?.totalTokens,
      findings: totals.shared?.findings === totals.process?.findings,
    },
  };
}

function compactFakeModel(fakeModel) {
  if (!fakeModel) return null;
  return {
    requests: fakeModel.requests,
    conversationCount: fakeModel.conversationCount,
    decisions: fakeModel.decisions,
    statuses: fakeModel.statuses,
    invalidFinalsByConversation: fakeModel.invalidFinalsByConversation,
    queuedMs: fakeModel.queuedMs,
    byRunLabel: Object.fromEntries(
      Object.entries(fakeModel.byRunLabel ?? {}).map(([label, summary]) => [
        label,
        {
          requests: summary.requests,
          decisions: summary.decisions,
          statuses: summary.statuses,
          queuedMs: summary.queuedMs,
        },
      ]),
    ),
  };
}

function summarizeRegressions(runs) {
  return {
    parityFailures: runs.filter((run) => Object.values(run.parity).some((value) => !value)),
    releaseFailures: runs.filter(
      (run) =>
        (run.release?.shared?.releaseErrors ?? 0) > 0 ||
        (run.release?.shared?.failedFinishes ?? 0) > 0 ||
        (run.release?.process?.releaseErrors ?? 0) > 0 ||
        (run.release?.process?.failedFinishes ?? 0) > 0,
    ),
    isolationFailures: runs.filter(
      (run) =>
        modeIsolationFailures(run.isolation?.shared, { allowedFrameMissingRunIds: 0 }) > 0 ||
        modeIsolationFailures(run.isolation?.process, {
          allowedFrameMissingRunIds: run.isolation?.process?.cases ?? 0,
        }) > 0,
    ),
    maxProcessFirstFrameMs: maxStat(
      runs.map((run) => run.harnessOverhead.process?.runnerInvocationFirstFrameMs?.max),
    ),
    maxParentMinusReviewDeltaMs: maxStat(
      runs.map((run) =>
        Math.abs(run.harnessOverhead.deltaSharedMinusProcess?.parentMinusReviewMeanMs ?? 0),
      ),
    ),
    maxProviderQueueMeanDeltaMs: maxStat(
      runs.map((run) => Math.abs(run.fakeProviderQueuedMs.meanDeltaSharedMinusProcess ?? 0)),
    ),
  };
}

function hasBlockingRegressions(regressions) {
  return (
    (regressions.parityFailures?.length ?? 0) > 0 ||
    (regressions.releaseFailures?.length ?? 0) > 0 ||
    (regressions.isolationFailures?.length ?? 0) > 0
  );
}

function modeIsolationFailures(mode, { allowedFrameMissingRunIds }) {
  if (!mode) return 1;
  return (
    (mode.duplicateRunIds ?? 0) +
    (mode.orphanFrames ?? 0) +
    (mode.missingFrameFiles ?? 0) +
    Math.max(0, (mode.frameMissingRunIds ?? 0) - allowedFrameMissingRunIds) +
    (mode.unexpectedFrameRunIds ?? 0) +
    (mode.missingTraceFiles ?? 0) +
    (mode.traceMissingRunIds ?? 0) +
    (mode.unexpectedTraceRunIds ?? 0)
  );
}

function compactTotals(totals) {
  if (!totals) return null;
  return {
    findings: totals.findings,
    modelCalls: totals.modelCalls,
    toolCalls: totals.toolCalls,
    totalTokens: totals.totalTokens,
    modelProviderRequestMs: totals.modelProviderRequestMs,
    reviewElapsedMs: totals.reviewElapsedMs,
    benchmarkElapsedMs: totals.benchmarkElapsedMs,
  };
}

function withoutDetail(mode) {
  if (!mode) return null;
  const { detail, ...rest } = mode;
  return rest;
}

function maxStat(values) {
  const clean = values.filter((value) => Number.isFinite(value));
  return clean.length ? Math.max(...clean) : null;
}

function nullableDelta(left, right) {
  if (!Number.isFinite(left) || !Number.isFinite(right)) return null;
  return left - right;
}

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
      continue;
    }
    if (!arg.startsWith("--")) fail(`unexpected argument: ${arg}`);
    const key = arg.slice(2).replace(/-([a-z])/g, (_, char) => char.toUpperCase());
    const value = argv[++index];
    if (value == null || value.startsWith("--")) fail(`missing value for ${arg}`);
    parsed[key] = value;
  }
  return parsed;
}

function listOfPositiveInts(value, name) {
  const parsed = String(value)
    .split(",")
    .map((item) => positiveInt(item.trim(), name));
  if (parsed.length === 0) fail(`${name} must include at least one value`);
  return [...new Set(parsed)].sort((left, right) => left - right);
}

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 1) fail(`${name} must be a positive integer`);
  return parsed;
}

function nonnegativeInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) fail(`${name} must be a non-negative integer`);
  return parsed;
}

function booleanArg(value, name) {
  if (value === true || value === "true") return true;
  if (value === false || value === "false") return false;
  fail(`${name} must be true or false`);
}

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\..+/, "Z");
}

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

function usage() {
  process.stderr.write(
    "Usage: run-fake-runner-mode-sweep.mjs [--runner-path target/release/muzen-runner] [--output-dir /tmp/sweep] [--concurrency 1,2,3,5,8] [--cases N] [--sessions N] [--max-active N] [--fixture-extra-lines N] [--fixture-line-bytes N] [--latency-ms 25] [--max-concurrent 1] [--final-mode clean|candidate] [--invalid-final-attempts N] [--http-error-attempts-per-request N] [--via-codex-proxy true|false] [--fail-on-regression true|false] [--post-prepare-cooldown-ms 3000]\n",
  );
}
