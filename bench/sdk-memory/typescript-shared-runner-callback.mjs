#!/usr/bin/env node
import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const SCHEMA_VERSION = "muzen.shared-runner-benchmark.v1";
const ROLES = ["correctness", "security", "performance", "maintainability", "architecture", "validator"];
const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "../..");

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const apiKey = process.env[args.apiKeyEnv];
  if (!apiKey) throw new Error(`${args.apiKeyEnv} is not set`);
  const workloads = JSON.parse(readFileSync(args.workloads, "utf8"));
  if (!Array.isArray(workloads) || workloads.length === 0) {
    throw new Error("--workloads must point to a non-empty JSON array");
  }

  const runnerPath = args.runnerPath ?? defaultRunnerPath();
  const llm = new SharedReviewCallback({
    baseUrl: args.baseUrl,
    apiKey,
    model: args.model,
    maxOutputTokens: args.maxOutputTokens,
  });
  const client = new JsonRpcRunnerClient({
    runnerPath,
    callbacks: { "model.complete": (params) => llm.complete(params) },
  });
  const sampler = new MemorySampler(args.sampleMs, client.child.pid);
  const startedAtUtc = new Date().toISOString();
  const started = performance.now();
  let results = [];
  let errorMessage;

  sampler.start();
  try {
    await client.request("runner.handshake", {
      protocolVersion: "muzen.runner.v1",
      clientName: "typescript-shared-runner-benchmark",
    });
    const promises = workloads.map((workload, index) =>
      client.request("run.start", buildRunStart(workload, index, args)),
    );
    results = await Promise.all(promises);
    if (args.holdMs > 0) await delay(args.holdMs);
  } catch (error) {
    errorMessage = error instanceof Error ? error.message : String(error);
  } finally {
    sampler.stop();
    await client.close().catch(() => {});
  }

  const report = {
    schemaVersion: SCHEMA_VERSION,
    mode: "shared-runner-stdio-real-model-callback",
    runner: { path: runnerPath },
    provider: {
      baseUrl: redactUrl(args.baseUrl),
      model: args.model,
      apiKeyEnv: args.apiKeyEnv,
    },
    workload: {
      count: workloads.length,
      ids: workloads.map((workload, index) => workload.id ?? `workload-${index + 1}`),
      maxOutputTokens: args.maxOutputTokens,
      maxToolCalls: args.maxToolCalls,
      maxFileKb: args.maxFileKb,
      maxSearchMatches: args.maxSearchMatches,
      workloads,
    },
    timing: {
      startedAtUtc,
      finishedAtUtc: new Date().toISOString(),
      elapsedMs: Math.max(1, Math.ceil(performance.now() - started)),
      holdMs: args.holdMs,
      sampleMs: args.sampleMs,
    },
    memory: sampler.report(),
    modelCallbacks: llm.report(),
    results: results.map((result) => summarizeRunResult(result)),
    benchmarkValid: benchmarkFailures(results, errorMessage, sampler, llm, workloads).length === 0,
    benchmarkFailures: benchmarkFailures(results, errorMessage, sampler, llm, workloads),
    errorMessage,
  };

  const output = JSON.stringify(report, null, 2);
  if (args.output) {
    mkdirSync(dirname(resolve(process.cwd(), args.output)), { recursive: true });
    writeFileSync(args.output, `${output}\n`);
  } else {
    console.log(output);
  }
  return report.benchmarkValid ? 0 : 1;
}

function buildRunStart(workload, index, args) {
  const repo = resolve(process.cwd(), required(workload.repo, `workloads[${index}].repo`));
  const changedFiles = requiredArray(workload.changedFiles, `workloads[${index}].changedFiles`);
  const baseRef = workload.baseRef;
  const inlineDiff = baseRef ? gitDiff(repo, baseRef) : undefined;
  const sessions = workload.sessions ?? Math.max(1, Math.ceil(changedFiles.length / args.filesPerSession));
  return {
    protocolVersion: "muzen.runner.v1",
    runId: workload.id ?? `shared-runner-${index + 1}-${Date.now()}`,
    repo,
    source: { type: "local", repo, changedFiles },
    changedFiles,
    change: inlineDiff
      ? {
          kind: "local_diff",
          baseRevision: baseRef,
          headRevision: "HEAD",
          changedFiles: changedFiles.map((path) => ({ path, status: "modified" })),
          diff: inlineDiff,
          reviewTarget: `local:${baseRef}..HEAD`,
        }
      : undefined,
    model: { callback: true },
    sessions: buildSessions(sessions, workload, changedFiles, args),
    limits: {
      maxActiveSessions: workload.maxActiveSessions ?? sessions,
      maxFileBytes: args.maxFileKb * 1024,
      maxSearchMatches: args.maxSearchMatches,
    },
    metadata: {
      benchmarkWorkloadId: workload.id ?? `workload-${index + 1}`,
      benchmarkMode: "shared-runner",
    },
  };
}

function buildSessions(count, workload, changedFiles, args) {
  return Array.from({ length: count }, (_, index) => {
    const role = ROLES[index % ROLES.length];
    const assigned = assignedFiles(changedFiles, index, count);
    return {
      id: `${workload.id ?? "workload"}-session-${index}`,
      role,
      objective:
        `Review this changed-file slice as ${role}. Gather concrete evidence with tools and return concise structured review JSON. Assigned files: ${assigned.join(", ")}`,
      budget: {
        maxTurns: args.maxTurns,
        maxToolCalls: args.maxToolCalls,
        maxPromptTokens: 64_000,
        maxOutputTokens: args.maxOutputTokens,
      },
    };
  });
}

function assignedFiles(files, sessionIndex, sessions) {
  return files.filter((_, index) => index % sessions === sessionIndex).slice(0, 8);
}

class SharedReviewCallback {
  constructor({ baseUrl, apiKey, model, maxOutputTokens }) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.apiKey = apiKey;
    this.model = model;
    this.maxOutputTokens = maxOutputTokens;
    this.calls = 0;
    this.toolPlanningCalls = 0;
    this.modelCalls = 0;
    this.inputTokens = 0;
    this.outputTokens = 0;
    this.totalTokens = 0;
    this.errors = [];
  }

  async complete(params) {
    this.calls += 1;
    if ((params.turn ?? 0) === 0) {
      this.toolPlanningCalls += 1;
      const changedFiles = changedFilesFromTranscript(params.transcript ?? []);
      const primary = primaryChangedFile(changedFiles);
      const callId = (suffix) => `${params.sessionId ?? "session"}-${params.turn ?? 0}-${suffix}`;
      return {
        toolCalls: [
          { callId: callId("diff"), toolId: "read_diff", arguments: {} },
          ...(primary
            ? [{ callId: callId("head-primary"), toolId: "read_head_file", arguments: { path: primary } }]
            : []),
          ...followUpQueries(changedFiles).slice(0, 2).map((query, index) => ({
            callId: callId(`search-${index}`),
            toolId: "search_text",
            arguments: { query },
          })),
        ].slice(0, 4),
        usage: { inputTokens: 0, outputTokens: 0, totalTokens: 0 },
      };
    }
    return await this.completeWithModel(params);
  }

  async completeWithModel(params) {
    this.modelCalls += 1;
    let body;
    try {
      const response = await fetch(`${this.baseUrl}/chat/completions`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.apiKey}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          model: this.model,
          temperature: 0,
          response_format: { type: "json_object" },
          ...tokenLimitParam(this.model, this.maxOutputTokens),
          messages: [
            ...transcriptMessages(params.transcript ?? []),
            {
              role: "user",
              content:
                "Return JSON only: {\"summary\": string, \"fileVerdicts\": [{\"path\": string, \"verdict\": \"clean\"|\"buggy\"|\"skipped\"}], \"findings\": [{\"title\": string, \"claim\": string, \"path\": string, \"startLine\": number, \"endLine\": number}]}. Include only concrete correctness findings supported by tool evidence.",
            },
          ],
        }),
      });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(`model request failed: ${response.status} ${text.slice(0, 300)}`);
      }
      body = await response.json();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.errors.push(message.slice(0, 500));
      throw error;
    }
    const usage = body.usage ?? {};
    const inputTokens = usage.prompt_tokens ?? usage.input_tokens ?? 0;
    const outputTokens = usage.completion_tokens ?? usage.output_tokens ?? 0;
    const totalTokens = usage.total_tokens ?? inputTokens + outputTokens;
    this.inputTokens += inputTokens;
    this.outputTokens += outputTokens;
    this.totalTokens += totalTokens;
    return {
      content:
        body.choices?.[0]?.message?.content ??
        JSON.stringify({ summary: "No model content returned.", fileVerdicts: [], findings: [] }),
      usage: { inputTokens, outputTokens, totalTokens },
    };
  }

  report() {
    return {
      calls: this.calls,
      toolPlanningCalls: this.toolPlanningCalls,
      modelCalls: this.modelCalls,
      inputTokens: this.inputTokens,
      outputTokens: this.outputTokens,
      totalTokens: this.totalTokens,
      errors: this.errors,
    };
  }
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
      if (!this.closed) this.rejectAll(new Error(`muzen-runner exited: code=${code} signal=${signal}`));
    });
    this.lines = createInterface({ input: this.child.stdout, crlfDelay: Number.POSITIVE_INFINITY });
    this.lines.on("line", (line) => void this.handleLine(line));
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolveRequest, rejectRequest) => {
      this.pending.set(id, { resolve: resolveRequest, reject: rejectRequest });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`, (error) => {
        if (error) {
          this.pending.delete(id);
          rejectRequest(error);
        }
      });
    });
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    this.lines.close();
    this.child.stdin.end();
    if (!this.child.killed) this.child.kill();
    this.rejectAll(new Error("runner client closed"));
  }

  async handleLine(line) {
    if (line.trim().length === 0) return;
    const frame = JSON.parse(line);
    if (frame.method && frame.id !== undefined) {
      await this.handleCallback(frame);
      return;
    }
    if (frame.method) return;
    const pending = this.pending.get(frame.id);
    if (!pending) return;
    this.pending.delete(frame.id);
    if (frame.error) pending.reject(new Error(frame.error.message ?? "runner request failed"));
    else pending.resolve(frame.result);
  }

  async handleCallback(frame) {
    const callback = this.callbacks[frame.method];
    if (!callback) {
      this.writeResponse({
        jsonrpc: "2.0",
        id: frame.id,
        error: { code: -32601, message: `unknown callback ${frame.method}` },
      });
      return;
    }
    try {
      this.writeResponse({ jsonrpc: "2.0", id: frame.id, result: await callback(frame.params) });
    } catch (error) {
      this.writeResponse({
        jsonrpc: "2.0",
        id: frame.id,
        error: { code: -32002, message: error instanceof Error ? error.message : String(error) },
      });
    }
  }

  writeResponse(response) {
    this.child.stdin.write(`${JSON.stringify(response)}\n`);
  }

  rejectAll(error) {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }
}

class MemorySampler {
  constructor(sampleMs, runnerPid) {
    this.sampleMs = sampleMs;
    this.runnerPid = runnerPid;
    this.started = performance.now();
    this.samples = [];
    this.peakClientRssBytes = 0;
    this.peakRunnerRssBytes = 0;
    this.peakCombinedRssBytes = 0;
  }

  start() {
    this.sample();
    this.timer = setInterval(() => this.sample(), this.sampleMs);
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.sample();
  }

  sample() {
    const table = processTable();
    const clientRssBytes = rssBytesForPid(table, process.pid);
    const runnerRssBytes = rssBytesForPid(table, this.runnerPid);
    const combinedRssBytes = clientRssBytes + runnerRssBytes;
    this.peakClientRssBytes = Math.max(this.peakClientRssBytes, clientRssBytes);
    this.peakRunnerRssBytes = Math.max(this.peakRunnerRssBytes, runnerRssBytes);
    this.peakCombinedRssBytes = Math.max(this.peakCombinedRssBytes, combinedRssBytes);
    this.samples.push({
      atMs: Math.max(0, Math.round(performance.now() - this.started)),
      clientRssBytes,
      runnerRssBytes,
      combinedRssBytes,
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
      runnerPid: this.runnerPid,
      samples: this.samples,
    };
  }
}

function summarizeRunResult(result) {
  return {
    runId: result?.runId,
    status: result?.status,
    summary: result?.summary,
    findings: result?.findings?.length ?? 0,
    fileReviews: result?.fileReviews?.length ?? 0,
  };
}

function benchmarkFailures(results, error, sampler, llm, workloads) {
  const failures = [];
  if (error) failures.push(`benchmark errored: ${error}`);
  if (results.length !== workloads.length) failures.push(`expected ${workloads.length} results, got ${results.length}`);
  for (const result of results) {
    if (result?.status !== "completed") failures.push(`run ${result?.runId ?? "unknown"} status was ${result?.status}`);
    if (result?.summary?.completedSessions !== result?.summary?.sessions) {
      failures.push(`run ${result?.runId ?? "unknown"} completed ${result?.summary?.completedSessions ?? 0}/${result?.summary?.sessions ?? 0} sessions`);
    }
  }
  if (llm.modelCalls === 0) failures.push("no live model callbacks were recorded");
  if (llm.errors.length > 0) failures.push(`${llm.errors.length} live model callback(s) failed`);
  if (sampler.peakRunnerRssBytes === 0) failures.push("memory sampler recorded no runner RSS samples");
  return failures;
}

function transcriptMessages(items) {
  const messages = [];
  for (const item of items) {
    if (item.kind === "system") messages.push({ role: "system", content: item.content ?? "" });
    else if (item.kind === "user") messages.push({ role: "user", content: item.content ?? "" });
    else if (item.kind === "assistant_text") messages.push({ role: "assistant", content: item.content ?? "" });
    else if (item.kind === "tool_result") {
      messages.push({ role: "user", content: `Tool ${item.toolId} returned ok=${item.ok}:\n${compactJson(item.data)}` });
    }
  }
  return messages.slice(-14);
}

function compactJson(value) {
  const text = JSON.stringify(value ?? null, null, 2);
  return text.length > 32000 ? `${text.slice(0, 32000)}\n...[truncated]` : text;
}

function changedFilesFromTranscript(items) {
  const text = items.map((item) => item.content ?? "").join("\n");
  const matches = [...text.matchAll(/[A-Za-z0-9_./-]+\.(?:ts|tsx|js|jsx|rs|go|py|java|kt|sql|prisma|d\.ts)/g)]
    .map((match) => match[0])
    .filter((path) => !path.startsWith("."));
  return [...new Set(matches)];
}

function primaryChangedFile(paths) {
  return [...paths].sort((left, right) => primaryPathScore(right) - primaryPathScore(left) || left.localeCompare(right))[0];
}

function followUpQueries(paths) {
  const tokens = rankedPathTokens(paths);
  return [tokens.slice(0, 5).join(" "), paths.slice(0, 2).map(pathStem).join(" ")].filter(Boolean);
}

function primaryPathScore(path) {
  const lower = path.toLowerCase();
  let score = 0;
  if (/\.(ts|tsx|js|jsx|rs|go|py|java|kt)$/.test(lower)) score += 20;
  if (lower.includes("/test/") || lower.includes(".test.") || lower.includes(".spec.")) score -= 12;
  if (lower.endsWith(".d.ts") || lower.includes("/types/")) score -= 8;
  if (lower.includes("/api/") || lower.includes("/server/") || lower.includes("/lib/")) score += 6;
  return score;
}

function rankedPathTokens(paths) {
  const counts = new Map();
  for (const path of paths) {
    for (const token of pathTokens(path)) counts.set(token, (counts.get(token) ?? 0) + 1);
  }
  return [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([token]) => token);
}

function pathTokens(path) {
  return path
    .split("/")
    .flatMap((part) => splitCamelAndWords(part.replace(/\.[^.]+$/, "")))
    .filter((token) => token.length >= 3 && !GENERIC_PATH_TOKENS.has(token));
}

function pathStem(path) {
  return splitCamelAndWords(path.split("/").pop()?.replace(/\.[^.]+$/, "") ?? "").slice(0, 4).join(" ");
}

function splitCamelAndWords(input) {
  return input
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .split(/[^A-Za-z0-9]+/)
    .map((token) => token.toLowerCase())
    .filter(Boolean);
}

const GENERIC_PATH_TOKENS = new Set(["src", "app", "apps", "web", "packages", "server", "client", "test", "types", "index", "lib"]);

function gitDiff(repo, baseRef) {
  for (const range of [`${baseRef}...HEAD`, `${baseRef}..HEAD`]) {
    try {
      const output = execFileSync("git", ["diff", "--patch", "--diff-filter=ACMRTUXB", range], {
        cwd: repo,
        encoding: "utf8",
        maxBuffer: 32 * 1024 * 1024,
      });
      if (output.trim()) return output;
    } catch {
      // Try the next range form.
    }
  }
  return undefined;
}

function processTable() {
  const output = execFileSync("ps", ["-axo", "pid=,rss=,comm="], { encoding: "utf8" });
  return output
    .split("\n")
    .map((line) => {
      const match = line.trim().match(/^(\d+)\s+(\d+)\s+(.+)$/);
      if (!match) return undefined;
      return { pid: Number(match[1]), rssBytes: Number(match[2]) * 1024, command: match[3] };
    })
    .filter(Boolean);
}

function rssBytesForPid(table, pid) {
  return table.find((proc) => proc.pid === pid)?.rssBytes ?? 0;
}

function tokenLimitParam(model, maxOutputTokens) {
  return model.startsWith("gpt-5") ? { max_completion_tokens: maxOutputTokens } : { max_tokens: maxOutputTokens };
}

function defaultRunnerPath() {
  if (process.env.MUZEN_RUNNER_PATH) return process.env.MUZEN_RUNNER_PATH;
  for (const candidate of [resolve(repoRoot, "target/release/muzen-runner"), resolve(repoRoot, "target/debug/muzen-runner")]) {
    if (existsSync(candidate)) return candidate;
  }
  return "muzen-runner";
}

function parseArgs(argv) {
  const parsed = {
    workloads: undefined,
    runnerPath: undefined,
    output: undefined,
    apiKeyEnv: process.env.AI_API_KEY ? "AI_API_KEY" : "OPENAI_API_KEY",
    baseUrl: process.env.AI_BASE_URL ?? process.env.OPENAI_BASE_URL ?? process.env.OAI_BASE_URL ?? "https://api.openai.com/v1",
    model: process.env.OPENAI_REVIEW_MODEL ?? process.env.OPENAI_MODEL ?? process.env.AI_MODEL ?? "gpt-4o-mini",
    maxTurns: 2,
    maxToolCalls: 4,
    maxOutputTokens: 1024,
    maxFileKb: 200,
    maxSearchMatches: 120,
    filesPerSession: 4,
    sampleMs: 50,
    holdMs: 500,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--workloads") parsed.workloads = requireValue(argv, ++index, arg);
    else if (arg === "--runner-path") parsed.runnerPath = requireValue(argv, ++index, arg);
    else if (arg === "--output") parsed.output = requireValue(argv, ++index, arg);
    else if (arg === "--api-key-env") parsed.apiKeyEnv = requireValue(argv, ++index, arg);
    else if (arg === "--base-url") parsed.baseUrl = requireValue(argv, ++index, arg);
    else if (arg === "--model") parsed.model = requireValue(argv, ++index, arg);
    else if (arg === "--max-turns") parsed.maxTurns = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-tool-calls") parsed.maxToolCalls = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-output-tokens") parsed.maxOutputTokens = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-file-kb") parsed.maxFileKb = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-search-matches") parsed.maxSearchMatches = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--files-per-session") parsed.filesPerSession = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--sample-ms") parsed.sampleMs = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--hold-ms") parsed.holdMs = positiveInt(requireValue(argv, ++index, arg), arg);
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (!parsed.workloads) throw new Error("--workloads is required");
  return parsed;
}

function requireValue(argv, index, name) {
  const value = argv[index];
  if (!value || value.startsWith("--")) throw new Error(`${name} requires a value`);
  return value;
}

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) throw new Error(`${name} must be a positive integer`);
  return parsed;
}

function required(value, name) {
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function requiredArray(value, name) {
  if (!Array.isArray(value) || value.length === 0) throw new Error(`${name} must be a non-empty array`);
  return value;
}

function redactUrl(url) {
  return url.replace(/([?&](?:api[_-]?key|token|key)=)[^&]+/gi, "$1<redacted>");
}

function delay(ms) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, ms));
}

main()
  .then((code) => process.exit(code ?? 0))
  .catch((error) => {
    console.error(error);
    process.exit(1);
  });
