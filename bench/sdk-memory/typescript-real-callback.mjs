#!/usr/bin/env node
import { spawn, execFileSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const SCHEMA_VERSION = "muzen.sdk-memory-benchmark.v1";
const ROLES = [
  "correctness",
  "security",
  "performance",
  "maintainability",
  "architecture",
  "validator",
];
const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "../..");

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    printHelp();
    return 0;
  }
  const apiKey = process.env[args.apiKeyEnv];
  if (!apiKey) {
    throw new Error(`${args.apiKeyEnv} is not set`);
  }

  const runnerPath = args.runnerPath ?? defaultRunnerPath();
  const repo = resolve(process.cwd(), args.repo);
  const sampler = new MemorySampler(args.sampleMs);
  const llm = new OpenAiCompatibleCallback({
    baseUrl: args.baseUrl,
    apiKey,
    model: args.model,
    maxOutputTokens: args.maxOutputTokens,
  });
  const startedAtUtc = new Date().toISOString();
  const started = performance.now();
  let result;
  let errorMessage;
  let client;

  sampler.start();
  try {
    client = new JsonRpcRunnerClient({
      runnerPath,
      callbacks: {
        "model.complete": (params) => llm.complete(params),
      },
    });
    await client.request("runner.handshake", {
      protocolVersion: "muzen.runner.v1",
      clientName: "typescript-real-sdk-memory-bench",
    });
    sampler.sample();
    result = await client.request("run.start", {
      protocolVersion: "muzen.runner.v1",
      runId: `typescript-real-sdk-memory-${Date.now()}`,
      repo,
      source: {
        type: "local",
        repo,
        changedFiles: args.changedFiles,
      },
      changedFiles: args.changedFiles,
      model: {
        callback: true,
      },
      sessions: buildSessions(args.sessions, args),
      limits: {
        maxActiveSessions: args.maxActive ?? args.sessions,
        maxFileBytes: args.maxFileKb * 1024,
        maxSearchMatches: args.maxSearchMatches,
      },
    });
    if (args.holdMs > 0) {
      await delay(args.holdMs);
    }
    sampler.sample();
  } catch (error) {
    errorMessage = error instanceof Error ? error.message : String(error);
  } finally {
    sampler.stop();
    await client?.close().catch(() => {});
  }

  const elapsedMs = Math.max(1, Math.ceil(performance.now() - started));
  const summary = result?.summary;
  const failures = benchmarkFailures(result, errorMessage, sampler, llm);
  const report = {
    schemaVersion: SCHEMA_VERSION,
    sdk: {
      language: "typescript",
      package: "@muzen/sdk runner-callback",
      runtime: `node ${process.version}`,
    },
    mode: "local-runner-stdio-real-model-callback",
    runner: {
      path: runnerPath,
    },
    provider: {
      baseUrl: redactUrl(args.baseUrl),
      model: args.model,
      apiKeyEnv: args.apiKeyEnv,
    },
    workload: {
      repo,
      sessions: args.sessions,
      maxActiveSessions: args.maxActive ?? args.sessions,
      maxTurns: args.maxTurns,
      maxToolCalls: args.maxToolCalls,
      maxFileKb: args.maxFileKb,
      maxSearchMatches: args.maxSearchMatches,
      changedFiles: args.changedFiles,
    },
    timing: {
      startedAtUtc,
      finishedAtUtc: new Date().toISOString(),
      elapsedMs,
      holdMs: args.holdMs,
      sampleMs: args.sampleMs,
    },
    memory: sampler.report(),
    modelCallbacks: llm.report(),
    result: result
      ? {
          status: result.status,
          summary,
          findings: result.findings?.length ?? 0,
          snapshots: result.snapshots ?? [],
        }
      : undefined,
    benchmarkValid: failures.length === 0,
    benchmarkFailures: failures,
  };

  const output = JSON.stringify(report, null, 2);
  if (args.output) {
    mkdirSync(dirname(resolve(process.cwd(), args.output)), { recursive: true });
    writeFileSync(args.output, `${output}\n`);
  } else {
    console.log(output);
  }
  return failures.length === 0 ? 0 : 1;
}

class JsonRpcRunnerClient {
  constructor({ runnerPath, callbacks }) {
    this.callbacks = callbacks;
    this.pending = new Map();
    this.nextId = 1;
    this.closed = false;
    this.child = spawn(runnerPath, ["stdio"], { stdio: "pipe" });
    this.child.once("error", (error) => this.rejectAll(error));
    this.child.once("exit", (code, signal) => {
      if (!this.closed) {
        this.rejectAll(new Error(`muzen-runner exited: code=${code} signal=${signal}`));
      }
    });
    this.lines = createInterface({
      input: this.child.stdout,
      crlfDelay: Number.POSITIVE_INFINITY,
    });
    this.lines.on("line", (line) => void this.handleLine(line));
  }

  request(method, params) {
    const id = this.nextId++;
    const frame = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolveRequest, rejectRequest) => {
      this.pending.set(id, { resolve: resolveRequest, reject: rejectRequest });
      this.child.stdin.write(`${JSON.stringify(frame)}\n`, (error) => {
        if (error) {
          this.pending.delete(id);
          rejectRequest(error);
        }
      });
    });
  }

  async close() {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.lines.close();
    this.child.stdin.end();
    if (!this.child.killed) {
      this.child.kill();
    }
    this.rejectAll(new Error("runner client closed"));
  }

  async handleLine(line) {
    if (line.trim().length === 0) {
      return;
    }
    const frame = JSON.parse(line);
    if (frame.method && frame.id !== undefined) {
      await this.handleCallback(frame);
      return;
    }
    if (frame.method) {
      return;
    }
    const pending = this.pending.get(frame.id);
    if (!pending) {
      return;
    }
    this.pending.delete(frame.id);
    if (frame.error) {
      pending.reject(new Error(frame.error.message ?? "runner request failed"));
    } else {
      pending.resolve(frame.result);
    }
  }

  async handleCallback(frame) {
    const callback = this.callbacks[frame.method];
    if (!callback) {
      this.writeResponse({
        jsonrpc: "2.0",
        id: frame.id,
        error: {
          code: -32601,
          message: `unknown callback ${frame.method}`,
          data: { kind: "method_not_found" },
        },
      });
      return;
    }
    try {
      const result = await callback(frame.params);
      this.writeResponse({ jsonrpc: "2.0", id: frame.id, result });
    } catch (error) {
      this.writeResponse({
        jsonrpc: "2.0",
        id: frame.id,
        error: {
          code: -32002,
          message: error instanceof Error ? error.message : String(error),
          data: { kind: "runner_error" },
        },
      });
    }
  }

  writeResponse(response) {
    this.child.stdin.write(`${JSON.stringify(response)}\n`);
  }

  rejectAll(error) {
    for (const pending of this.pending.values()) {
      pending.reject(error);
    }
    this.pending.clear();
  }
}

class OpenAiCompatibleCallback {
  constructor({ baseUrl, apiKey, model, maxOutputTokens }) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.apiKey = apiKey;
    this.model = model;
    this.maxOutputTokens = maxOutputTokens;
    this.calls = 0;
    this.inputTokens = 0;
    this.outputTokens = 0;
    this.totalTokens = 0;
    this.errors = 0;
    this.errorMessages = [];
  }

  async complete(params) {
    this.calls += 1;
    let response;
    try {
      response = await fetch(`${this.baseUrl}/chat/completions`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.apiKey}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          model: this.model,
          temperature: 0,
          max_tokens: this.maxOutputTokens,
          messages: [
            ...transcriptMessages(params.transcript ?? []),
            {
              role: "user",
              content:
                "For this live SDK memory benchmark, respond with one concise sentence and do not request tools.",
            },
          ],
        }),
      });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(`model request failed: ${response.status} ${text.slice(0, 200)}`);
      }
    } catch (error) {
      this.errors += 1;
      const message = error instanceof Error ? error.message : String(error);
      this.errorMessages.push(message.slice(0, 300));
      throw error;
    }
    const body = await response.json();
    const usage = body.usage ?? {};
    const inputTokens = usage.prompt_tokens ?? usage.input_tokens ?? 0;
    const outputTokens = usage.completion_tokens ?? usage.output_tokens ?? 0;
    const totalTokens = usage.total_tokens ?? inputTokens + outputTokens;
    this.inputTokens += inputTokens;
    this.outputTokens += outputTokens;
    this.totalTokens += totalTokens;
    return {
      content: body.choices?.[0]?.message?.content ?? "Live model benchmark completed.",
      usage: {
        inputTokens,
        outputTokens,
        totalTokens,
      },
    };
  }

  report() {
    return {
      calls: this.calls,
      inputTokens: this.inputTokens,
      outputTokens: this.outputTokens,
      totalTokens: this.totalTokens,
      errors: this.errors,
      errorMessages: this.errorMessages,
    };
  }
}

function transcriptMessages(items) {
  const messages = [];
  for (const item of items) {
    if (item.kind === "system") {
      messages.push({ role: "system", content: item.content ?? "" });
    } else if (item.kind === "user") {
      messages.push({ role: "user", content: item.content ?? "" });
    } else if (item.kind === "assistant_text") {
      messages.push({ role: "assistant", content: item.content ?? "" });
    }
  }
  if (messages.length === 0) {
    messages.push({ role: "user", content: "Run a concise live SDK memory benchmark response." });
  }
  return messages.slice(-8);
}

function buildSessions(count, options) {
  return Array.from({ length: count }, (_, index) => {
    const role = ROLES[index % ROLES.length];
    return {
      id: `typescript-real-sdk-bench-session-${index}`,
      role,
      objective:
        `Live SDK memory benchmark as ${role}; produce one concise benchmark response.`,
      budget: {
        maxTurns: options.maxTurns,
        maxToolCalls: options.maxToolCalls,
        maxPromptTokens: 32_000,
        maxOutputTokens: options.maxOutputTokens,
      },
    };
  });
}

function benchmarkFailures(result, error, sampler, llm) {
  const failures = [];
  if (error) {
    failures.push(`benchmark errored: ${error}`);
  }
  if (!result) {
    failures.push("no run result returned");
  } else {
    if (result.status !== "completed") {
      failures.push(`run status was ${result.status}`);
    }
    if (result.summary?.completedSessions !== result.summary?.sessions) {
      failures.push(
        `only ${result.summary?.completedSessions ?? 0}/${result.summary?.sessions ?? 0} sessions completed`,
      );
    }
  }
  if (llm.calls === 0) {
    failures.push("no live model callbacks were recorded");
  }
  if (llm.errors > 0) {
    failures.push(`${llm.errors} live model callback(s) failed`);
  }
  if (sampler.peakCombinedRssBytes === 0) {
    failures.push("memory sampler recorded no RSS samples");
  }
  return failures;
}

class MemorySampler {
  constructor(sampleMs) {
    this.sampleMs = sampleMs;
    this.started = performance.now();
    this.samples = [];
    this.runnerPids = new Set();
    this.peakClientRssBytes = 0;
    this.peakRunnerRssBytes = 0;
    this.peakCombinedRssBytes = 0;
  }

  start() {
    this.sample();
    this.timer = setInterval(() => this.sample(), this.sampleMs);
  }

  stop() {
    if (this.timer) {
      clearInterval(this.timer);
    }
    this.sample();
  }

  sample() {
    const table = processTable();
    const clientRssBytes = rssBytesForPid(table, process.pid);
    const runners = runnerDescendants(table, process.pid);
    const runnerRssBytes = runners.reduce((sum, proc) => sum + proc.rssBytes, 0);
    const combinedRssBytes = clientRssBytes + runnerRssBytes;
    for (const proc of runners) {
      this.runnerPids.add(proc.pid);
    }
    this.peakClientRssBytes = Math.max(this.peakClientRssBytes, clientRssBytes);
    this.peakRunnerRssBytes = Math.max(this.peakRunnerRssBytes, runnerRssBytes);
    this.peakCombinedRssBytes = Math.max(this.peakCombinedRssBytes, combinedRssBytes);
    this.samples.push({
      atMs: Math.max(0, Math.round(performance.now() - this.started)),
      clientRssBytes,
      runnerRssBytes,
      combinedRssBytes,
      runnerPids: runners.map((proc) => proc.pid),
    });
  }

  report() {
    const last = this.samples[this.samples.length - 1];
    return {
      peakClientRssBytes: this.peakClientRssBytes,
      peakRunnerRssBytes: this.peakRunnerRssBytes,
      peakCombinedRssBytes: this.peakCombinedRssBytes,
      finalClientRssBytes: last?.clientRssBytes ?? 0,
      finalRunnerRssBytes: last?.runnerRssBytes ?? 0,
      finalCombinedRssBytes: last?.combinedRssBytes ?? 0,
      sampleCount: this.samples.length,
      runnerPids: Array.from(this.runnerPids).sort((left, right) => left - right),
      samples: this.samples,
    };
  }
}

function processTable() {
  const output = execFileSync("ps", ["-axo", "pid=,ppid=,rss=,comm="], {
    encoding: "utf8",
  });
  return output
    .split("\n")
    .map((line) => {
      const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.+)$/);
      if (!match) {
        return undefined;
      }
      return {
        pid: Number(match[1]),
        ppid: Number(match[2]),
        rssBytes: Number(match[3]) * 1024,
        command: match[4],
      };
    })
    .filter(Boolean);
}

function rssBytesForPid(table, pid) {
  return table.find((proc) => proc.pid === pid)?.rssBytes ?? process.memoryUsage().rss;
}

function runnerDescendants(table, parentPid) {
  const byParent = new Map();
  for (const proc of table) {
    const children = byParent.get(proc.ppid) ?? [];
    children.push(proc);
    byParent.set(proc.ppid, children);
  }
  const descendants = [];
  const stack = [...(byParent.get(parentPid) ?? [])];
  while (stack.length > 0) {
    const proc = stack.pop();
    descendants.push(proc);
    stack.push(...(byParent.get(proc.pid) ?? []));
  }
  return descendants.filter((proc) => basename(proc.command).includes("muzen-runner"));
}

function defaultRunnerPath() {
  if (process.env.MUZEN_RUNNER_PATH) {
    return process.env.MUZEN_RUNNER_PATH;
  }
  for (const candidate of [
    resolve(repoRoot, "target/release/muzen-runner"),
    resolve(repoRoot, "target/debug/muzen-runner"),
  ]) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  return "muzen-runner";
}

function parseArgs(argv) {
  const parsed = {
    repo: ".",
    sessions: 2,
    maxActive: undefined,
    maxTurns: 1,
    maxToolCalls: 1,
    maxOutputTokens: 48,
    maxFileKb: 200,
    maxSearchMatches: 120,
    holdMs: 1000,
    sampleMs: 50,
    runnerPath: undefined,
    output: undefined,
    changedFiles: [],
    apiKeyEnv: process.env.AI_API_KEY ? "AI_API_KEY" : "OPENAI_API_KEY",
    baseUrl:
      process.env.AI_BASE_URL ??
      process.env.OPENAI_BASE_URL ??
      process.env.OAI_BASE_URL ??
      "https://api.openai.com/v1",
    model: process.env.OPENAI_REVIEW_MODEL ?? process.env.AI_MODEL ?? "gpt-4o-mini",
    help: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") parsed.help = true;
    else if (arg === "--repo") parsed.repo = requireValue(argv, ++index, arg);
    else if (arg === "--sessions") parsed.sessions = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-active") parsed.maxActive = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-turns") parsed.maxTurns = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-tool-calls") parsed.maxToolCalls = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-output-tokens") parsed.maxOutputTokens = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-file-kb") parsed.maxFileKb = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-search-matches") parsed.maxSearchMatches = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--hold-ms") parsed.holdMs = nonNegativeInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--sample-ms") parsed.sampleMs = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--runner-path") parsed.runnerPath = requireValue(argv, ++index, arg);
    else if (arg === "--output") parsed.output = requireValue(argv, ++index, arg);
    else if (arg === "--changed-file") parsed.changedFiles.push(requireValue(argv, ++index, arg));
    else if (arg === "--api-key-env") parsed.apiKeyEnv = requireValue(argv, ++index, arg);
    else if (arg === "--base-url") parsed.baseUrl = requireValue(argv, ++index, arg);
    else if (arg === "--model") parsed.model = requireValue(argv, ++index, arg);
    else throw new Error(`unknown argument: ${arg}`);
  }
  return parsed;
}

function requireValue(argv, index, name) {
  const value = argv[index];
  if (!value || value.startsWith("--")) {
    throw new Error(`${name} requires a value`);
  }
  return value;
}

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error(`${name} must be a positive integer`);
  }
  return parsed;
}

function nonNegativeInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`${name} must be a non-negative integer`);
  }
  return parsed;
}

function redactUrl(url) {
  try {
    const parsed = new URL(url);
    parsed.username = "";
    parsed.password = "";
    return parsed.toString().replace(/\/$/, "");
  } catch {
    return "<invalid-url>";
  }
}

function delay(ms) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, ms));
}

function printHelp() {
  console.log(`Usage: node bench/sdk-memory/typescript-real-callback.mjs [options]

Runs muzen-runner with model.callback=true and services model.complete via an
OpenAI-compatible Chat Completions request.

Options:
  --repo PATH                 Repo to review. Default: .
  --sessions N                Number of live model sessions. Default: 2
  --model MODEL               Model id. Default: OPENAI_REVIEW_MODEL or gpt-4o-mini
  --base-url URL              OpenAI-compatible base URL. Default: OPENAI_BASE_URL/OAI_BASE_URL or OpenAI
  --api-key-env NAME          API key env var. Default: OPENAI_API_KEY
  --runner-path PATH          muzen-runner path
  --output PATH               Write JSON report to path
`);
}

const exitCode = await main();
process.exitCode = exitCode;
