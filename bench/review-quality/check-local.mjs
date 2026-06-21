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
    cases: args.cases || "5",
    concurrency: args.concurrency || "5",
    maxToolCalls: args.maxToolCalls || "6",
    maxTurns: args.maxTurns || "10",
    latencyMs: args.latencyMs || "5",
    jitterMs: args.jitterMs || "10",
    maxConcurrent: args.maxConcurrent || "64",
  });
  assertProbe(probe, summary);
  results.push({
    name: probe.name,
    outputDir,
    shared: compactTotals(summary.totals.shared),
    process: compactTotals(summary.totals.process),
    exhaustedMaxToolCalls: summary.exhaustedMaxToolCalls,
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
