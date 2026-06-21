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
const includeCodexProxy = booleanArg(args.includeCodexProxy || "false", "--include-codex-proxy");
const includeProtocolPressure = booleanArg(
  args.includeProtocolPressure || "false",
  "--include-protocol-pressure",
);
const protocolPressureIterations = positiveInt(
  args.protocolPressureIterations || "2",
  "--protocol-pressure-iterations",
);
const protocolPressureFixtureExtraLines = nonnegativeInt(
  args.protocolPressureFixtureExtraLines || "400",
  "--protocol-pressure-fixture-extra-lines",
);
const protocolPressureFixtureLineBytes = positiveInt(
  args.protocolPressureFixtureLineBytes || "160",
  "--protocol-pressure-fixture-line-bytes",
);
const startup = runStartupProbe({
  runnerPath,
  samples: args.startupSamples || args.concurrency || "5",
  concurrency: args.startupConcurrency || args.concurrency || "5",
  timeoutMs: args.startupTimeoutMs || "10000",
});
assertStartupProbe(startup);

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
    sessions: "0",
    toolsBeforeFinal: "1",
    finalMode: "candidate",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 5,
    expectFinalOutputRepairAttempts: 0,
    expectFinalOutputCount: 2,
    expectCandidateLifecycle: {
      emitted: 1,
      validationsStarted: 1,
      validationsCompleted: 1,
      decisions: 1,
      accepted: 1,
      rejected: 0,
      publicationSkipped: 0,
      publicationSkippedBudgetExhausted: 0,
    },
  },
  {
    name: "schema-repair-per-conversation",
    sessions: "0",
    toolsBeforeFinal: "1",
    invalidFinalAttempts: "1",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 0,
    expectInvalidFinalsPerConversation: true,
    expectFinalOutputRepairAttempts: 1,
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
    expectProviderTiming: {
      minProviderRequestMs: 1,
      expectedRetryBackoffMs: 0,
      maxProviderRequestDeltaMs: 500,
      maxRetryBackoffDeltaMs: 0,
      maxFakeQueueMeanDeltaMs: 25,
      maxFakeQueueP95DeltaMs: 50,
    },
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

if (includeCodexProxy) {
  probes.push({
    name: "codex-proxy-deterministic-retry",
    toolsBeforeFinal: "1",
    finalMode: "candidate",
    cases: "5",
    concurrency: "5",
    sessions: "0",
    maxConcurrent: "1",
    latencyMs: "25",
    jitterMs: "0",
    viaCodexProxy: "true",
    httpErrorAttemptsPerRequest: "1",
    expectSharedOnlyExhaustion: 0,
    expectExhausted: 0,
    expectFindings: 5,
    expectProviderErrorsGreaterThan: 0,
    expectFakeHttpErrors: true,
    expectProviderQueue: true,
    expectProviderTiming: {
      minProviderRequestMs: 1,
      minRetryBackoffMs: 1000,
      maxProviderRequestDeltaMs: 1500,
      maxRetryBackoffDeltaMs: 1500,
      maxFakeQueueMeanDeltaMs: 50,
      maxFakeQueueP95DeltaMs: 100,
    },
    expectConcurrentAdmission: true,
  });
}

const results = [];
for (const probe of probes) {
  const outputDir = path.join(outputRoot, probe.name);
  const summary = runProbe({
    runnerPath,
    outputDir,
    toolsBeforeFinal: probe.toolsBeforeFinal,
    invalidFinalAttempts: probe.invalidFinalAttempts || "0",
    httpErrorAttemptsPerRequest: probe.httpErrorAttemptsPerRequest || "0",
    viaCodexProxy: probe.viaCodexProxy || "false",
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
const protocolPressure = includeProtocolPressure
  ? runProtocolPressureSweep({
      runnerPath,
      outputDir: path.join(outputRoot, "protocol-mixed-pressure"),
      iterations: protocolPressureIterations,
      fixtureExtraLines: protocolPressureFixtureExtraLines,
      fixtureLineBytes: protocolPressureFixtureLineBytes,
    })
  : null;

process.stdout.write(
  `${JSON.stringify(
    {
      schemaVersion: "muzen.review-quality-check-local.v1",
      generatedAtUtc: new Date().toISOString(),
      outputRoot,
      runnerPath,
      config: {
        includeCodexProxy,
        includeProtocolPressure,
        protocolPressureIterations,
        protocolPressureFixtureExtraLines,
        protocolPressureFixtureLineBytes,
      },
      startup,
      probes: results,
      protocolPressure,
    },
    null,
    2,
  )}\n`,
);

function runProtocolPressureSweep({
  runnerPath,
  outputDir,
  iterations,
  fixtureExtraLines,
  fixtureLineBytes,
}) {
  const result = spawnSync(
    "node",
    [
      "bench/review-quality/tools/run-fake-protocol-mixed-pressure-sweep.mjs",
      "--runner-path",
      runnerPath,
      "--output-dir",
      outputDir,
      "--iterations",
      String(iterations),
      "--fixture-extra-lines",
      String(fixtureExtraLines),
      "--fixture-line-bytes",
      String(fixtureLineBytes),
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
      `protocol pressure sweep failed with status ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  let summary;
  try {
    summary = JSON.parse(result.stdout);
  } catch (error) {
    fail(`protocol pressure sweep did not emit JSON: ${error.message}\nstdout:\n${result.stdout}`);
  }
  if (Object.keys(summary.regressions ?? {}).length > 0) {
    fail(
      `protocol pressure sweep found regressions; see ${path.join(outputDir, "mixed-pressure-sweep-summary.json")}\n${JSON.stringify(summary.regressions, null, 2)}`,
    );
  }
  return {
    outputDir,
    schemaVersion: summary.schemaVersion,
    config: summary.config,
    aggregate: summary.aggregate,
    regressions: summary.regressions,
  };
}

function runStartupProbe({ runnerPath, samples, concurrency, timeoutMs }) {
  const result = spawnSync(
    "node",
    [
      "bench/review-quality/tools/measure-runner-startup.mjs",
      "--runner-path",
      runnerPath,
      "--samples",
      samples,
      "--concurrency",
      concurrency,
      "--timeout-ms",
      timeoutMs,
    ],
    {
      cwd: process.cwd(),
      env: process.env,
      encoding: "utf8",
      maxBuffer: 1024 * 1024 * 16,
    },
  );
  if (result.status !== 0) {
    fail(
      `runner startup probe failed with status ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  try {
    return JSON.parse(result.stdout);
  } catch (error) {
    fail(`runner startup probe did not emit JSON: ${error.message}\nstdout:\n${result.stdout}`);
  }
}

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
  httpErrorAttemptsPerRequest,
  viaCodexProxy,
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
      "--http-error-attempts-per-request",
      httpErrorAttemptsPerRequest,
      "--via-codex-proxy",
      viaCodexProxy,
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

function assertStartupProbe(startup) {
  assertEqual("runner startup failed samples", startup.timing.failed, 0);
  assertGreaterThan("runner startup ok samples", startup.timing.ok, 0);
  assertGreaterThan(
    "runner startup first-frame samples",
    startup.timing.firstFrameMs.count,
    0,
  );
  assertGreaterThan(
    "runner startup handshake samples",
    startup.timing.handshakeMs.count,
    0,
  );
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
    `${probe.name} provider error parity`,
    providerErrors(summary.totals.shared),
    providerErrors(summary.totals.process),
  );
  if (probe.expectProviderErrorsGreaterThan != null) {
    assertGreaterThan(
      `${probe.name} provider errors`,
      providerErrors(summary.totals.shared),
      probe.expectProviderErrorsGreaterThan,
    );
  } else {
    assertEqual(`${probe.name} shared provider errors`, providerErrors(summary.totals.shared), 0);
    assertEqual(`${probe.name} process provider errors`, providerErrors(summary.totals.process), 0);
  }
  assertEqual(`${probe.name} shared release errors`, summary.release.shared.releaseErrors, 0);
  assertEqual(`${probe.name} shared failed finishes`, summary.release.shared.failedFinishes, 0);
  assertEqual(`${probe.name} process release errors`, summary.release.process.releaseErrors, 0);
  assertEqual(`${probe.name} process failed finishes`, summary.release.process.failedFinishes, 0);
  assertIsolation(probe.name, "shared", summary.isolation.shared, {
    allowedFrameMissingRunIds: 0,
  });
  assertIsolation(probe.name, "process", summary.isolation.process, {
    allowedFrameMissingRunIds: summary.isolation.process.cases,
  });
  if (probe.expectInvalidFinalsPerConversation) {
    assertInvalidFinalsPerMode(
      probe.name,
      "shared",
      summary.fakeModel.byRunLabel.shared,
      summary.config.caseCount,
    );
    assertInvalidFinalsPerMode(
      probe.name,
      "process",
      summary.fakeModel.byRunLabel.process,
      summary.config.caseCount,
    );
    assertEqual(
      `${probe.name} total invalid finals`,
      summary.fakeModel.decisions.invalid_final_text ?? 0,
      summary.config.caseCount * 2,
    );
  }
  if (probe.expectFinalOutputRepairAttempts != null) {
    assertFinalOutputRepairDiagnostics(
      probe.name,
      "shared",
      summary.observedRuns.shared,
      summary.config.caseCount,
      probe.expectFinalOutputRepairAttempts,
      probe.expectFinalOutputCount ?? 1,
    );
    assertFinalOutputRepairDiagnostics(
      probe.name,
      "process",
      summary.observedRuns.process,
      summary.config.caseCount,
      probe.expectFinalOutputRepairAttempts,
      probe.expectFinalOutputCount ?? 1,
    );
  }
  if (probe.expectCandidateLifecycle) {
    assertCandidateLifecycle(
      probe.name,
      "shared",
      summary.observedRuns.shared,
      summary.config.caseCount,
      probe.expectCandidateLifecycle,
    );
    assertCandidateLifecycle(
      probe.name,
      "process",
      summary.observedRuns.process,
      summary.config.caseCount,
      probe.expectCandidateLifecycle,
    );
  }
  if (probe.expectProviderQueue) {
    assertQueuedProvider(probe.name, "shared", summary.fakeModel.byRunLabel.shared);
    assertQueuedProvider(probe.name, "process", summary.fakeModel.byRunLabel.process);
  }
  if (probe.expectProviderTiming) {
    assertProviderTiming(probe.name, summary.timing, probe.expectProviderTiming);
  }
  if (probe.expectFakeHttpErrors) {
    const sharedErrors =
      summary.fakeModel.byRunLabel.shared?.decisions?.configured_http_error ?? 0;
    const processErrors =
      summary.fakeModel.byRunLabel.process?.decisions?.configured_http_error ?? 0;
    assertEqual(`${probe.name} fake HTTP error parity`, sharedErrors, processErrors);
    assertGreaterThan(`${probe.name} fake HTTP errors`, sharedErrors, 0);
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

function assertIsolation(probeName, mode, isolation, { allowedFrameMissingRunIds }) {
  assertEqual(`${probeName} ${mode} duplicate runIds`, isolation.duplicateRunIds, 0);
  assertEqual(`${probeName} ${mode} orphan frames`, isolation.orphanFrames, 0);
  assertEqual(`${probeName} ${mode} missing frame files`, isolation.missingFrameFiles, 0);
  assertAtMost(
    `${probeName} ${mode} frame missing runIds`,
    isolation.frameMissingRunIds,
    allowedFrameMissingRunIds,
  );
  assertEqual(`${probeName} ${mode} unexpected frame runIds`, isolation.unexpectedFrameRunIds, 0);
  assertEqual(`${probeName} ${mode} missing trace files`, isolation.missingTraceFiles, 0);
  assertEqual(`${probeName} ${mode} trace missing runIds`, isolation.traceMissingRunIds, 0);
  assertEqual(`${probeName} ${mode} unexpected trace runIds`, isolation.unexpectedTraceRunIds, 0);
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
    conversationCount: fakeModel.conversationCount,
    decisions: fakeModel.decisions,
    invalidFinalsByConversation: fakeModel.invalidFinalsByConversation,
    statuses: fakeModel.statuses,
    queuedMs: fakeModel.queuedMs,
    byRunLabel: Object.fromEntries(
      Object.entries(fakeModel.byRunLabel ?? {}).map(([label, summary]) => [
        label,
        {
          requests: summary.requests,
          conversationCount: summary.conversationCount,
          decisions: summary.decisions,
          invalidFinalsByConversation: summary.invalidFinalsByConversation,
          statuses: summary.statuses,
          queuedMs: summary.queuedMs,
        },
      ]),
    ),
  };
}

function assertInvalidFinalsPerMode(probeName, mode, summary, caseCount) {
  if (!summary) fail(`${probeName} ${mode} fake-model phase metrics missing`);
  const invalidFinalsByConversation = Object.values(
    summary.invalidFinalsByConversation ?? {},
  );
  assertEqual(
    `${probeName} ${mode} invalid-final conversation count`,
    invalidFinalsByConversation.length,
    caseCount,
  );
  assertEqual(
    `${probeName} ${mode} invalid finals`,
    summary.decisions.invalid_final_text ?? 0,
    caseCount,
  );
  for (const [index, count] of invalidFinalsByConversation.entries()) {
    assertEqual(`${probeName} ${mode} invalid finals conversation ${index + 1}`, count, 1);
  }
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
    jobBuildElapsedMs: mode.jobBuildElapsedMs,
    jobBuildChangedFilesMs: mode.jobBuildChangedFilesMs,
    jobBuildInlineDiffMs: mode.jobBuildInlineDiffMs,
    jobBuildRunStartBuildMs: mode.jobBuildRunStartBuildMs,
    runnerInvocationElapsedMs: mode.runnerInvocationElapsedMs,
    runnerInvocationFirstFrameMs: mode.runnerInvocationFirstFrameMs,
    runnerInvocationHandshakeMs: mode.runnerInvocationHandshakeMs,
    runnerInvocationRunStartMs: mode.runnerInvocationRunStartMs,
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

function assertProviderTiming(probeName, timing, expected) {
  const modeTotals = timing.modeTotals ?? {};
  const shared = modeTotals.shared ?? {};
  const process = modeTotals.process ?? {};
  const delta = modeTotals.deltaSharedMinusProcess ?? {};
  assertGreaterThan(
    `${probeName} shared provider request ms`,
    shared.modelProviderRequestMs,
    expected.minProviderRequestMs,
  );
  assertGreaterThan(
    `${probeName} process provider request ms`,
    process.modelProviderRequestMs,
    expected.minProviderRequestMs,
  );
  if (expected.expectedRetryBackoffMs != null) {
    assertEqual(
      `${probeName} shared retry backoff ms`,
      shared.modelRetryBackoffMs,
      expected.expectedRetryBackoffMs,
    );
    assertEqual(
      `${probeName} process retry backoff ms`,
      process.modelRetryBackoffMs,
      expected.expectedRetryBackoffMs,
    );
  }
  if (expected.minRetryBackoffMs != null) {
    assertGreaterThan(
      `${probeName} shared retry backoff ms`,
      shared.modelRetryBackoffMs,
      expected.minRetryBackoffMs,
    );
    assertGreaterThan(
      `${probeName} process retry backoff ms`,
      process.modelRetryBackoffMs,
      expected.minRetryBackoffMs,
    );
  }
  assertAtMostAbs(
    `${probeName} provider request delta ms`,
    delta.modelProviderRequestMs,
    expected.maxProviderRequestDeltaMs,
  );
  assertAtMostAbs(
    `${probeName} retry backoff delta ms`,
    delta.modelRetryBackoffMs,
    expected.maxRetryBackoffDeltaMs,
  );
  const fakeProvider = timing.fakeProvider ?? {};
  assertEqual(
    `${probeName} fake provider request parity`,
    fakeProvider.shared?.requests,
    fakeProvider.process?.requests,
  );
  assertAtMostAbs(
    `${probeName} fake provider queue mean delta ms`,
    fakeProvider.deltaSharedMinusProcess?.queuedMs?.mean,
    expected.maxFakeQueueMeanDeltaMs,
  );
  assertAtMostAbs(
    `${probeName} fake provider queue p95 delta ms`,
    fakeProvider.deltaSharedMinusProcess?.queuedMs?.p95,
    expected.maxFakeQueueP95DeltaMs,
  );
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

function assertFinalOutputRepairDiagnostics(
  probeName,
  mode,
  observedRuns,
  expectedCases,
  expectedRepairAttempts,
  expectedFinalOutputCount,
) {
  const finalOutput = observedRuns.completionFinalOutput;
  assertEqual(
    `${probeName} ${mode} final-output diagnostic cases`,
    finalOutput.count.count,
    expectedCases,
  );
  assertEqual(
    `${probeName} ${mode} final-output attempts min`,
    finalOutput.attempted.min,
    expectedFinalOutputCount,
  );
  assertEqual(
    `${probeName} ${mode} final-output attempts max`,
    finalOutput.attempted.max,
    expectedFinalOutputCount,
  );
  assertEqual(
    `${probeName} ${mode} final-output parse success min`,
    finalOutput.parseSuccess.min,
    expectedFinalOutputCount,
  );
  assertEqual(
    `${probeName} ${mode} final-output parse success max`,
    finalOutput.parseSuccess.max,
    expectedFinalOutputCount,
  );
  assertEqual(
    `${probeName} ${mode} final-output schema success min`,
    finalOutput.schemaValidationSuccess.min,
    expectedFinalOutputCount,
  );
  assertEqual(
    `${probeName} ${mode} final-output schema success max`,
    finalOutput.schemaValidationSuccess.max,
    expectedFinalOutputCount,
  );
  assertEqual(
    `${probeName} ${mode} final-output accepted min`,
    finalOutput.accepted.min,
    expectedFinalOutputCount,
  );
  assertEqual(
    `${probeName} ${mode} final-output accepted max`,
    finalOutput.accepted.max,
    expectedFinalOutputCount,
  );
  assertEqual(`${probeName} ${mode} final-output rejected min`, finalOutput.rejected.min, 0);
  assertEqual(`${probeName} ${mode} final-output rejected max`, finalOutput.rejected.max, 0);
  assertEqual(
    `${probeName} ${mode} final-output repair attempts min`,
    finalOutput.repairAttemptCount.min,
    expectedRepairAttempts,
  );
  assertEqual(
    `${probeName} ${mode} final-output repair attempts max`,
    finalOutput.repairAttemptCount.max,
    expectedRepairAttempts,
  );
  assertEqual(
    `${probeName} ${mode} final-output max repair attempts min`,
    finalOutput.maxRepairAttemptCount.min,
    expectedRepairAttempts,
  );
  assertEqual(
    `${probeName} ${mode} final-output max repair attempts max`,
    finalOutput.maxRepairAttemptCount.max,
    expectedRepairAttempts,
  );
}

function assertCandidateLifecycle(probeName, mode, observedRuns, expectedCases, expected) {
  const lifecycle = observedRuns.candidateLifecycle;
  for (const [field, expectedValue] of Object.entries(expected)) {
    const stats = lifecycle[field];
    assertEqual(`${probeName} ${mode} candidate ${field} cases`, stats.count, expectedCases);
    assertEqual(`${probeName} ${mode} candidate ${field} min`, stats.min, expectedValue);
    assertEqual(`${probeName} ${mode} candidate ${field} max`, stats.max, expectedValue);
  }
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
    frameMissingRunIds: isolation.frameMissingRunIds,
    unexpectedFrameRunIds: isolation.unexpectedFrameRunIds,
    missingTraceFiles: isolation.missingTraceFiles,
    traceMissingRunIds: isolation.traceMissingRunIds,
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

function assertAtMost(label, actual, threshold) {
  if (!(actual <= threshold)) {
    fail(`${label}: expected <= ${threshold}, got ${actual}`);
  }
}

function assertAtMostAbs(label, actual, threshold) {
  if (!Number.isFinite(actual) || Math.abs(actual) > threshold) {
    fail(`${label}: expected absolute value <= ${threshold}, got ${actual}`);
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

function booleanArg(value, name) {
  if (value === true || value === "true") return true;
  if (value === false || value === "false") return false;
  fail(`${name} must be true or false`);
}

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1) fail(`${name} must be a positive integer`);
  return parsed;
}

function nonnegativeInt(value, name) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 0) fail(`${name} must be a non-negative integer`);
  return parsed;
}

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\..+/, "Z");
}

function usage() {
  process.stderr.write(
    "Usage: check-local.mjs [--runner-path target/release/muzen-runner] [--output-dir bench/results-review-quality/check-local] [--startup-samples 5] [--startup-concurrency 5] [--startup-timeout-ms 10000] [--include-codex-proxy true|false] [--include-protocol-pressure true|false] [--protocol-pressure-iterations 2] [--protocol-pressure-fixture-extra-lines 400] [--protocol-pressure-fixture-line-bytes 160]\n",
  );
}
