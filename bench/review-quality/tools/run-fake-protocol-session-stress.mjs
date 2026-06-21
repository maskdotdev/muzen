#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  usage();
  process.exit(0);
}

const runnerPath = path.resolve(args.runnerPath || "target/release/muzen-runner");
const outputDir = path.resolve(
  args.outputDir || `/tmp/muzen-fake-protocol-session-stress-${timestamp()}`,
);
const cases = positiveInt(args.cases || "6", "--cases");
const concurrency = positiveInt(args.concurrency || "3", "--concurrency");
const sessions = positiveInt(args.sessions || "3", "--sessions");
const maxActiveSessions = positiveInt(args.maxActiveSessions || args.maxActive || "2", "--max-active-sessions");
const maxToolCalls = positiveInt(args.maxToolCalls || "4", "--max-tool-calls");
const maxTurns = positiveInt(args.maxTurns || "6", "--max-turns");
const toolsPerSession = positiveInt(args.toolsPerSession || "1", "--tools-per-session");
const toolCallsPerTurn = positiveInt(args.toolCallsPerTurn || "1", "--tool-calls-per-turn");
const toolDelayMs = nonnegativeInt(args.toolDelayMs || "100", "--tool-delay-ms");
const modelDelayMs = nonnegativeInt(args.modelDelayMs || "0", "--model-delay-ms");
const artifactBytes = positiveInt(args.artifactBytes || "2048", "--artifact-bytes");
const failOnRegression = booleanArg(args.failOnRegression || "true", "--fail-on-regression");

const config = {
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
  artifactBytes,
};

async function main() {
  if (!fs.existsSync(runnerPath)) {
    fail(`runner not found at ${runnerPath}; run: cargo build --release --bin muzen-runner`);
  }

  fs.mkdirSync(outputDir, { recursive: true });
  const fixtureRoot = path.join(outputDir, "fixtures");
  fs.rmSync(fixtureRoot, { recursive: true, force: true });
  fs.mkdirSync(fixtureRoot, { recursive: true });
  const fixtures = createFixtures({ fixtureRoot, cases });

  const shared = await runSharedMode({ runnerPath, outputDir, fixtures, config });
  const processMode = await runProcessMode({ runnerPath, outputDir, fixtures, config });
  const report = buildReport({ outputDir, runnerPath, config, shared, process: processMode });
  fs.writeFileSync(
    path.join(outputDir, "protocol-session-stress-summary.json"),
    `${JSON.stringify(report, null, 2)}\n`,
  );
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  if (failOnRegression && hasBlockingRegression(report)) {
    fail(
      `fake protocol session stress found regressions; see ${path.join(outputDir, "protocol-session-stress-summary.json")}`,
    );
  }
}

async function runSharedMode({ runnerPath, outputDir, fixtures, config }) {
  const runner = new ProtocolRunner(runnerPath, {
    label: "shared",
    logDir: path.join(outputDir, "shared"),
    toolDelayMs: config.toolDelayMs,
    modelDelayMs: config.modelDelayMs,
    artifactBytes: config.artifactBytes,
    toolsPerSession: config.toolsPerSession,
    toolCallsPerTurn: config.toolCallsPerTurn,
  });
  await runner.start();
  try {
    const startedAt = Date.now();
    const results = await runPool(fixtures, config.concurrency, (fixture) =>
      runner.runStart(runStartForFixture(fixture, config)),
    );
    return summarizeMode({
      label: "shared",
      startedAt,
      completedAt: Date.now(),
      results,
      callbacks: runner.callbacks,
      notifications: runner.notifications,
      frames: runner.frames,
      stderr: runner.stderr,
    });
  } finally {
    await runner.close();
  }
}

async function runProcessMode({ runnerPath, outputDir, fixtures, config }) {
  const startedAt = Date.now();
  const runners = [];
  try {
    const results = await runPool(fixtures, config.concurrency, async (fixture) => {
      const runner = new ProtocolRunner(runnerPath, {
        label: `process-${fixture.index}`,
        logDir: path.join(outputDir, "process", fixture.name),
        toolDelayMs: config.toolDelayMs,
        modelDelayMs: config.modelDelayMs,
        artifactBytes: config.artifactBytes,
        toolsPerSession: config.toolsPerSession,
        toolCallsPerTurn: config.toolCallsPerTurn,
      });
      runners.push(runner);
      await runner.start();
      try {
        return await runner.runStart(runStartForFixture(fixture, config));
      } finally {
        await runner.close();
      }
    });
    return summarizeMode({
      label: "process",
      startedAt,
      completedAt: Date.now(),
      results,
      callbacks: runners.flatMap((runner) => runner.callbacks),
      notifications: runners.flatMap((runner) => runner.notifications),
      frames: runners.flatMap((runner) => runner.frames),
      stderr: runners.map((runner) => runner.stderr).join("\n"),
    });
  } finally {
    await Promise.all(runners.map((runner) => runner.close().catch(() => {})));
  }
}

class ProtocolRunner {
  constructor(runnerPath, { label, logDir, toolDelayMs, modelDelayMs, artifactBytes, toolsPerSession, toolCallsPerTurn }) {
    this.runnerPath = runnerPath;
    this.label = label;
    this.logDir = logDir;
    this.toolDelayMs = toolDelayMs;
    this.modelDelayMs = modelDelayMs;
    this.artifactBytes = artifactBytes;
    this.toolsPerSession = toolsPerSession;
    this.toolCallsPerTurn = toolCallsPerTurn;
    this.child = null;
    this.nextId = 1;
    this.pending = new Map();
    this.frames = [];
    this.callbacks = [];
    this.notifications = [];
    this.stderr = "";
    this.exit = null;
  }

  async start() {
    fs.mkdirSync(this.logDir, { recursive: true });
    this.child = spawn(this.runnerPath, ["stdio"], {
      cwd: process.cwd(),
      env: process.env,
      stdio: ["pipe", "pipe", "pipe"],
    });
    createInterface({ input: this.child.stdout }).on("line", (line) => {
      void this.handleLine(line);
    });
    this.child.stderr.on("data", (chunk) => {
      const text = chunk.toString("utf8");
      this.stderr += text;
      fs.appendFileSync(path.join(this.logDir, "stderr.log"), text);
    });
    this.exit = new Promise((resolve) => {
      this.child.on("exit", (code, signal) => {
        for (const pending of this.pending.values()) {
          pending.reject(new Error(`runner exited code=${code} signal=${signal}`));
        }
        this.pending.clear();
        resolve({ code, signal });
      });
      this.child.on("error", (error) => {
        for (const pending of this.pending.values()) pending.reject(error);
        this.pending.clear();
        resolve({ code: 1, signal: null, error });
      });
    });
    await this.request("runner.handshake", {
      protocolVersion: "muzen.runner.v1",
      clientName: `fake-protocol-session-stress-${this.label}`,
    });
  }

  async close() {
    if (!this.child || this.child.exitCode !== null) return;
    this.child.stdin.end();
    const timeout = sleep(1000).then(() => {
      if (this.child?.exitCode === null) this.child.kill("SIGKILL");
    });
    await Promise.race([this.exit, timeout]);
  }

  runStart(params) {
    return this.request("run.start", params);
  }

  request(method, params) {
    if (!this.child || this.child.exitCode !== null) {
      return Promise.reject(new Error("runner is not running"));
    }
    const id = this.nextId++;
    const promise = new Promise((resolve, reject) => {
      this.pending.set(String(id), { resolve, reject, method, params });
    });
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return promise;
  }

  async handleLine(line) {
    if (!line.trim()) return;
    let frame;
    try {
      frame = JSON.parse(line);
    } catch (error) {
      fs.appendFileSync(path.join(this.logDir, "parse-errors.log"), `${error.message}\n${line}\n`);
      return;
    }
    this.frames.push(frame);
    fs.appendFileSync(path.join(this.logDir, "frames.jsonl"), `${JSON.stringify(frame)}\n`);

    if (frame.method && frame.id !== undefined && frame.id !== null) {
      await this.answerCallback(frame);
      return;
    }
    if (frame.method) {
      this.notifications.push(frame);
      return;
    }
    if (frame.id !== undefined && frame.id !== null) {
      const pending = this.pending.get(String(frame.id));
      if (!pending) return;
      this.pending.delete(String(frame.id));
      if (frame.error) {
        pending.reject(new Error(`${frame.error.message}: ${JSON.stringify(frame.error.data ?? {})}`));
      } else {
        pending.resolve(frame.result);
      }
    }
  }

  async answerCallback(frame) {
    const params = frame.params ?? {};
    const record = {
      label: this.label,
      id: frame.id,
      method: frame.method,
      runId: params.runId ?? null,
      sessionId: params.sessionId ?? null,
      argumentRunId: params.arguments?.runId ?? null,
      argumentSessionId: params.arguments?.sessionId ?? null,
      turn: params.turn ?? null,
      callId: params.callId ?? null,
      toolId: params.toolId ?? null,
      startedAt: Date.now(),
    };
    this.callbacks.push(record);
    try {
      const result =
        frame.method === "model.complete"
          ? await this.modelComplete(params)
          : frame.method === "tool.execute"
            ? await this.toolExecute(params)
            : (() => {
                throw new Error(`unexpected callback ${frame.method}`);
              })();
      record.completedAt = Date.now();
      this.write({
        jsonrpc: "2.0",
        id: frame.id,
        result,
      });
    } catch (error) {
      record.completedAt = Date.now();
      record.error = error instanceof Error ? error.message : String(error);
      this.write({
        jsonrpc: "2.0",
        id: frame.id,
        error: {
          code: -32000,
          message: record.error,
          data: { kind: "fake_protocol_session_stress_error" },
        },
      });
    }
  }

  async modelComplete(params) {
    if (this.modelDelayMs > 0) await sleep(this.modelDelayMs);
    const turn = Number(params.turn ?? 0);
    const runId = String(params.runId ?? "unknown-run");
    const sessionId = String(params.sessionId ?? "unknown-session");
    const requestedToolCalls = transcriptRequestedToolCalls(params.transcript);
    const remainingRequestedTools = Math.max(0, this.toolsPerSession - requestedToolCalls);
    const finalInstructionRequested = transcriptHasFinalInstruction(params.transcript);
    if (!finalInstructionRequested && remainingRequestedTools > 0) {
      const callsThisTurn = Math.min(this.toolCallsPerTurn, remainingRequestedTools);
      return {
        toolCalls: Array.from({ length: callsThisTurn }, (_, index) => ({
            callId: `${runId}-${sessionId}-probe-${turn}-${index}`,
            toolId: "ownership_probe",
            arguments: { runId, sessionId, turn, index },
        })),
        usage: usageFor(turn),
      };
    }
    return {
      content: JSON.stringify({
        verdict: "clean",
        summary: `${runId}/${sessionId} completed after callback tool`,
        candidates: [],
        notes: [],
        completeness: {
          reviewedChangedFiles: ["Cargo.toml"],
          reviewedRiskEntries: [],
          unreviewedRiskEntries: [],
          unresolvedQuestions: [],
          incompleteReasons: [],
          ignoredChildCandidates: [],
        },
      }),
      usage: usageFor(turn),
    };
  }

  async toolExecute(params) {
    if (this.toolDelayMs > 0) await sleep(this.toolDelayMs);
    const runId = String(params.runId ?? "unknown-run");
    const sessionId = String(params.sessionId ?? "unknown-session");
    const callId = String(params.callId ?? "unknown-call");
    const payloadPrefix = `artifact run=${runId} session=${sessionId} call=${callId} `;
    const content =
      payloadPrefix.length >= this.artifactBytes
        ? payloadPrefix.slice(0, this.artifactBytes)
        : `${payloadPrefix}${"a".repeat(this.artifactBytes - payloadPrefix.length)}`;
    return {
      data: {
        runId,
        sessionId,
        callId,
        argumentRunId: params.arguments?.runId ?? null,
        argumentSessionId: params.arguments?.sessionId ?? null,
      },
      artifact: {
        key: `${runId}-${sessionId}-${callId}`,
        content,
      },
    };
  }

  write(frame) {
    this.child.stdin.write(`${JSON.stringify(frame)}\n`);
  }
}

function runStartForFixture(fixture, config) {
  return {
    protocolVersion: "muzen.runner.v1",
    runId: fixture.runId,
    repo: fixture.repo,
    changedFiles: ["Cargo.toml"],
    model: { callback: true },
    tools: [
      {
        id: "ownership_probe",
        description: "Records run and session ownership for protocol stress tests.",
        parameters: {
          type: "object",
          additionalProperties: false,
          required: ["runId", "sessionId", "turn", "index"],
          properties: {
            runId: { type: "string" },
            sessionId: { type: "string" },
            turn: { type: "integer" },
            index: { type: "integer" },
          },
        },
        effects: ["write_artifact"],
      },
    ],
    sessions: Array.from({ length: config.sessions }, (_, index) => ({
      id: `explicit-session-${index + 1}`,
      role: roleForIndex(index),
      objective: `Exercise explicit callback session ${index + 1} for ${fixture.runId}.`,
      budget: {
        maxTurns: config.maxTurns,
        maxToolCalls: config.maxToolCalls,
        maxPromptTokens: 64000,
        maxOutputTokens: 8000,
      },
    })),
    limits: {
      maxActiveSessions: config.maxActiveSessions,
      maxFileBytes: 200 * 1024,
      maxSearchMatches: 120,
    },
  };
}

function createFixtures({ fixtureRoot, cases }) {
  return Array.from({ length: cases }, (_, zeroIndex) => {
    const index = zeroIndex + 1;
    const name = `protocol-case-${index}`;
    const repo = path.join(fixtureRoot, name);
    fs.mkdirSync(repo, { recursive: true });
    fs.writeFileSync(
      path.join(repo, "Cargo.toml"),
      `[package]\nname = "fixture-${index}"\nversion = "0.0.0"\n`,
    );
    return {
      index,
      name,
      repo,
      runId: `protocol-run-${index}`,
    };
  });
}

function summarizeMode({ label, startedAt, completedAt, results, callbacks, notifications, frames, stderr }) {
  const budgetRejectedByRun = countBy(
    frames.filter(isBudgetRejectedToolCallFrame),
    (frame) => frameRunId(frame) ?? "unknown",
  );
  const runResults = results.map((result) => {
    const diagnostics = Array.isArray(result.summary?.completionDiagnostics)
      ? result.summary.completionDiagnostics
      : [];
    return {
      runId: result.runId,
      status: result.status,
      sessions: result.summary?.sessions ?? null,
      completedSessions: result.summary?.completedSessions ?? null,
      modelCalls: result.summary?.modelCalls ?? null,
      toolCalls: result.summary?.toolCalls ?? null,
      totalTokens: result.summary?.totalTokens ?? null,
      findings: Array.isArray(result.findings) ? result.findings.length : null,
      sessionOutputs: Array.isArray(result.sessionOutputs) ? result.sessionOutputs.length : null,
      sessionOutputIds: Array.isArray(result.sessionOutputs)
        ? result.sessionOutputs.map((output) => output.sessionId).sort()
        : [],
      diagnosticToolCallsUsed: sumNumbers(diagnostics.map((diagnostic) => diagnostic.toolCallsUsed)),
      diagnosticCustomToolCalls: sumNumbers(
        diagnostics.map((diagnostic) => diagnostic.toolCounts?.custom),
      ),
      diagnosticExhaustedSessions: diagnostics.filter(
        (diagnostic) => diagnostic.exhaustedToolBudget === true,
      ).length,
      budgetRejectedToolCalls: budgetRejectedByRun[result.runId] ?? 0,
    };
  });
  const callbacksByRun = groupBy(callbacks, (record) => record.runId ?? "unknown");
  const runCallbackSummaries = Object.fromEntries(
    Object.entries(callbacksByRun).map(([runId, records]) => [
      runId,
      {
        total: records.length,
        modelComplete: records.filter((record) => record.method === "model.complete").length,
        toolExecute: records.filter((record) => record.method === "tool.execute").length,
        sessions: [...new Set(records.map((record) => record.sessionId).filter(Boolean))].sort(),
        delayedToolMs: stats(
          records
            .filter((record) => record.method === "tool.execute")
            .map((record) => (record.completedAt ?? record.startedAt) - record.startedAt),
        ),
      },
    ]),
  );
  return {
    label,
    wallMs: completedAt - startedAt,
    runs: runResults.length,
    statuses: countObject(runResults.map((result) => result.status)),
    sessions: stats(runResults.map((result) => result.sessions)),
    completedSessions: stats(runResults.map((result) => result.completedSessions)),
    modelCalls: stats(runResults.map((result) => result.modelCalls)),
    toolCalls: stats(runResults.map((result) => result.toolCalls)),
    diagnosticToolCallsUsed: stats(runResults.map((result) => result.diagnosticToolCallsUsed)),
    diagnosticCustomToolCalls: stats(runResults.map((result) => result.diagnosticCustomToolCalls)),
    diagnosticExhaustedSessions: stats(runResults.map((result) => result.diagnosticExhaustedSessions)),
    budgetRejectedToolCalls: stats(runResults.map((result) => result.budgetRejectedToolCalls)),
    totalTokens: stats(runResults.map((result) => result.totalTokens)),
    findings: stats(runResults.map((result) => result.findings)),
    sessionOutputs: stats(runResults.map((result) => result.sessionOutputs)),
    callbacks: {
      total: callbacks.length,
      byMethod: countObject(callbacks.map((record) => record.method)),
      byRun: runCallbackSummaries,
      runIdMismatches: callbacks.filter((record) => {
        if (record.method !== "tool.execute") return false;
        return record.runId !== record.argumentRunId || record.sessionId !== record.argumentSessionId;
      }).length,
      errors: callbacks.filter((record) => record.error).length,
    },
    notifications: {
      total: notifications.length,
      byMethod: countObject(notifications.map((frame) => frame.method)),
      runFinished: notifications.filter((frame) => frame.method === "run.finished").length,
      unexpectedRunIds: notifications.filter((frame) => {
        const runId = frameRunId(frame);
        return runId && !runResults.some((result) => result.runId === runId);
      }).length,
    },
    frames: {
      total: frames.length,
      unexpectedRunIds: frames.filter((frame) => {
        const runId = frameRunId(frame);
        return runId && !runResults.some((result) => result.runId === runId);
      }).length,
    },
    stderrBytes: Buffer.byteLength(stderr),
    results: runResults,
  };
}

function buildReport({ outputDir, runnerPath, config, shared, process }) {
  const expectedSuccessfulToolCallsPerSession = Math.min(config.toolsPerSession, config.maxToolCalls);
  const expectedToolCallsPerRun = config.sessions * expectedSuccessfulToolCallsPerSession;
  const expectedBudgetRejectedToolCallsPerRun =
    config.sessions * expectedBudgetRejectedToolCallsPerSession(config);
  const expectedExhaustedSessionsPerRun =
    config.toolsPerSession >= config.maxToolCalls ? config.sessions : 0;
  const parity = {
    statuses: JSON.stringify(shared.statuses) === JSON.stringify(process.statuses),
    sessions:
      shared.sessions.min === process.sessions.min && shared.sessions.max === process.sessions.max,
    completedSessions:
      shared.completedSessions.min === process.completedSessions.min &&
      shared.completedSessions.max === process.completedSessions.max,
    modelCalls:
      shared.modelCalls.min === process.modelCalls.min && shared.modelCalls.max === process.modelCalls.max,
    toolCalls:
      shared.toolCalls.min === process.toolCalls.min && shared.toolCalls.max === process.toolCalls.max,
    diagnosticToolCallsUsed:
      shared.diagnosticToolCallsUsed.min === process.diagnosticToolCallsUsed.min &&
      shared.diagnosticToolCallsUsed.max === process.diagnosticToolCallsUsed.max,
    diagnosticCustomToolCalls:
      shared.diagnosticCustomToolCalls.min === process.diagnosticCustomToolCalls.min &&
      shared.diagnosticCustomToolCalls.max === process.diagnosticCustomToolCalls.max,
    diagnosticExhaustedSessions:
      shared.diagnosticExhaustedSessions.min === process.diagnosticExhaustedSessions.min &&
      shared.diagnosticExhaustedSessions.max === process.diagnosticExhaustedSessions.max,
    budgetRejectedToolCalls:
      shared.budgetRejectedToolCalls.min === process.budgetRejectedToolCalls.min &&
      shared.budgetRejectedToolCalls.max === process.budgetRejectedToolCalls.max,
    totalTokens:
      shared.totalTokens.min === process.totalTokens.min && shared.totalTokens.max === process.totalTokens.max,
    findings:
      shared.findings.min === process.findings.min && shared.findings.max === process.findings.max,
    sessionOutputs:
      shared.sessionOutputs.min === process.sessionOutputs.min &&
      shared.sessionOutputs.max === process.sessionOutputs.max,
    callbacksByMethod:
      JSON.stringify(shared.callbacks.byMethod) === JSON.stringify(process.callbacks.byMethod),
  };
  const regressions = {
    parityFailures: Object.entries(parity)
      .filter(([, ok]) => !ok)
      .map(([name]) => name),
    isolationFailures: [
      ...(shared.callbacks.runIdMismatches ? ["shared.callbackRunIdMismatches"] : []),
      ...(process.callbacks.runIdMismatches ? ["process.callbackRunIdMismatches"] : []),
      ...(shared.callbacks.errors ? ["shared.callbackErrors"] : []),
      ...(process.callbacks.errors ? ["process.callbackErrors"] : []),
      ...(shared.notifications.unexpectedRunIds ? ["shared.notificationUnexpectedRunIds"] : []),
      ...(process.notifications.unexpectedRunIds ? ["process.notificationUnexpectedRunIds"] : []),
      ...(shared.frames.unexpectedRunIds ? ["shared.frameUnexpectedRunIds"] : []),
      ...(process.frames.unexpectedRunIds ? ["process.frameUnexpectedRunIds"] : []),
    ],
    toolAccountingFailures: [
      ...(shared.toolCalls.min !== expectedToolCallsPerRun || shared.toolCalls.max !== expectedToolCallsPerRun
        ? ["shared.toolCalls"]
        : []),
      ...(process.toolCalls.min !== expectedToolCallsPerRun || process.toolCalls.max !== expectedToolCallsPerRun
        ? ["process.toolCalls"]
        : []),
      ...(shared.diagnosticToolCallsUsed.min !== expectedToolCallsPerRun ||
      shared.diagnosticToolCallsUsed.max !== expectedToolCallsPerRun
        ? ["shared.diagnosticToolCallsUsed"]
        : []),
      ...(process.diagnosticToolCallsUsed.min !== expectedToolCallsPerRun ||
      process.diagnosticToolCallsUsed.max !== expectedToolCallsPerRun
        ? ["process.diagnosticToolCallsUsed"]
        : []),
      ...(shared.diagnosticCustomToolCalls.min !== expectedToolCallsPerRun ||
      shared.diagnosticCustomToolCalls.max !== expectedToolCallsPerRun
        ? ["shared.diagnosticCustomToolCalls"]
        : []),
      ...(process.diagnosticCustomToolCalls.min !== expectedToolCallsPerRun ||
      process.diagnosticCustomToolCalls.max !== expectedToolCallsPerRun
        ? ["process.diagnosticCustomToolCalls"]
        : []),
      ...(shared.diagnosticExhaustedSessions.min !== expectedExhaustedSessionsPerRun ||
      shared.diagnosticExhaustedSessions.max !== expectedExhaustedSessionsPerRun
        ? ["shared.diagnosticExhaustedSessions"]
        : []),
      ...(process.diagnosticExhaustedSessions.min !== expectedExhaustedSessionsPerRun ||
      process.diagnosticExhaustedSessions.max !== expectedExhaustedSessionsPerRun
        ? ["process.diagnosticExhaustedSessions"]
        : []),
      ...(shared.budgetRejectedToolCalls.min !== expectedBudgetRejectedToolCallsPerRun ||
      shared.budgetRejectedToolCalls.max !== expectedBudgetRejectedToolCallsPerRun
        ? ["shared.budgetRejectedToolCalls"]
        : []),
      ...(process.budgetRejectedToolCalls.min !== expectedBudgetRejectedToolCallsPerRun ||
      process.budgetRejectedToolCalls.max !== expectedBudgetRejectedToolCallsPerRun
        ? ["process.budgetRejectedToolCalls"]
        : []),
    ],
    explicitSessionFailures: [
      ...(shared.sessions.min !== config.sessions || shared.sessions.max !== config.sessions
        ? ["shared.sessions"]
        : []),
      ...(process.sessions.min !== config.sessions || process.sessions.max !== config.sessions
        ? ["process.sessions"]
        : []),
      ...(shared.completedSessions.min !== config.sessions || shared.completedSessions.max !== config.sessions
        ? ["shared.completedSessions"]
        : []),
      ...(process.completedSessions.min !== config.sessions || process.completedSessions.max !== config.sessions
        ? ["process.completedSessions"]
        : []),
      ...(shared.sessionOutputs.min !== config.sessions || shared.sessionOutputs.max !== config.sessions
        ? ["shared.sessionOutputs"]
        : []),
      ...(process.sessionOutputs.min !== config.sessions || process.sessionOutputs.max !== config.sessions
        ? ["process.sessionOutputs"]
        : []),
    ],
  };
  return {
    schemaVersion: "muzen.fake-protocol-session-stress.v1",
    generatedAtUtc: new Date().toISOString(),
    outputDir,
    runnerPath,
    config,
    parity,
    regressions,
    shared,
    process,
  };
}

function hasBlockingRegression(report) {
  return (
    report.regressions.parityFailures.length > 0 ||
    report.regressions.isolationFailures.length > 0 ||
    report.regressions.toolAccountingFailures.length > 0 ||
    report.regressions.explicitSessionFailures.length > 0
  );
}

async function runPool(items, concurrencyLimit, worker) {
  const results = new Array(items.length);
  let next = 0;
  const workers = Array.from({ length: Math.min(concurrencyLimit, items.length) }, async () => {
    while (next < items.length) {
      const index = next;
      next += 1;
      results[index] = await worker(items[index], index);
    }
  });
  await Promise.all(workers);
  return results;
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

function isBudgetRejectedToolCallFrame(frame) {
  const trace = frame.method === "event.runtime" ? frame.params?.event?.agentTrace : null;
  return trace?.traceKind === "tool_call_rejected" && trace.details?.errorCode === "budget_exceeded";
}

function expectedBudgetRejectedToolCallsPerSession(config) {
  let remainingRequestedTools = config.toolsPerSession;
  let remainingBudget = config.maxToolCalls;
  let rejected = 0;
  while (remainingRequestedTools > 0 && remainingBudget > 0) {
    const requested = Math.min(config.toolCallsPerTurn, remainingRequestedTools);
    const allowed = Math.min(requested, remainingBudget);
    rejected += requested - allowed;
    remainingBudget -= allowed;
    remainingRequestedTools -= requested;
  }
  return rejected;
}

function roleForIndex(index) {
  return ["correctness", "security", "performance"][index % 3];
}

function transcriptRequestedToolCalls(transcript) {
  if (!Array.isArray(transcript)) return 0;
  return transcript.reduce((total, item) => {
    if (item?.kind !== "assistant_tool_calls" || !Array.isArray(item.calls)) return total;
    return total + item.calls.length;
  }, 0);
}

function transcriptHasFinalInstruction(transcript) {
  if (!Array.isArray(transcript)) return false;
  return transcript.some(
    (item) =>
      item?.kind === "user" &&
      typeof item.content === "string" &&
      item.content.includes("Return the final output for this session now"),
  );
}

function usageFor(turn) {
  return {
    inputTokens: 10 + turn,
    outputTokens: 5,
    totalTokens: 15 + turn,
  };
}

function groupBy(items, keyFn) {
  return items.reduce((groups, item) => {
    const key = keyFn(item);
    (groups[key] ||= []).push(item);
    return groups;
  }, {});
}

function sumNumbers(values) {
  return values.filter(Number.isFinite).reduce((total, value) => total + value, 0);
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

function countObject(values) {
  const counts = new Map();
  for (const value of values) {
    if (value == null) continue;
    counts.set(value, (counts.get(value) ?? 0) + 1);
  }
  return Object.fromEntries([...counts.entries()].sort(([left], [right]) => String(left).localeCompare(String(right))));
}

function countBy(items, keyFn) {
  const counts = {};
  for (const item of items) {
    const key = keyFn(item);
    if (key == null) continue;
    counts[key] = (counts[key] ?? 0) + 1;
  }
  return counts;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
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
    "Usage: run-fake-protocol-session-stress.mjs [--runner-path target/release/muzen-runner] [--output-dir /tmp/stress] [--cases 6] [--concurrency 3] [--sessions 3] [--max-active-sessions 2] [--max-tool-calls 4] [--max-turns 6] [--tools-per-session 1] [--tool-calls-per-turn 1] [--tool-delay-ms 100] [--model-delay-ms 0] [--artifact-bytes 2048] [--fail-on-regression true|false]\n",
  );
}

await main();
