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
  args.outputDir || `/tmp/muzen-fake-protocol-mixed-pressure-${timestamp()}`,
);
const runnerPath = path.resolve(args.runnerPath || "target/release/muzen-runner");
const iterations = positiveInt(args.iterations || "5", "--iterations");
const cases = positiveInt(args.cases || "6", "--cases");
const concurrency = positiveInt(args.concurrency || "3", "--concurrency");
const sessions = positiveInt(args.sessions || "4", "--sessions");
const maxActiveSessions = positiveInt(
  args.maxActiveSessions || args.maxActive || "2",
  "--max-active-sessions",
);
const maxToolCalls = positiveInt(args.maxToolCalls || "1", "--max-tool-calls");
const maxTurns = positiveInt(args.maxTurns || "5", "--max-turns");
const toolsPerSession = positiveInt(args.toolsPerSession || "2", "--tools-per-session");
const toolCallsPerTurn = positiveInt(args.toolCallsPerTurn || "2", "--tool-calls-per-turn");
const toolDelayMs = nonnegativeInt(args.toolDelayMs || "120", "--tool-delay-ms");
const modelDelayMs = nonnegativeInt(args.modelDelayMs || "20", "--model-delay-ms");
const heartbeatIntervalMs = positiveInt(args.heartbeatIntervalMs || "25", "--heartbeat-interval-ms");
const heartbeatLeaseSeconds = positiveInt(args.heartbeatLeaseSeconds || "1", "--heartbeat-lease-seconds");
const statusPollIntervalMs = positiveInt(args.statusPollIntervalMs || "25", "--status-poll-interval-ms");
const requestCancelAfterStatus = positiveInt(
  args.requestCancelAfterStatus || "1",
  "--request-cancel-after-status",
);
const artifactBytes = positiveInt(args.artifactBytes || "4096", "--artifact-bytes");
const failOnRegression = booleanArg(args.failOnRegression || "true", "--fail-on-regression");

if (!fs.existsSync(runnerPath)) {
  fail(`runner not found at ${runnerPath}; run: cargo build --release --bin muzen-runner`);
}

fs.mkdirSync(outputDir, { recursive: true });
const runs = [];
for (let iteration = 1; iteration <= iterations; iteration += 1) {
  const runDir = path.join(outputDir, `iteration-${String(iteration).padStart(2, "0")}`);
  const result = spawnSync(
    "node",
    [
      "bench/review-quality/tools/run-fake-protocol-session-stress.mjs",
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
      "--max-active-sessions",
      String(maxActiveSessions),
      "--max-tool-calls",
      String(maxToolCalls),
      "--max-turns",
      String(maxTurns),
      "--tools-per-session",
      String(toolsPerSession),
      "--tool-calls-per-turn",
      String(toolCallsPerTurn),
      "--tool-delay-ms",
      String(toolDelayMs),
      "--model-delay-ms",
      String(modelDelayMs),
      "--heartbeat-mode",
      "continue",
      "--heartbeat-interval-ms",
      String(heartbeatIntervalMs),
      "--heartbeat-lease-seconds",
      String(heartbeatLeaseSeconds),
      "--status-poll-interval-ms",
      String(statusPollIntervalMs),
      "--request-cancel-mode",
      "cancel-first",
      "--request-cancel-after-status",
      String(requestCancelAfterStatus),
      "--artifact-bytes",
      String(artifactBytes),
      "--fail-on-regression",
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
      `mixed pressure iteration ${iteration} failed status=${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    );
  }
  let summary;
  try {
    summary = JSON.parse(result.stdout);
  } catch (error) {
    fail(
      `mixed pressure iteration ${iteration} emitted invalid JSON: ${error.message}\nstdout:\n${result.stdout}`,
    );
  }
  runs.push(compactRun(summary, { iteration, outputDir: runDir }));
}

const report = {
  schemaVersion: "muzen.fake-protocol-mixed-pressure-sweep.v1",
  generatedAtUtc: new Date().toISOString(),
  outputDir,
  runnerPath,
  config: {
    iterations,
    cases,
    concurrency,
    sessions,
    maxActiveSessions,
    maxToolCalls,
    maxTurns,
    toolsPerSession,
    toolCallsPerTurn,
    toolDelayMs,
    modelDelayMs,
    heartbeatIntervalMs,
    heartbeatLeaseSeconds,
    statusPollIntervalMs,
    requestCancelAfterStatus,
    artifactBytes,
    failOnRegression,
  },
  regressions: summarizeRegressions(runs),
  aggregate: aggregateRuns(runs),
  runs,
};

fs.writeFileSync(path.join(outputDir, "mixed-pressure-sweep-summary.json"), `${JSON.stringify(report, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
if (failOnRegression && hasBlockingRegressions(report.regressions)) {
  fail(`fake protocol mixed pressure sweep found regressions; see ${path.join(outputDir, "mixed-pressure-sweep-summary.json")}`);
}

function compactRun(summary, { iteration, outputDir }) {
  return {
    iteration,
    outputDir,
    regressions: summary.regressions,
    shared: compactMode(summary.shared),
    process: compactMode(summary.process),
    parity: summary.parity,
  };
}

function compactMode(mode) {
  return {
    statuses: mode.statuses,
    completedRuns: {
      count: mode.completedRuns?.count ?? null,
      toolCalls: mode.completedRuns?.toolCalls ?? null,
      diagnosticExhaustedSessions: mode.completedRuns?.diagnosticExhaustedSessions ?? null,
      budgetRejectedToolCalls: mode.completedRuns?.budgetRejectedToolCalls ?? null,
      sessionOutputs: mode.completedRuns?.sessionOutputs ?? null,
    },
    storedResultRetrieved: mode.storedResultRetrieved,
    storedResultSessionOutputs: mode.storedResultSessionOutputs,
    cancelRequests: mode.cancelRequests,
    heartbeatCallbacks: mode.callbacks?.byMethod?.["run.heartbeat"] ?? 0,
    runningStatusPolls: mode.runningStatusPolls,
    callbackRunIdMismatches: mode.callbacks?.runIdMismatches ?? null,
    callbackErrors: mode.callbacks?.errors ?? null,
    notificationUnexpectedRunIds: mode.notifications?.unexpectedRunIds ?? null,
    frameUnexpectedRunIds: mode.frames?.unexpectedRunIds ?? null,
    stderrBytes: mode.stderrBytes,
  };
}

function summarizeRegressions(runs) {
  const byBucket = {};
  for (const run of runs) {
    for (const [bucket, entries] of Object.entries(run.regressions ?? {})) {
      if (!Array.isArray(entries) || entries.length === 0) continue;
      byBucket[bucket] ||= [];
      byBucket[bucket].push({ iteration: run.iteration, entries });
    }
  }
  return byBucket;
}

function aggregateRuns(runs) {
  return {
    iterations: runs.length,
    sharedStatuses: countStatusObjects(runs.map((run) => run.shared.statuses)),
    processStatuses: countStatusObjects(runs.map((run) => run.process.statuses)),
    sharedHeartbeatCallbacks: stats(runs.map((run) => run.shared.heartbeatCallbacks)),
    processHeartbeatCallbacks: stats(runs.map((run) => run.process.heartbeatCallbacks)),
    sharedRunningStatusPollsMin: stats(runs.map((run) => run.shared.runningStatusPolls?.min)),
    processRunningStatusPollsMin: stats(runs.map((run) => run.process.runningStatusPolls?.min)),
    sharedCompletedRunToolCallsMin: stats(
      runs.map((run) => run.shared.completedRuns.toolCalls?.min),
    ),
    processCompletedRunToolCallsMin: stats(
      runs.map((run) => run.process.completedRuns.toolCalls?.min),
    ),
    sharedCompletedRunBudgetRejectedMin: stats(
      runs.map((run) => run.shared.completedRuns.budgetRejectedToolCalls?.min),
    ),
    processCompletedRunBudgetRejectedMin: stats(
      runs.map((run) => run.process.completedRuns.budgetRejectedToolCalls?.min),
    ),
  };
}

function hasBlockingRegressions(regressions) {
  return Object.values(regressions).some((entries) => Array.isArray(entries) && entries.length > 0);
}

function countStatusObjects(statuses) {
  const counts = {};
  for (const status of statuses) {
    for (const [name, count] of Object.entries(status ?? {})) {
      counts[name] = (counts[name] ?? 0) + count;
    }
  }
  return counts;
}

function stats(values) {
  const clean = values.filter(Number.isFinite).sort((left, right) => left - right);
  if (clean.length === 0) return { count: 0, min: null, p50: null, p95: null, max: null, mean: null };
  return {
    count: clean.length,
    min: clean[0],
    p50: percentile(clean, 0.5),
    p95: percentile(clean, 0.95),
    max: clean.at(-1),
    mean: clean.reduce((total, value) => total + value, 0) / clean.length,
  };
}

function percentile(sorted, p) {
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil(sorted.length * p) - 1))];
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

function positiveInt(value, label) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1) fail(`${label} must be a positive integer`);
  return parsed;
}

function nonnegativeInt(value, label) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 0) fail(`${label} must be a non-negative integer`);
  return parsed;
}

function booleanArg(value, label) {
  if (value === true || value === "true") return true;
  if (value === false || value === "false") return false;
  fail(`${label} must be true or false`);
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
    "Usage: run-fake-protocol-mixed-pressure-sweep.mjs [--runner-path target/release/muzen-runner] [--output-dir /tmp/sweep] [--iterations 5] [--cases 6] [--concurrency 3] [--sessions 4] [--max-active-sessions 2] [--max-tool-calls 1] [--max-turns 5] [--tools-per-session 2] [--tool-calls-per-turn 2] [--tool-delay-ms 120] [--model-delay-ms 20] [--heartbeat-interval-ms 25] [--status-poll-interval-ms 25] [--request-cancel-after-status 1] [--artifact-bytes 4096] [--fail-on-regression true|false]\n",
  );
}
