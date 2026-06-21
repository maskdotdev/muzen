#!/usr/bin/env node

import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { createInterface } from "node:readline";
import {
  buildProductionReviewJob,
  buildProductionReviewReport,
  writeProductionReviewReport,
} from "../run-production-review.mjs";

class SharedRunnerClient {
  constructor(runnerPath, { stderrPath, metricsPath, progress = "true" } = {}) {
    this.runnerPath = runnerPath;
    this.stderrPath = stderrPath;
    this.metricsPath = metricsPath;
    this.progress = progress === "1" || progress === "true" || progress === true;
    this.child = null;
    this.nextId = 1;
    this.pending = new Map();
    this.activeRuns = new Map();
    this.stderr = "";
    this.exit = null;
  }

  async start() {
    this.child = spawn(this.runnerPath, ["stdio"], {
      cwd: process.cwd(),
      env: process.env,
      stdio: ["pipe", "pipe", "pipe"],
    });
    const stdout = createInterface({ input: this.child.stdout });
    stdout.on("line", (line) => this.handleLine(line));
    this.child.stderr.on("data", (chunk) => {
      const text = chunk.toString("utf8");
      this.stderr += text;
      if (this.stderrPath) fs.appendFileSync(this.stderrPath, text);
      if (this.progress && text.trim()) {
        process.stderr.write(`[muzen-concurrent:runner] ${trim(text)}\n`);
      }
    });
    this.exit = new Promise((resolve) => {
      this.child.on("exit", (code, signal) => {
        const exit = { code, signal };
        for (const pending of this.pending.values()) {
          pending.reject(new Error(`shared runner exited code=${code} signal=${signal}`));
        }
        this.pending.clear();
        resolve(exit);
      });
      this.child.on("error", (error) => {
        for (const pending of this.pending.values()) {
          pending.reject(error);
        }
        this.pending.clear();
        resolve({ code: 1, signal: null, error });
      });
    });
    await this.request("runner.handshake", {
      protocolVersion: "muzen.runner.v1",
      clientName: "review-quality-benchmark-shared",
    });
  }

  async run(runStart, { framesPath, name }) {
    const runId = runStart.runId || "muzen-run";
    this.activeRuns.set(runId, { framesPath, name });
    const stderrStart = this.stderr.length;
    try {
      const result = await this.request("run.start", runStart, { runId });
      return {
        ok: true,
        exitCode: 0,
        result,
        stderr: this.stderr.slice(stderrStart),
      };
    } catch (error) {
      return {
        ok: false,
        exitCode: 1,
        result: null,
        stderr: this.stderr.slice(stderrStart),
        error: error instanceof Error ? error.message : String(error),
      };
    } finally {
      this.activeRuns.delete(runId);
    }
  }

  request(method, params, { runId = null } = {}) {
    if (!this.child || this.child.exitCode !== null) {
      return Promise.reject(new Error("shared runner is not running"));
    }
    const id = this.nextId++;
    const promise = new Promise((resolve, reject) => {
      this.pending.set(String(id), { resolve, reject, runId });
    });
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return promise;
  }

  handleLine(line) {
    if (!line.trim()) return;
    let frame;
    try {
      frame = JSON.parse(line);
    } catch (error) {
      if (this.stderrPath) {
        fs.appendFileSync(this.stderrPath, `failed to parse runner frame: ${error.message}\n${line}\n`);
      }
      return;
    }
    const responsePending =
      frame.id !== undefined && frame.id !== null ? this.pending.get(String(frame.id)) : null;
    const runId = frameRunId(frame) || responsePending?.runId || null;
    const active = runId ? this.activeRuns.get(runId) : null;
    if (active?.framesPath) {
      fs.appendFileSync(active.framesPath, `${JSON.stringify(frame)}\n`);
    } else if (runId && this.metricsPath) {
      appendJsonl(this.metricsPath, {
        event: "orphan_frame",
        atUtc: new Date().toISOString(),
        runId,
        method: frame.method ?? null,
        id: frame.id ?? null,
        hasResult: Boolean(frame.result),
        hasError: Boolean(frame.error),
      });
    }
    if (responsePending) {
      this.pending.delete(String(frame.id));
      if (frame.error) {
        responsePending.reject(new Error(`${frame.error.message}: ${JSON.stringify(frame.error.data ?? {})}`));
      } else {
        responsePending.resolve(frame.result);
      }
    }
  }

  async close() {
    if (!this.child) return;
    if (this.child.exitCode !== null) {
      await this.exit;
      return;
    }
    this.child.stdin.end();
    const timeout = new Promise((resolve) => {
      setTimeout(() => {
        if (this.child.exitCode === null) this.child.kill();
        resolve({ code: null, signal: "timeout" });
      }, 5000).unref();
    });
    await Promise.race([this.exit, timeout]);
  }
}

const DEFAULT_CASE_SOURCE =
  "/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-50-retry/summary.json";
const DEFAULT_GOLDEN_DIR =
  "/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-direct/goldens";
const DEFAULT_WORKTREE_ROOT = "/tmp/muzen-hagent-martian-worktrees";
const SHARED_RUNNER_START_TIMEOUT_MS = 30_000;
const DEFAULT_OUTPUT_DIR =
  "/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-50-c5";
const DEFAULT_MODEL = process.env.MODEL || "gpt-5.5";
const DEFAULT_SEMANTIC_MODEL =
  process.env.MARTIAN_SEMANTIC_MODEL || process.env.ANTI_CHEAT_MODEL || "gpt-5.4-mini";

const args = parseArgs(process.argv.slice(2));

if (args.help) {
  usage();
  process.exit(0);
}

const outputDir = path.resolve(args.outputDir || DEFAULT_OUTPUT_DIR);
const traceRoot = path.join(outputDir, "traces");
const logsDir = path.join(outputDir, "logs");
const metricsPath = path.join(outputDir, "metrics.jsonl");
const samplesPath = path.join(outputDir, "process-samples.jsonl");
const jobsDir = path.join(outputDir, "jobs");
const semanticDir = path.resolve(
  args.semanticOutputDir ||
    `${outputDir}-semantic-judge-${args.semanticModel || DEFAULT_SEMANTIC_MODEL}`,
);
const model = args.model || DEFAULT_MODEL;
const semanticModel = args.semanticModel || DEFAULT_SEMANTIC_MODEL;
const concurrency = positiveInt(args.concurrency || "5", "--concurrency");
const runnerMode = args.runnerMode || "shared";
if (!["shared", "process"].includes(runnerMode)) {
  throw new Error("--runner-mode must be one of: shared, process");
}
const limit = args.limit ? positiveInt(args.limit, "--limit") : null;
const sampleIntervalMs = positiveInt(args.sampleIntervalMs || "5000", "--sample-interval-ms");
const maxCapturedTextBytes = args.maxCapturedTextBytes
  ? positiveInt(args.maxCapturedTextBytes, "--max-captured-text-bytes")
  : null;
const repeatCases = args.repeatCases ? positiveInt(args.repeatCases, "--repeat-cases") : 1;
const cases = loadCases({
  caseSource: args.caseSource || DEFAULT_CASE_SOURCE,
  goldenDir: args.goldenDir || DEFAULT_GOLDEN_DIR,
  worktreeRoot: args.worktreeRoot || DEFAULT_WORKTREE_ROOT,
  limit,
  repeatCases,
});

fs.mkdirSync(outputDir, { recursive: true });
fs.mkdirSync(traceRoot, { recursive: true });
fs.mkdirSync(logsDir, { recursive: true });
fs.mkdirSync(jobsDir, { recursive: true });
fs.writeFileSync(metricsPath, "");
fs.writeFileSync(samplesPath, "");

const startedAt = Date.now();
const state = {
  active: new Map(),
  completed: [],
  failed: [],
  peakRunnerRssBytes: 0,
  peakHarnessRssBytes: 0,
  peakProcessTreeRssBytes: 0,
  peakAggregateRssBytes: 0,
  harnessPid: process.pid,
  runnerPid: null,
  runnerMode,
};

process.stderr.write(
  `[muzen-concurrent] ${new Date().toISOString()} starting ${cases.length} cases concurrency=${concurrency} runnerMode=${runnerMode} model=${model}\n`,
);

const runnerPath = path.resolve(args.runnerPath || "target/release/muzen-runner");
const runnerClient =
  runnerMode === "shared"
    ? new SharedRunnerClient(runnerPath, {
        stderrPath: path.join(logsDir, "shared-runner.stderr.log"),
        metricsPath,
        progress: args.progress ?? "true",
      })
    : null;
const sampler = setInterval(() => sampleProcesses(state), sampleIntervalMs);
sampler.unref();

try {
  if (runnerClient) {
    try {
      await withTimeout(
        runnerClient.start(),
        SHARED_RUNNER_START_TIMEOUT_MS,
        `shared runner startup timed out after ${SHARED_RUNNER_START_TIMEOUT_MS}ms`,
      );
      state.runnerPid = runnerClient.child?.pid ?? null;
    } catch (error) {
      await runnerClient.close().catch((closeError) => {
        appendJsonl(metricsPath, {
          event: "runner_start_cleanup_error",
          atUtc: new Date().toISOString(),
          error: closeError instanceof Error ? closeError.message : String(closeError),
        });
      });
      throw error;
    }
  }
  await runPool(cases, concurrency, (testCase) =>
    runReviewCase(testCase, {
      outputDir,
      traceRoot,
      logsDir,
      jobsDir,
      model,
      runnerMode,
      runnerPath,
      runnerClient,
      sessions: args.sessions || "1",
      maxActive: args.maxActive || "1",
      maxTurns: args.maxTurns || "60",
      maxToolCalls: args.maxToolCalls || "50",
      maxPromptTokens: args.maxPromptTokens || "64000",
      maxOutputTokens: args.maxOutputTokens || null,
      maxCapturedTextBytes,
      progress: args.progress ?? "true",
      state,
      metricsPath,
    }),
  );
} finally {
  clearInterval(sampler);
  sampleProcesses(state);
  if (runnerClient) await runnerClient.close();
}

const reviewResults = cases.map((testCase) =>
  summarizeReviewCase(testCase, path.join(outputDir, `${testCase.outputName}.json`)),
);
const reviewSummary = buildReviewSummary({
  model,
  cases: reviewResults,
  startedAt,
  completedAt: Date.now(),
  concurrency,
  sampleIntervalMs,
  outputDir,
  traceRoot,
  logsDir,
  metricsPath,
  samplesPath,
  peakAggregateRssBytes: state.peakAggregateRssBytes,
  peakRunnerRssBytes: state.peakRunnerRssBytes,
  peakHarnessRssBytes: state.peakHarnessRssBytes,
  peakProcessTreeRssBytes: state.peakProcessTreeRssBytes,
  runnerMode,
});
fs.writeFileSync(path.join(outputDir, "summary.json"), `${JSON.stringify(reviewSummary, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(reviewSummary, null, 2)}\n`);

if (args.skipSemantic !== "true") {
  fs.mkdirSync(semanticDir, { recursive: true });
  const semanticResults = [];
  for (const result of reviewResults) {
    const semanticOutput = path.join(semanticDir, `${result.name}.semantic.json`);
    const commandArgs = [
      "bench/review-quality/tools/score-martian-semantic.mjs",
      "--result",
      result.outputPath,
      "--golden",
      result.golden,
      "--model",
      semanticModel,
      "--adjudicate-unmatched",
      "--output",
      semanticOutput,
    ];
    process.stderr.write(
      `[muzen-concurrent] ${new Date().toISOString()} semantic ${result.name}\n`,
    );
    const semantic = spawnSyncChecked("node", commandArgs, {
      env: {
        ...process.env,
        MODEL: model,
        MARTIAN_SEMANTIC_MODEL: semanticModel,
      },
    });
    semanticResults.push({
      ...summarizeSemanticCase(result, semanticOutput),
      stderr: semantic.stderr,
    });
  }
  const semanticSummary = buildSemanticSummary({
    model,
    semanticModel,
    outputDir: semanticDir,
    sourceOutputDir: outputDir,
    cases: semanticResults,
  });
  fs.writeFileSync(path.join(semanticDir, "summary.json"), `${JSON.stringify(semanticSummary, null, 2)}\n`);
}

process.exitCode = reviewResults.every((result) => result.reviewValid) ? 0 : 1;

async function runPool(items, width, worker) {
  let cursor = 0;
  const workers = Array.from({ length: Math.min(width, items.length) }, async () => {
    while (cursor < items.length) {
      const index = cursor++;
      await worker(items[index], index);
    }
  });
  await Promise.all(workers);
}

async function runReviewCase(testCase, options) {
  if (options.runnerMode === "process") {
    return await runReviewCaseProcess(testCase, options);
  }
  const outputPath = path.join(options.outputDir, `${testCase.outputName}.json`);
  const traceDir = path.join(options.traceRoot, testCase.outputName);
  const stderrPath = path.join(options.logsDir, `${testCase.outputName}.stderr.log`);
  const stdoutPath = path.join(options.logsDir, `${testCase.outputName}.stdout.log`);
  const startedAt = Date.now();
  const runId = `review-quality-${testCase.outputName}-${startedAt}`;
  const job = buildProductionReviewJob({
    repo: testCase.worktree,
    baseRef: testCase.baseRef,
    runnerPath: options.runnerPath,
    goldenPath: testCase.golden,
    mode: "review",
    sessions: positiveInt(options.sessions, "--sessions"),
    maxActive: positiveInt(options.maxActive, "--max-active"),
    maxTurns: positiveInt(options.maxTurns, "--max-turns"),
    maxToolCalls: positiveInt(options.maxToolCalls, "--max-tool-calls"),
    maxPromptTokens: positiveInt(options.maxPromptTokens, "--max-prompt-tokens"),
    maxOutputTokens: options.maxOutputTokens ? positiveInt(options.maxOutputTokens, "--max-output-tokens") : 8000,
    model: options.model,
    outputPath,
    traceOutputDir: traceDir,
    runId,
    maxCapturedTextBytes: options.maxCapturedTextBytes,
    tempDir: path.join(options.jobsDir, testCase.outputName),
  });
  process.stderr.write(`[muzen-concurrent] ${new Date(startedAt).toISOString()} start ${testCase.name}\n`);
  options.state.active.set(runId, {
    name: testCase.name,
    startedAt,
    peakRssBytes: 0,
    peakRunnerRssBytes: 0,
    peakProcessTreeRssBytes: 0,
    runId,
  });
  appendJsonl(options.metricsPath, {
    event: "start",
    atUtc: new Date(startedAt).toISOString(),
    name: testCase.name,
    runId,
    outputPath,
    framesPath: job.framesPath,
  });

  let run;
  let report = null;
  let error = null;
  try {
    run = await options.runnerClient.run(job.runStart, {
      framesPath: job.framesPath,
      name: testCase.name,
    });
    const elapsedMs = Date.now() - startedAt;
    report = buildProductionReviewReport(job, run, { elapsedMs });
    writeProductionReviewReport(report, outputPath);
    fs.writeFileSync(stdoutPath, `${JSON.stringify(report, null, 2)}\n`);
    fs.writeFileSync(stderrPath, run.stderr || "");
  } catch (caught) {
    error = caught instanceof Error ? caught : new Error(String(caught));
    fs.writeFileSync(stderrPath, `${error.stack || error.message}\n`);
    fs.writeFileSync(stdoutPath, "");
  }

  const active = options.state.active.get(runId);
  options.state.active.delete(runId);
  const completedAt = Date.now();
  const code = error || report?.reviewValid === false ? 1 : 0;
  const record = {
    event: "finish",
    atUtc: new Date(completedAt).toISOString(),
    name: testCase.name,
    runId,
    code,
    signal: null,
    elapsedMs: completedAt - startedAt,
    peakRssBytes: active?.peakRssBytes || 0,
    peakRunnerRssBytes: active?.peakRunnerRssBytes || 0,
    peakProcessTreeRssBytes: active?.peakProcessTreeRssBytes || 0,
    outputPath,
    framesPath: job.framesPath,
    stderrPath,
    stdoutPath,
    error: error ? error.message : report?.error || null,
  };
  appendJsonl(options.metricsPath, record);
  if (code === 0) {
    options.state.completed.push(record);
    process.stderr.write(
      `[muzen-concurrent] ${new Date(completedAt).toISOString()} done ${testCase.name} elapsed=${formatSeconds(record.elapsedMs)}s peakRss=${formatMiB(record.peakRssBytes)}MiB\n`,
    );
  } else {
    options.state.failed.push(record);
    process.stderr.write(
      `[muzen-concurrent] ${new Date(completedAt).toISOString()} failed ${testCase.name} code=${code}\n`,
    );
  }
}

async function runReviewCaseProcess(testCase, options) {
  const outputPath = path.join(options.outputDir, `${testCase.outputName}.json`);
  const traceDir = path.join(options.traceRoot, testCase.outputName);
  const stderrPath = path.join(options.logsDir, `${testCase.outputName}.stderr.log`);
  const stdoutPath = path.join(options.logsDir, `${testCase.outputName}.stdout.log`);
  const startedAt = Date.now();
  const runId = `review-quality-${testCase.outputName}-${startedAt}`;
  const commandArgs = [
    "bench/review-quality/run-production-review.mjs",
    "--repo",
    testCase.worktree,
    "--base-ref",
    testCase.baseRef,
    "--runner-path",
    options.runnerPath,
    "--golden",
    testCase.golden,
    "--mode",
    "review",
    "--run-id",
    runId,
    "--sessions",
    options.sessions,
    "--max-active",
    options.maxActive,
    "--max-turns",
    options.maxTurns,
    "--max-tool-calls",
    options.maxToolCalls,
    "--max-prompt-tokens",
    options.maxPromptTokens,
    "--model",
    options.model,
    "--output",
    outputPath,
    "--trace-output-dir",
    traceDir,
    "--temp-dir",
    path.join(options.jobsDir, testCase.outputName),
    "--progress",
    options.progress,
  ];
  if (options.maxOutputTokens) {
    commandArgs.push("--max-output-tokens", options.maxOutputTokens);
  }
  if (options.maxCapturedTextBytes) {
    commandArgs.push("--max-captured-text-bytes", String(options.maxCapturedTextBytes));
  }

  const stdout = fs.openSync(stdoutPath, "w");
  const stderr = fs.openSync(stderrPath, "w");
  process.stderr.write(`[muzen-concurrent] ${new Date(startedAt).toISOString()} start ${testCase.name}\n`);
  const child = spawn("node", commandArgs, {
    cwd: process.cwd(),
    env: {
      ...process.env,
      MODEL: options.model,
    },
    stdio: ["ignore", stdout, stderr],
  });
  options.state.active.set(runId, {
    name: testCase.name,
    startedAt,
    peakRssBytes: 0,
    peakRunnerRssBytes: 0,
    peakProcessTreeRssBytes: 0,
    runId,
    pid: child.pid,
  });
  appendJsonl(options.metricsPath, {
    event: "start",
    atUtc: new Date(startedAt).toISOString(),
    name: testCase.name,
    runId,
    pid: child.pid,
    outputPath,
  });

  const exit = await new Promise((resolve) => {
    child.on("exit", (code, signal) => resolve({ code, signal }));
    child.on("error", (error) => resolve({ code: 1, signal: null, error }));
  });
  fs.closeSync(stdout);
  fs.closeSync(stderr);

  const active = options.state.active.get(runId);
  options.state.active.delete(runId);
  const completedAt = Date.now();
  const report = fs.existsSync(outputPath) ? readJson(outputPath) : null;
  const code = exit.code || (report?.reviewValid === false ? 1 : 0);
  const record = {
    event: "finish",
    atUtc: new Date(completedAt).toISOString(),
    name: testCase.name,
    runId,
    pid: child.pid,
    code,
    signal: exit.signal,
    elapsedMs: completedAt - startedAt,
    peakRssBytes: active?.peakRssBytes || 0,
    peakRunnerRssBytes: active?.peakRunnerRssBytes || 0,
    peakProcessTreeRssBytes: active?.peakProcessTreeRssBytes || 0,
    outputPath,
    stderrPath,
    stdoutPath,
    error: exit.error ? String(exit.error.message || exit.error) : report?.error || null,
  };
  appendJsonl(options.metricsPath, record);
  if (code === 0) {
    options.state.completed.push(record);
    process.stderr.write(
      `[muzen-concurrent] ${new Date(completedAt).toISOString()} done ${testCase.name} elapsed=${formatSeconds(record.elapsedMs)}s peakRss=${formatMiB(record.peakRssBytes)}MiB\n`,
    );
  } else {
    options.state.failed.push(record);
    process.stderr.write(
      `[muzen-concurrent] ${new Date(completedAt).toISOString()} failed ${testCase.name} code=${code}\n`,
    );
  }
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

function sampleProcesses(state) {
  if (state.active.size === 0) return;
  if (state.runnerMode === "process") {
    const harness = processMetrics([state.harnessPid]);
    const processes = [];
    let aggregateRssBytes = harness.rssBytes;
    let aggregateCpuPercent = harness.cpuPercent;
    for (const active of state.active.values()) {
      const metrics = processTreeMetrics(active.pid);
      active.peakRssBytes = Math.max(active.peakRssBytes, metrics.rssBytes);
      active.peakRunnerRssBytes = Math.max(active.peakRunnerRssBytes, metrics.rssBytes);
      active.peakProcessTreeRssBytes = Math.max(active.peakProcessTreeRssBytes, metrics.rssBytes);
      aggregateRssBytes += metrics.rssBytes;
      aggregateCpuPercent += metrics.cpuPercent;
      processes.push({
        name: active.name,
        pid: active.pid,
        rssBytes: metrics.rssBytes,
        processCount: metrics.processCount,
        cpuPercent: metrics.cpuPercent,
        children: metrics.processes,
      });
    }
    state.peakAggregateRssBytes = Math.max(state.peakAggregateRssBytes, aggregateRssBytes);
    state.peakRunnerRssBytes = Math.max(
      state.peakRunnerRssBytes,
      ...processes.map((process) => process.rssBytes),
      0,
    );
    state.peakHarnessRssBytes = Math.max(state.peakHarnessRssBytes, harness.rssBytes);
    state.peakProcessTreeRssBytes = Math.max(state.peakProcessTreeRssBytes, aggregateRssBytes);
    appendJsonl(samplesPath, {
      atUtc: new Date().toISOString(),
      active: state.active.size,
      aggregateRssBytes,
      runnerRssBytes: processes.reduce((sum, process) => sum + process.rssBytes, 0),
      harnessRssBytes: harness.rssBytes,
      processTreeRssBytes: aggregateRssBytes,
      cpuPercent: aggregateCpuPercent,
      activeReviews: [...state.active.values()].map((active) => active.name),
      processes: [
        {
          name: "harness",
          pid: state.harnessPid,
          rssBytes: harness.rssBytes,
          processCount: harness.processCount,
          cpuPercent: harness.cpuPercent,
          children: harness.processes,
        },
        ...processes,
      ],
    });
    return;
  }
  const runner = state.runnerPid ? processTreeMetrics(state.runnerPid) : emptyProcessMetrics();
  const harness = processMetrics([state.harnessPid]);
  const processTree = processTreeMetrics(state.harnessPid);
  const aggregateRssBytes = processTree.rssBytes;
  for (const active of state.active.values()) {
    active.peakRssBytes = Math.max(active.peakRssBytes, runner.rssBytes);
    active.peakRunnerRssBytes = Math.max(active.peakRunnerRssBytes, runner.rssBytes);
    active.peakProcessTreeRssBytes = Math.max(
      active.peakProcessTreeRssBytes,
      processTree.rssBytes,
    );
  }
  state.peakAggregateRssBytes = Math.max(state.peakAggregateRssBytes, aggregateRssBytes);
  state.peakRunnerRssBytes = Math.max(state.peakRunnerRssBytes, runner.rssBytes);
  state.peakHarnessRssBytes = Math.max(state.peakHarnessRssBytes, harness.rssBytes);
  state.peakProcessTreeRssBytes = Math.max(state.peakProcessTreeRssBytes, processTree.rssBytes);
  appendJsonl(samplesPath, {
    atUtc: new Date().toISOString(),
    active: state.active.size,
    aggregateRssBytes,
    runnerRssBytes: runner.rssBytes,
    harnessRssBytes: harness.rssBytes,
    processTreeRssBytes: processTree.rssBytes,
    activeReviews: [...state.active.values()].map((active) => active.name),
    processes: [
      {
        name: "harness",
        pid: state.harnessPid,
        rssBytes: harness.rssBytes,
        processCount: harness.processCount,
        cpuPercent: harness.cpuPercent,
        children: harness.processes,
      },
      {
        name: "runner",
        pid: state.runnerPid,
        rssBytes: runner.rssBytes,
        processCount: runner.processCount,
        cpuPercent: runner.cpuPercent,
        children: runner.processes,
      },
      {
        name: "process-tree",
        pid: state.harnessPid,
        rssBytes: processTree.rssBytes,
        processCount: processTree.processCount,
        cpuPercent: processTree.cpuPercent,
        children: processTree.processes,
      },
    ],
  });
}

function processTreeMetrics(rootPid) {
  if (!rootPid) return emptyProcessMetrics();
  const pids = collectProcessTree(rootPid);
  return processMetrics(pids);
}

function processMetrics(pids) {
  if (pids.length === 0) {
    return emptyProcessMetrics();
  }
  try {
    const output = execFileSync("ps", ["-o", "pid=,ppid=,rss=,pcpu=,comm=", "-p", pids.join(",")], {
      encoding: "utf8",
    });
    let rssKb = 0;
    let cpuPercent = 0;
    const processes = [];
    for (const line of output.trim().split("\n")) {
      const parts = line.trim().split(/\s+/);
      if (parts.length < 4) continue;
      const rssBytes = (Number(parts[2]) || 0) * 1024;
      const processCpu = Number(parts[3]) || 0;
      rssKb += Number(parts[2]) || 0;
      cpuPercent += processCpu;
      processes.push({
        pid: Number(parts[0]) || null,
        ppid: Number(parts[1]) || null,
        rssBytes,
        cpuPercent: processCpu,
        command: parts.slice(4).join(" "),
      });
    }
    return { rssBytes: rssKb * 1024, cpuPercent, processCount: processes.length, processes };
  } catch {
    return emptyProcessMetrics();
  }
}

function emptyProcessMetrics() {
  return { rssBytes: 0, cpuPercent: 0, processCount: 0, processes: [] };
}

function collectProcessTree(rootPid) {
  const seen = new Set();
  const pending = [String(rootPid)];
  while (pending.length > 0) {
    const pid = pending.shift();
    if (!pid || seen.has(pid)) continue;
    seen.add(pid);
    try {
      const children = execFileSync("pgrep", ["-P", pid], { encoding: "utf8" })
        .trim()
        .split(/\s+/)
        .filter(Boolean);
      pending.push(...children);
    } catch {
      continue;
    }
  }
  return [...seen];
}

function loadCases({ caseSource, goldenDir, worktreeRoot, limit, repeatCases }) {
  const summary = readJson(caseSource);
  const rawCases = Array.isArray(summary.results) ? summary.results : [];
  const selected = limit ? rawCases.slice(0, limit) : rawCases;
  const repeated = [];
  for (let repeatIndex = 0; repeatIndex < repeatCases; repeatIndex += 1) {
    for (const entry of selected) {
      repeated.push({ entry, repeatIndex });
    }
  }
  return repeated.map(({ entry, repeatIndex }) => {
    const parsed = parsePullUrl(entry.prUrl);
    const baseOutputName = `${parsed.owner}-${parsed.repo}-pull-${parsed.number}`;
    const outputName =
      repeatCases === 1 ? baseOutputName : `${baseOutputName}-repeat-${repeatIndex + 1}`;
    const worktree = path.join(worktreeRoot, `${parsed.owner}-${parsed.repo}-pr-${parsed.number}`);
    const golden = path.join(goldenDir, `${baseOutputName}.json`);
    if (!fs.existsSync(worktree)) {
      throw new Error(`missing worktree for ${entry.prUrl}: ${worktree}`);
    }
    if (!fs.existsSync(golden)) {
      throw new Error(`missing golden for ${entry.prUrl}: ${golden}`);
    }
    return {
      name: outputName,
      baseName: baseOutputName,
      repeatIndex: repeatIndex + 1,
      outputName,
      prUrl: entry.prUrl,
      title: entry.title,
      owner: parsed.owner,
      repo: parsed.repo,
      number: parsed.number,
      worktree,
      baseRef: `hagent-martian/pr-${parsed.number}-base`,
      golden,
    };
  });
}

function summarizeReviewCase(testCase, outputPath) {
  if (!fs.existsSync(outputPath)) {
    return {
      ...testCase,
      outputPath,
      reviewValid: false,
      error: "missing_output",
    };
  }
  const result = readJson(outputPath);
  return {
    name: testCase.name,
    prUrl: testCase.prUrl,
    title: testCase.title,
    outputPath,
    traceDir: path.join(traceRoot, testCase.outputName),
    golden: testCase.golden,
    reviewValid: result.reviewValid !== false,
    validationFailures: result.validationFailures || [],
    goldenIssues: result.inputs?.goldenIssueCount || 0,
    findings: result.review?.findings ?? result.findings?.length ?? 0,
    hits: result.benchmark?.hits?.length || 0,
    misses: result.benchmark?.misses?.length || 0,
    falsePositives: result.benchmark?.falsePositiveCount || 0,
    candidateCount: result.benchmark?.candidateCount ?? null,
    modelCalls: result.review?.modelCalls || 0,
    toolCalls: result.review?.toolCalls || 0,
    modelMetrics: result.review?.modelMetrics || {},
    tokens: result.review?.tokens || {},
    elapsedMs: result.review?.elapsedMs ?? result.benchmark?.elapsedMs ?? 0,
    error: result.error || null,
  };
}

function summarizeSemanticCase(reviewResult, outputPath) {
  const semantic = readJson(outputPath);
  const summary = semantic.summary || {};
  return {
    name: reviewResult.name,
    outputPath,
    goldenIssues: summary.goldenIssues || 0,
    candidates: summary.candidates || 0,
    hits: summary.hits || 0,
    misses: summary.misses || 0,
    falsePositives: summary.falsePositives || 0,
    precision: summary.precision ?? null,
    recall: summary.recall ?? null,
    errors: summary.errors || 0,
    unmatchedClassificationCounts: summary.unmatchedClassificationCounts || {},
    actionableUnmatchedCount: summary.actionableUnmatchedCount || 0,
    insufficientUnmatchedCount: summary.insufficientUnmatchedCount || 0,
    unclearUnmatchedCount: summary.unclearUnmatchedCount || 0,
    unadjudicatedUnmatchedCount: summary.unadjudicatedUnmatchedCount || 0,
    rankedSlices: summary.rankedSlices || {},
    rankDiagnostics: summary.rankDiagnostics || {},
  };
}

function buildReviewSummary({
  model,
  cases,
  startedAt,
  completedAt,
  concurrency,
  sampleIntervalMs,
  outputDir,
  traceRoot,
  logsDir,
  metricsPath,
  samplesPath,
  peakAggregateRssBytes,
  peakRunnerRssBytes,
  peakHarnessRssBytes,
  peakProcessTreeRssBytes,
  runnerMode,
}) {
  const totals = {
    goldenIssues: sum(cases, "goldenIssues"),
    findings: sum(cases, "findings"),
    hits: sum(cases, "hits"),
    misses: sum(cases, "misses"),
    falsePositives: sum(cases, "falsePositives"),
    modelCalls: sum(cases, "modelCalls"),
    toolCalls: sum(cases, "toolCalls"),
    modelProviderRequestMs: sumNested(cases, ["modelMetrics", "providerRequestMs"]),
    modelRetryBackoffMs: sumNested(cases, ["modelMetrics", "retryBackoffMs"]),
    modelLimiterWaitMs: sumNested(cases, ["modelMetrics", "limiterWaitMs"]),
    modelTimeoutErrors: sumNested(cases, ["modelMetrics", "timeoutErrors"]),
    modelRetryableProviderErrors: sumNested(cases, ["modelMetrics", "retryableProviderErrors"]),
    modelNonRetryableProviderErrors: sumNested(cases, ["modelMetrics", "nonRetryableProviderErrors"]),
    inputTokens: sumNested(cases, ["tokens", "inputTokens"]),
    outputTokens: sumNested(cases, ["tokens", "outputTokens"]),
    totalTokens: sumNested(cases, ["tokens", "totalTokens"]),
    reviewElapsedMs: sum(cases, "elapsedMs"),
  };
  return {
    schemaVersion: "muzen.martian-concurrent-review.v1",
    generatedAtUtc: new Date(completedAt).toISOString(),
    model,
    runnerMode,
    concurrency,
    sampleIntervalMs,
    caseCount: cases.length,
    validCount: cases.filter((testCase) => testCase.reviewValid).length,
    wallMs: completedAt - startedAt,
    outputDir,
    traceRoot,
    logsDir,
    metricsPath,
    samplesPath,
    peakAggregateRssBytes,
    peakAggregateRssMiB: bytesToMiB(peakAggregateRssBytes),
    peakRunnerRssBytes,
    peakRunnerRssMiB: bytesToMiB(peakRunnerRssBytes),
    peakHarnessRssBytes,
    peakHarnessRssMiB: bytesToMiB(peakHarnessRssBytes),
    peakProcessTreeRssBytes,
    peakProcessTreeRssMiB: bytesToMiB(peakProcessTreeRssBytes),
    totals,
    rawMatcherDiagnostics: {
      hits: totals.hits,
      misses: totals.misses,
      falsePositives: totals.falsePositives,
      note:
        "The local matcher can over-count semantic hits; use the semantic summary for precision and recall.",
    },
    results: cases,
  };
}

function buildSemanticSummary({ model, semanticModel, outputDir, sourceOutputDir, cases }) {
  const totals = {
    goldenIssues: sum(cases, "goldenIssues"),
    candidates: sum(cases, "candidates"),
    hits: sum(cases, "hits"),
    misses: sum(cases, "misses"),
    falsePositives: sum(cases, "falsePositives"),
    errors: sum(cases, "errors"),
    actionableUnmatchedCount: sum(cases, "actionableUnmatchedCount"),
    insufficientUnmatchedCount: sum(cases, "insufficientUnmatchedCount"),
    unclearUnmatchedCount: sum(cases, "unclearUnmatchedCount"),
    unadjudicatedUnmatchedCount: sum(cases, "unadjudicatedUnmatchedCount"),
  };
  return {
    schemaVersion: "muzen.martian-concurrent-semantic.v1",
    generatedAtUtc: new Date().toISOString(),
    model,
    semanticModel,
    outputDir,
    sourceOutputDir,
    caseCount: cases.length,
    totals,
    precision: totals.candidates > 0 ? totals.hits / totals.candidates : 0,
    recall: totals.goldenIssues > 0 ? totals.hits / totals.goldenIssues : 0,
    results: cases,
  };
}

function parsePullUrl(url) {
  const match = String(url).match(/^https:\/\/github\.com\/([^/]+)\/([^/]+)\/pull\/(\d+)$/);
  if (!match) {
    throw new Error(`unsupported PR URL: ${url}`);
  }
  return {
    owner: match[1],
    repo: match[2],
    number: Number(match[3]),
  };
}

function spawnSyncChecked(command, commandArgs, options = {}) {
  const result = execFileSync(command, commandArgs, {
    cwd: process.cwd(),
    env: options.env || process.env,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  return { stdout: result, stderr: "" };
}

function withTimeout(promise, ms, message) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(message)), ms);
    timer.unref();
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function appendJsonl(file, value) {
  fs.appendFileSync(file, `${JSON.stringify(value)}\n`);
}

function sum(items, field) {
  return items.reduce((total, item) => total + Number(item[field] || 0), 0);
}

function sumNested(items, fields) {
  return items.reduce((total, item) => {
    let value = item;
    for (const field of fields) value = value?.[field];
    return total + Number(value || 0);
  }, 0);
}

function bytesToMiB(bytes) {
  return Math.round((bytes / 1024 / 1024) * 10) / 10;
}

function formatMiB(bytes) {
  return (bytes / 1024 / 1024).toFixed(1);
}

function trim(value, max = 2000) {
  const text = String(value ?? "").trim();
  return text.length > max ? `${text.slice(0, max)}...` : text;
}

function formatSeconds(ms) {
  return (ms / 1000).toFixed(1);
}

function positiveInt(value, label) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`${label} must be a positive integer`);
  }
  return parsed;
}

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
      continue;
    }
    if (!arg.startsWith("--")) {
      throw new Error(`unexpected argument: ${arg}`);
    }
    const key = arg.slice(2).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
    const next = argv[index + 1];
    if (!next || next.startsWith("--")) {
      parsed[key] = "true";
    } else {
      parsed[key] = next;
      index += 1;
    }
  }
  return parsed;
}

function usage() {
  process.stdout.write(`Usage:
  node bench/review-quality/tools/run-muzen-martian-concurrent.mjs [options]

Options:
  --concurrency <n>             Review process concurrency (default: 5)
  --runner-mode <mode>          shared or process (default: shared)
  --limit <n>                   Limit cases from the source summary
  --case-source <file>          Previous summary with Martian PR URLs
  --golden-dir <dir>            Golden JSON directory
  --worktree-root <dir>         Materialized worktree root
  --output-dir <dir>            Review output directory
  --model <model>               Review model (default: MODEL or gpt-5.5)
  --semantic-model <model>      Semantic judge model (default: gpt-5.4-mini)
  --skip-semantic true          Skip semantic scoring
  --sample-interval-ms <n>      Process tree RSS sample interval (default: 5000)
  --max-captured-text-bytes <n> Snapshot text capture budget per run
`);
}
