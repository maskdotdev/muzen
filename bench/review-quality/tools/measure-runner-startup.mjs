#!/usr/bin/env node

import { spawn } from "node:child_process";
import fs from "node:fs";
import { performance } from "node:perf_hooks";
import path from "node:path";
import { createInterface } from "node:readline";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  usage();
  process.exit(0);
}

const runnerPath = path.resolve(args.runnerPath || "target/release/muzen-runner");
const samples = positiveInt(args.samples || "5", "--samples");
const concurrency = positiveInt(args.concurrency || "1", "--concurrency");
const timeoutMs = positiveInt(args.timeoutMs || "10000", "--timeout-ms");

if (!fs.existsSync(runnerPath)) {
  fail(`runner not found at ${runnerPath}; run: cargo build --release --bin muzen-runner`);
}

const measurements = [];
let nextSample = 1;
await Promise.all(
  Array.from({ length: Math.min(samples, concurrency) }, async () => {
    while (nextSample <= samples) {
      const sample = nextSample;
      nextSample += 1;
      measurements.push(await measureStartup({ runnerPath, sample, timeoutMs }));
    }
  }),
);
measurements.sort((left, right) => left.sample - right.sample);

process.stdout.write(
  `${JSON.stringify(
    {
      schemaVersion: "muzen.runner-startup-measurement.v1",
      generatedAtUtc: new Date().toISOString(),
      runnerPath,
      samples,
      concurrency,
      timeoutMs,
      timing: summarizeMeasurements(measurements),
      measurements,
    },
    null,
    2,
  )}\n`,
);

async function measureStartup({ runnerPath, sample, timeoutMs }) {
  const startedAt = performance.now();
  const child = spawn(runnerPath, ["stdio"], {
    cwd: process.cwd(),
    env: process.env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const spawnReturnedAt = performance.now();
  const stderr = [];
  const frames = [];
  const pending = new Map();
  let firstFrameAt = null;
  let handshakeStartedAt = null;
  let handshakeCompletedAt = null;
  let stdinEndedAt = null;
  let exitedAt = null;
  let timeout = null;

  const exit = new Promise((resolve) => {
    child.on("exit", (code, signal) => {
      exitedAt = performance.now();
      for (const waiter of pending.values()) {
        waiter.reject(new Error(`runner exited before response code=${code} signal=${signal}`));
      }
      pending.clear();
      resolve({ code, signal });
    });
    child.on("error", (error) => {
      exitedAt = performance.now();
      for (const waiter of pending.values()) {
        waiter.reject(error);
      }
      pending.clear();
      resolve({ code: 1, signal: null, error: error.message });
    });
  });

  child.stderr.on("data", (chunk) => stderr.push(chunk.toString("utf8")));
  const readline = createInterface({ input: child.stdout });
  readline.on("line", (line) => {
    if (!line.trim()) return;
    if (firstFrameAt === null) firstFrameAt = performance.now();
    let frame;
    try {
      frame = JSON.parse(line);
    } catch (error) {
      frames.push({
        parseError: error instanceof Error ? error.message : String(error),
        line,
      });
      return;
    }
    frames.push(frame);
    if (frame.id !== undefined && pending.has(String(frame.id))) {
      const waiter = pending.get(String(frame.id));
      pending.delete(String(frame.id));
      if (frame.error) {
        waiter.reject(new Error(JSON.stringify(frame.error)));
      } else {
        waiter.resolve(frame.result);
      }
    }
  });

  const request = (method, params) => {
    const id = 1;
    const promise = new Promise((resolve, reject) => {
      pending.set(String(id), { resolve, reject });
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return promise;
  };

  try {
    const timer = new Promise((_, reject) => {
      timeout = setTimeout(() => {
        child.kill();
        reject(new Error(`runner startup sample ${sample} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
    });
    handshakeStartedAt = performance.now();
    await Promise.race([
      request("runner.handshake", {
        protocolVersion: "muzen.runner.v1",
        clientName: "review-quality-startup-measurement",
      }),
      timer,
    ]);
    handshakeCompletedAt = performance.now();
    child.stdin.end();
    stdinEndedAt = performance.now();
    const exitResult = await Promise.race([exit, timer]);
    return measurementRecord({
      sample,
      ok: exitResult.code === 0,
      exit: exitResult,
      startedAt,
      spawnReturnedAt,
      firstFrameAt,
      handshakeStartedAt,
      handshakeCompletedAt,
      stdinEndedAt,
      exitedAt,
      frames,
      stderr,
    });
  } catch (error) {
    child.kill();
    const exitResult = await exit;
    return measurementRecord({
      sample,
      ok: false,
      exit: exitResult,
      error: error instanceof Error ? error.message : String(error),
      startedAt,
      spawnReturnedAt,
      firstFrameAt,
      handshakeStartedAt,
      handshakeCompletedAt,
      stdinEndedAt,
      exitedAt,
      frames,
      stderr,
    });
  } finally {
    if (timeout) clearTimeout(timeout);
  }
}

function measurementRecord({
  sample,
  ok,
  exit,
  error = null,
  startedAt,
  spawnReturnedAt,
  firstFrameAt,
  handshakeStartedAt,
  handshakeCompletedAt,
  stdinEndedAt,
  exitedAt,
  frames,
  stderr,
}) {
  return {
    sample,
    ok,
    exit,
    error,
    frameCount: frames.length,
    stderrBytes: stderr.join("").length,
    spawnReturnMs: elapsed(spawnReturnedAt, startedAt),
    firstFrameMs: elapsed(firstFrameAt, startedAt),
    handshakeMs: elapsed(handshakeCompletedAt, handshakeStartedAt),
    handshakeCompletedMs: elapsed(handshakeCompletedAt, startedAt),
    exitAfterStdinEndMs: elapsed(exitedAt, stdinEndedAt),
    totalUntilExitMs: elapsed(exitedAt, startedAt),
  };
}

function summarizeMeasurements(measurements) {
  return {
    ok: measurements.filter((measurement) => measurement.ok).length,
    failed: measurements.filter((measurement) => !measurement.ok).length,
    spawnReturnMs: stats(measurements.map((measurement) => measurement.spawnReturnMs)),
    firstFrameMs: stats(measurements.map((measurement) => measurement.firstFrameMs)),
    handshakeMs: stats(measurements.map((measurement) => measurement.handshakeMs)),
    handshakeCompletedMs: stats(
      measurements.map((measurement) => measurement.handshakeCompletedMs),
    ),
    exitAfterStdinEndMs: stats(
      measurements.map((measurement) => measurement.exitAfterStdinEndMs),
    ),
    totalUntilExitMs: stats(measurements.map((measurement) => measurement.totalUntilExitMs)),
  };
}

function stats(values) {
  const clean = values
    .filter((value) => Number.isFinite(value))
    .sort((left, right) => left - right);
  if (clean.length === 0) {
    return { count: 0, min: null, p50: null, p95: null, max: null, mean: null };
  }
  return {
    count: clean.length,
    min: round(clean[0]),
    p50: round(percentile(clean, 0.5)),
    p95: round(percentile(clean, 0.95)),
    max: round(clean.at(-1)),
    mean: round(clean.reduce((total, value) => total + value, 0) / clean.length),
  };
}

function percentile(sortedValues, percentileValue) {
  const index = Math.min(
    sortedValues.length - 1,
    Math.max(0, Math.ceil(sortedValues.length * percentileValue) - 1),
  );
  return sortedValues[index];
}

function elapsed(end, start) {
  if (!Number.isFinite(end) || !Number.isFinite(start)) return null;
  return round(end - start);
}

function round(value) {
  return Math.round(value * 10) / 10;
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

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 1) fail(`${name} must be a positive integer`);
  return parsed;
}

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

function usage() {
  process.stderr.write(
    "Usage: measure-runner-startup.mjs [--runner-path target/release/muzen-runner] [--samples 5] [--concurrency 1] [--timeout-ms 10000]\n",
  );
}
