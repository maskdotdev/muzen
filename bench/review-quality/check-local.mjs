#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  usage();
  process.exit(0);
}

const runnerPath = path.resolve(args.runnerPath || "target/release/muzen-runner");
if (!fs.existsSync(runnerPath)) {
  fail(`runner not found at ${runnerPath}; run: cargo build --release --bin muzen-runner`);
}

const outputRoot = path.resolve(
  args.outputDir || `bench/results-review-quality/check-local-${timestamp()}`,
);
fs.mkdirSync(outputRoot, { recursive: true });

const probes = [
  {
    name: "finalize-after-one-tool",
    toolsBeforeFinal: "1",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
  },
  {
    name: "symmetric-tool-budget-exhaustion",
    toolsBeforeFinal: "infinite",
    finalMode: "clean",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 5,
    expectFindings: 0,
  },
  {
    name: "candidate-publication",
    toolsBeforeFinal: "1",
    finalMode: "candidate",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 5,
  },
  {
    name: "schema-repair-per-conversation",
    toolsBeforeFinal: "1",
    invalidFinalAttempts: "1",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 0,
    expectInvalidFinalsPerConversation: true,
  },
  {
    name: "provider-queue-saturation",
    toolsBeforeFinal: "1",
    maxConcurrent: "1",
    latencyMs: "25",
    jitterMs: "0",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 0,
    expectProviderQueue: true,
    expectConcurrentAdmission: true,
  },
  {
    name: "caller-hard-cap-budget",
    toolsBeforeFinal: "1",
    cases: "1",
    concurrency: "1",
    maxToolCalls: "4",
    maxTurns: "8",
    sessions: "1",
    maxActive: "1",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 0,
    expectCompletionMaxToolCalls: 4,
  },
  {
    name: "adaptive-budget-surface",
    toolsBeforeFinal: "1",
    cases: "1",
    concurrency: "1",
    maxToolCalls: "4",
    maxTurns: "8",
    sessions: "0",
    maxActive: "1",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 0,
    expectCompletionMaxToolCallsGreaterThan: 4,
  },
];

const results = [];
for (const probe of probes) {
  const outputDir = path.join(outputRoot, probe.name);
  const summary = runProbe({
    runnerPath,
    outputDir,
    toolsBeforeFinal: probe.toolsBeforeFinal,
    invalidFinalAttempts: probe.invalidFinalAttempts || "0",
    finalMode: probe.finalMode || "clean",
    cases: probe.cases || args.cases || "5",
    concurrency: probe.concurrency || args.concurrency || "5",
    maxToolCalls: probe.maxToolCalls || args.maxToolCalls || "6",
    maxTurns: probe.maxTurns || args.maxTurns || "10",
    sessions: probe.sessions || args.sessions || "1",
    maxActive: probe.maxActive || args.maxActive || "1",
    latencyMs: probe.latencyMs || args.latencyMs || "5",
    jitterMs: probe.jitterMs || args.jitterMs || "10",
    maxConcurrent: probe.maxConcurrent || args.maxConcurrent || "64",
  });
  assertProbe(probe, summary);
  results.push({
    name: probe.name,
    outputDir,
    shared: compactTotals(summary.totals.shared),
    process: compactTotals(summary.totals.process),
    exhaustedMaxToolCalls: summary.exhaustedMaxToolCalls,
    observedRuns: summary.observedRuns,
    timing: compactTiming(summary.timing),
    harnessOverhead: compactHarnessOverhead(summary.harnessOverhead),
    fakeModel: compactFakeModel(summary.fakeModel),
    release: summary.release,
    admission: compactAdmission(summary.admission),
    isolation: compactIsolation(summary.isolation),
  });
}

process.stdout.write(
  `${JSON.stringify(
    {
      schemaVersion: "muzen.review-quality-check-local.v1",
      generatedAtUtc: new Date().toISOString(),
      outputRoot,
      runnerPath,
      probes: results,
    },
    null,
    2,
  )}\n`,
);

function runProbe({
  runnerPath,
  outputDir,
  toolsBeforeFinal,
  invalidFinalAttempts,
  finalMode,
  cases,
  concurrency,
  maxToolCalls,
  maxTurns,
  sessions,
  maxActive,
  latencyMs,
  jitterMs,
  maxConcurrent,
}) {
  const result = spawnSync(
    "node",
    [
      "bench/review-quality/tools/run-fake-runner-mode-repro.mjs",
      "--runner-path",
      runnerPath,
      "--output-dir",
      outputDir,
      "--cases",
      cases,
      "--concurrency",
      concurrency,
      "--max-tool-calls",
      maxToolCalls,
      "--max-turns",
      maxTurns,
      "--sessions",
      sessions,
      "--max-active",
      maxActive,
      "--tools-before-final",
      toolsBeforeFinal,
      "--invalid-final-attempts",
      invalidFinalAttempts,
      "--final-mode",
      finalMode,
      "--latency-ms",
      latencyMs,
      "--jitter-ms",
      jitterMs,
      "--max-concurrent",
      maxConcurrent,
      "--progress",
      "false",
    ],
    {
      cwd: process.cwd(),
      env: process.env,
      encoding: "utf8",
      maxBuffer: 1024 * 1024 * 64,
    },
  );
  if (result.status !== 0) {
    fail(
      `fake runner-mode probe failed with status ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  try {
    return JSON.parse(result.stdout);
  } catch (error) {
    fail(`fake runner-mode probe did not emit JSON: ${error.message}\nstdout:\n${result.stdout}`);
  }
}

function assertProbe(probe, summary) {
  assertEqual(
    `${probe.name} shared-only exhaustion`,
    summary.exhaustedMaxToolCalls.sharedOnly,
    probe.expectSharedOnlyExhaustion,
  );
  assertEqual(
    `${probe.name} shared exhaustion`,
    summary.exhaustedMaxToolCalls.shared,
    probe.expectExhausted,
  );
  assertEqual(
    `${probe.name} process exhaustion`,
    summary.exhaustedMaxToolCalls.process,
    probe.expectExhausted,
  );
  assertEqual(
    `${probe.name} model calls`,
    summary.totals.shared.modelCalls,
    summary.totals.process.modelCalls,
  );
  assertEqual(
    `${probe.name} tool calls`,
    summary.totals.shared.toolCalls,
    summary.totals.process.toolCalls,
  );
  assertEqual(
    `${probe.name} total tokens`,
    summary.totals.shared.totalTokens,
    summary.totals.process.totalTokens,
  );
  assertEqual(
    `${probe.name} shared findings`,
    summary.totals.shared.findings,
    probe.expectFindings ?? 0,
  );
  assertEqual(
    `${probe.name} process findings`,
    summary.totals.process.findings,
    probe.expectFindings ?? 0,
  );
  assertEqual(
    `${probe.name} provider errors`,
    providerErrors(summary.totals.shared),
    0,
  );
  assertEqual(
    `${probe.name} provider errors`,
    providerErrors(summary.totals.process),
    0,
  );
  assertEqual(`${probe.name} shared release errors`, summary.release.shared.releaseErrors, 0);
  assertEqual(`${probe.name} shared failed finishes`, summary.release.shared.failedFinishes, 0);
  assertEqual(`${probe.name} process release errors`, summary.release.process.releaseErrors, 0);
  assertEqual(`${probe.name} process failed finishes`, summary.release.process.failedFinishes, 0);
  assertIsolation(probe.name, "shared", summary.isolation.shared, { requireFrames: true });
  assertIsolation(probe.name, "process", summary.isolation.process, { requireFrames: false });
  if (probe.expectInvalidFinalsPerConversation) {
    const invalidFinalsByConversation = Object.values(
      summary.fakeModel.invalidFinalsByConversation ?? {},
    );
    assertEqual(
      `${probe.name} invalid-final conversation count`,
      invalidFinalsByConversation.length,
      summary.config.caseCount,
    );
    assertEqual(
      `${probe.name} invalid finals`,
      summary.fakeModel.decisions.invalid_final_text ?? 0,
      summary.config.caseCount * 2,
    );
    for (const [index, count] of invalidFinalsByConversation.entries()) {
      assertEqual(`${probe.name} invalid finals conversation ${index + 1}`, count, 2);
    }
  }
  if (probe.expectProviderQueue) {
    assertQueuedProvider(probe.name, "shared", summary.fakeModel.byRunLabel.shared);
    assertQueuedProvider(probe.name, "process", summary.fakeModel.byRunLabel.process);
  }
  if (probe.expectConcurrentAdmission) {
    assertGreaterThan(
      `${probe.name} shared concurrent admission`,
      summary.admission.shared.maxActiveRuns,
      1,
    );
    assertGreaterThan(
      `${probe.name} process concurrent admission`,
      summary.admission.process.maxActiveRuns,
      1,
    );
  }
  if (probe.expectCompletionMaxToolCalls != null) {
    assertCompletionMaxToolCalls(
      probe.name,
      "shared",
      summary.observedRuns.shared,
      probe.expectCompletionMaxToolCalls,
    );
    assertCompletionMaxToolCalls(
      probe.name,
      "process",
      summary.observedRuns.process,
      probe.expectCompletionMaxToolCalls,
    );
  }
  if (probe.expectCompletionMaxToolCallsGreaterThan != null) {
    assertGreaterThan(
      `${probe.name} shared completion max tool calls`,
      summary.observedRuns.shared.completionMaxToolCalls.max,
      probe.expectCompletionMaxToolCallsGreaterThan,
    );
    assertGreaterThan(
      `${probe.name} process completion max tool calls`,
      summary.observedRuns.process.completionMaxToolCalls.max,
      probe.expectCompletionMaxToolCallsGreaterThan,
    );
  }
}

function assertIsolation(probeName, mode, isolation, { requireFrames }) {
  assertEqual(`${probeName} ${mode} duplicate runIds`, isolation.duplicateRunIds, 0);
  assertEqual(`${probeName} ${mode} orphan frames`, isolation.orphanFrames, 0);
  assertEqual(`${probeName} ${mode} missing trace files`, isolation.missingTraceFiles, 0);
  assertEqual(`${probeName} ${mode} trace missing runIds`, isolation.traceMissingRunIds, 0);
  assertEqual(`${probeName} ${mode} unexpected trace runIds`, isolation.unexpectedTraceRunIds, 0);
  if (requireFrames) {
    assertEqual(`${probeName} ${mode} missing frame files`, isolation.missingFrameFiles, 0);
    assertEqual(`${probeName} ${mode} frame missing runIds`, isolation.frameMissingRunIds, 0);
    assertEqual(`${probeName} ${mode} unexpected frame runIds`, isolation.unexpectedFrameRunIds, 0);
  }
}

function compactTotals(totals) {
  return {
    findings: totals.findings,
    modelCalls: totals.modelCalls,
    toolCalls: totals.toolCalls,
    totalTokens: totals.totalTokens,
    providerErrors: providerErrors(totals),
  };
}

function compactFakeModel(fakeModel) {
  return {
    requests: fakeModel.requests,
    queuedMs: fakeModel.queuedMs,
    byRunLabel: Object.fromEntries(
      Object.entries(fakeModel.byRunLabel ?? {}).map(([label, summary]) => [
        label,
        {
          requests: summary.requests,
          queuedMs: summary.queuedMs,
        },
      ]),
    ),
  };
}

function compactTiming(timing) {
  return {
    modeTotals: timing.modeTotals,
    fakeProvider: timing.fakeProvider,
  };
}

function compactAdmission(admission) {
  return {
    shared: compactModeAdmission(admission.shared),
    process: compactModeAdmission(admission.process),
  };
}

function compactHarnessOverhead(harnessOverhead) {
  return {
    shared: compactHarnessOverheadMode(harnessOverhead.shared),
    process: compactHarnessOverheadMode(harnessOverhead.process),
    deltaSharedMinusProcess: harnessOverhead.deltaSharedMinusProcess,
  };
}

function compactHarnessOverheadMode(mode) {
  return {
    cases: mode.cases,
    parentElapsedMs: mode.parentElapsedMs,
    benchmarkElapsedMs: mode.benchmarkElapsedMs,
    reviewElapsedMs: mode.reviewElapsedMs,
    parentMinusBenchmarkMs: mode.parentMinusBenchmarkMs,
    parentMinusReviewMs: mode.parentMinusReviewMs,
    benchmarkMinusReviewMs: mode.benchmarkMinusReviewMs,
  };
}

function compactModeAdmission(admission) {
  return {
    runs: admission.runs,
    maxActiveRuns: admission.maxActiveRuns,
    startWindowMs: admission.startWindowMs,
    finishWindowMs: admission.finishWindowMs,
    elapsedMs: admission.elapsedMs,
  };
}

function assertQueuedProvider(probeName, mode, summary) {
  if (!summary) fail(`${probeName} ${mode} fake-model phase metrics missing`);
  assertGreaterThan(`${probeName} ${mode} fake-model requests`, summary.requests, 0);
  assertGreaterThan(`${probeName} ${mode} fake-model queued max ms`, summary.queuedMs.max, 0);
  assertGreaterThan(`${probeName} ${mode} fake-model queued p95 ms`, summary.queuedMs.p95, 0);
}

function assertCompletionMaxToolCalls(probeName, mode, observedRuns, expected) {
  assertEqual(
    `${probeName} ${mode} completion max tool calls min`,
    observedRuns.completionMaxToolCalls.min,
    expected,
  );
  assertEqual(
    `${probeName} ${mode} completion max tool calls max`,
    observedRuns.completionMaxToolCalls.max,
    expected,
  );
}

function compactIsolation(isolation) {
  return {
    shared: compactModeIsolation(isolation.shared),
    process: compactModeIsolation(isolation.process),
  };
}

function compactModeIsolation(isolation) {
  return {
    cases: isolation.cases,
    duplicateRunIds: isolation.duplicateRunIds,
    orphanFrames: isolation.orphanFrames,
    missingFrameFiles: isolation.missingFrameFiles,
    unexpectedFrameRunIds: isolation.unexpectedFrameRunIds,
    missingTraceFiles: isolation.missingTraceFiles,
    unexpectedTraceRunIds: isolation.unexpectedTraceRunIds,
  };
}

function providerErrors(totals) {
  return (
    totals.modelTimeoutErrors +
    totals.modelRetryableProviderErrors +
    totals.modelNonRetryableProviderErrors
  );
}

function assertEqual(label, actual, expected) {
  if (actual !== expected) {
    fail(`${label}: expected ${expected}, got ${actual}`);
  }
}

function assertGreaterThan(label, actual, threshold) {
  if (!(actual > threshold)) {
    fail(`${label}: expected > ${threshold}, got ${actual}`);
  }
}

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
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

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\..+/, "Z");
}

function usage() {
  process.stderr.write(
    "Usage: check-local.mjs [--runner-path target/release/muzen-runner] [--output-dir bench/results-review-quality/check-local]\n",
  );
}
