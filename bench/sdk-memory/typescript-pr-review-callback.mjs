#!/usr/bin/env node
import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "../..");

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const apiKey = process.env[args.apiKeyEnv];
  if (!apiKey) throw new Error(`${args.apiKeyEnv} is not set`);
  const runnerPath = args.runnerPath ?? defaultRunnerPath();
  const repo = resolve(process.cwd(), args.repo);
  const inlineDiff = args.baseRef ? gitDiff(repo, args.baseRef) : undefined;
  const reviewer = new PlannedUnitReviewCallback({
    baseUrl: args.baseUrl,
    apiKey,
    model: args.model,
    maxOutputTokens: args.maxOutputTokens,
    changedFiles: args.changedFiles,
  });
  let result;
  let errorMessage;
  let client;
  const startedAtUtc = new Date().toISOString();
  const started = performance.now();

  try {
    client = new JsonRpcRunnerClient({
      runnerPath,
      callbacks: { "model.complete": (params) => reviewer.complete(params) },
    });
    await client.request("runner.handshake", {
      protocolVersion: "muzen.runner.v1",
      clientName: "typescript-pr-review-callback",
    });
    result = await client.request("run.start", {
      protocolVersion: "muzen.runner.v1",
      runId: `typescript-pr-review-${Date.now()}`,
      repo,
      source: {
        type: "local",
        repo,
        changedFiles: args.changedFiles,
      },
      changedFiles: args.changedFiles,
      change: inlineDiff
        ? {
            kind: "local_diff",
            baseRevision: args.baseRef,
            headRevision: "HEAD",
            changedFiles: args.changedFiles.map((path) => ({ path, status: "modified" })),
            diff: inlineDiff,
            reviewTarget: `local:${args.baseRef}..HEAD`,
          }
        : undefined,
      model: { callback: true },
      sessions: [
        {
          id: "typescript-pr-review-session",
          role: "correctness",
          objective:
            "Review this planned changed-file batch for actionable correctness issues.",
          budget: {
            maxTurns: 2,
            maxToolCalls: args.maxToolCalls,
            maxPromptTokens: 64000,
            maxOutputTokens: args.maxOutputTokens,
          },
        },
      ],
      limits: {
        maxActiveSessions: 1,
        maxFileBytes: args.maxFileKb * 1024,
        maxSearchMatches: args.maxSearchMatches,
      },
    });
  } catch (error) {
    errorMessage = error instanceof Error ? error.message : String(error);
  } finally {
    await client?.close().catch(() => {});
  }

  const report = {
    schemaVersion: "muzen.pr-review-callback.v1",
    mode: "local-runner-stdio-real-model-planned-unit-review",
    runner: { path: runnerPath },
    provider: {
      baseUrl: redactUrl(args.baseUrl),
      model: args.model,
      apiKeyEnv: args.apiKeyEnv,
    },
    workload: {
      repo,
      changedFiles: args.changedFiles,
      baseRef: args.baseRef,
      maxToolCalls: args.maxToolCalls,
      maxFileKb: args.maxFileKb,
      maxSearchMatches: args.maxSearchMatches,
    },
    timing: {
      startedAtUtc,
      finishedAtUtc: new Date().toISOString(),
      elapsedMs: Math.max(1, Math.ceil(performance.now() - started)),
    },
    modelCallbacks: reviewer.report(),
    errorMessage,
    result,
    reviewValid:
      !errorMessage &&
      result?.status === "completed" &&
      result?.summary?.completedSessions === result?.summary?.sessions &&
      (result?.fileReviews?.length ?? 0) === args.changedFiles.length &&
      (result?.fileReviews ?? []).every((review) =>
        review.verdict === "skipped" || (review.evidenceCount ?? 0) > 0
      ) &&
      (result?.summary?.toolCalls ?? 0) > 0 &&
      reviewer.modelCalls > 0,
  };

  const output = JSON.stringify(report, null, 2);
  if (args.output) {
    mkdirSync(dirname(resolve(process.cwd(), args.output)), { recursive: true });
    writeFileSync(args.output, `${output}\n`);
  } else {
    console.log(output);
  }
  return report.reviewValid ? 0 : 1;
}

class PlannedUnitReviewCallback {
  constructor({ baseUrl, apiKey, model, maxOutputTokens, changedFiles }) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.apiKey = apiKey;
    this.model = model;
    this.maxOutputTokens = maxOutputTokens;
    this.changedFiles = changedFiles;
    this.calls = 0;
    this.toolPlanningCalls = 0;
    this.modelCalls = 0;
    this.inputTokens = 0;
    this.outputTokens = 0;
    this.totalTokens = 0;
    this.errors = [];
    this.finalContents = [];
    this.finalMessageDebug = [];
  }

  async complete(params) {
    this.calls += 1;
    if ((params.turn ?? 0) === 0) {
      this.toolPlanningCalls += 1;
      const callId = (suffix) => `${params.sessionId ?? "planned-unit"}-${params.turn ?? 0}-${suffix}`;
      const firstTurnQueries = this.followUpQueries().slice(0, 2);
      const calls = [
        { callId: callId("diff"), toolId: "read_diff", arguments: {} },
        {
          callId: callId("head-primary"),
          toolId: "read_head_file",
          arguments: { path: this.primaryChangedFile() },
        },
        ...firstTurnQueries.map((query, index) => ({
          callId: callId(`search-${index}`),
          toolId: "search_text",
          arguments: { query },
        })),
      ];
      if (calls.length < 4) {
        calls.push({
          callId: callId("search-context"),
          toolId: "search_text",
          arguments: { query: this.genericQueryFromChangedFiles() },
        });
      }
      return {
        toolCalls: calls,
        usage: { inputTokens: 0, outputTokens: 0, totalTokens: 0 },
      };
    }
    if ((params.turn ?? 0) === 1) {
      this.toolPlanningCalls += 1;
      const callId = (suffix) => `${params.sessionId ?? "planned-unit"}-${params.turn ?? 1}-${suffix}`;
      const followUpQueries = this.followUpQueries();
      return {
        toolCalls: [
          ...followUpQueries.map((query, index) => ({
            callId: callId(`search-${index}`),
            toolId: "search_text",
            arguments: { query },
          })),
          {
            callId: callId("related-changed-file"),
            toolId: "find_related_files",
            arguments: { path: this.primaryChangedFile() },
          },
        ],
        usage: { inputTokens: 0, outputTokens: 0, totalTokens: 0 },
      };
    }
    return await this.completeWithModel(params);
  }

  primaryChangedFile() {
    return [...this.changedFiles]
      .sort((left, right) => primaryPathScore(right) - primaryPathScore(left) || left.localeCompare(right))[0];
  }

  followUpQueries() {
    const tokens = rankedPathTokens(this.changedFiles);
    const primary = this.primaryChangedFile();
    const primaryStem = pathStem(primary);
    return [
      [primaryStem, ...tokens.slice(0, 4)].filter(Boolean).join(" "),
      tokens.slice(0, 6).join(" "),
      this.changedFiles.slice(0, 2).map(pathStem).filter(Boolean).join(" "),
    ].filter((query, index, all) => query && all.indexOf(query) === index);
  }

  genericQueryFromChangedFiles() {
    return rankedPathTokens(this.changedFiles).slice(0, 6).join(" ") || this.changedFiles.map(pathStem).join(" ");
  }

  async completeWithModel(params) {
    this.modelCalls += 1;
    let body;
    try {
      const messages = [
        {
          role: "system",
          content:
            "You are reviewing a merge request. Use only the transcript evidence, including any artifactContent embedded in tool results. Return JSON only, with keys summary, fileVerdicts, and findings. fileVerdicts must include one verdict for every assigned changed file path. findings must be actionable bugs introduced by the change and each item must include title, claim, path, startLine, and endLine. If no bug is supported, return findings: []. For each reviewed source file, audit the changed invariants before deciding it is clean: persistent state updates, destructive queries, branching filters, boundary and interval math, equality/value semantics, validation, authorization or scoping assumptions, concurrency assumptions, and contracts with nearby helpers or callers. Report only issues directly supported by the gathered evidence.",
        },
        ...transcriptMessages(params.transcript ?? []),
        {
          role: "user",
          content:
            `Now produce the final structured review JSON. Include fileVerdicts for exactly these changed files: ${this.changedFiles.join(", ")}. Before returning a clean verdict for a source file, make sure the artifactContent and diff evidence do not support a concrete correctness issue. Do not include markdown or explanatory text outside JSON.`,
        },
      ];
      const joinedMessages = messages.map((message) => message.content ?? "").join("\n");
      this.finalMessageDebug.push({
        messageCount: messages.length,
        chars: joinedMessages.length,
        hasArtifactContent: joinedMessages.includes("artifactContent"),
        toolResultMessages: messages.filter((message) => message.content?.includes("Tool ")).length,
        changedFileMentions: this.changedFiles.filter((path) => joinedMessages.includes(path)).length,
      });
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
          messages,
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
    const content =
      body.choices?.[0]?.message?.content ??
      JSON.stringify({ summary: "No model content returned.", fileVerdicts: [], findings: [] });
    this.finalContents.push(content);
    return {
      content,
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
      finalContents: this.finalContents,
      finalMessageDebug: this.finalMessageDebug,
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
      this.child.stdin.write(`${JSON.stringify({
        jsonrpc: "2.0",
        id: frame.id,
        error: { code: -32601, message: `unknown callback ${frame.method}` },
      })}\n`);
      return;
    }
    try {
      const result = await callback(frame.params);
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: frame.id, result })}\n`);
    } catch (error) {
      this.child.stdin.write(`${JSON.stringify({
        jsonrpc: "2.0",
        id: frame.id,
        error: { code: -32002, message: error instanceof Error ? error.message : String(error) },
      })}\n`);
    }
  }

  rejectAll(error) {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }
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
  return messages.slice(-16);
}

function compactJson(value) {
  const text = JSON.stringify(value ?? null, null, 2);
  return text.length > 45000 ? `${text.slice(0, 45000)}\n...[truncated]` : text;
}

function primaryPathScore(path) {
  const lower = path.toLowerCase();
  let score = 0;
  if (/\.(ts|tsx|js|jsx|rs|go|py|java|kt|cs|rb|php)$/.test(lower)) score += 20;
  if (lower.includes("/test/") || lower.includes(".test.") || lower.includes(".spec.")) score -= 15;
  if (lower.endsWith(".d.ts") || lower.includes("/types/")) score -= 10;
  if (lower.includes("/api/") || lower.includes("/server/") || lower.includes("/routers/")) score += 8;
  if (lower.includes("/lib/") || lower.includes("/core/") || lower.includes("/service")) score += 5;
  if (lower.includes("/migration") || lower.includes("/schema")) score += 3;
  return score;
}

function rankedPathTokens(paths) {
  const counts = new Map();
  for (const path of paths) {
    for (const token of pathTokens(path)) {
      counts.set(token, (counts.get(token) ?? 0) + 1);
    }
  }
  return [...counts.entries()]
    .sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]))
    .map(([token]) => token);
}

function pathStem(path) {
  return splitCamelAndWords(path.split("/").pop()?.replace(/\.[^.]+$/, "") ?? "")
    .slice(0, 4)
    .join(" ");
}

function pathTokens(path) {
  return path
    .split("/")
    .flatMap((part) => splitCamelAndWords(part.replace(/\.[^.]+$/, "")))
    .filter((token) => token.length >= 3 && !GENERIC_PATH_TOKENS.has(token));
}

function splitCamelAndWords(input) {
  return input
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .split(/[^A-Za-z0-9]+/)
    .map((token) => token.toLowerCase())
    .filter(Boolean);
}

const GENERIC_PATH_TOKENS = new Set([
  "src",
  "app",
  "apps",
  "web",
  "packages",
  "package",
  "server",
  "client",
  "test",
  "tests",
  "spec",
  "types",
  "index",
  "lib",
]);

function tokenLimitParam(model, maxOutputTokens) {
  return model.startsWith("gpt-5")
    ? { max_completion_tokens: maxOutputTokens }
    : { max_tokens: maxOutputTokens };
}

function defaultRunnerPath() {
  for (const candidate of [
    resolve(repoRoot, "target/release/muzen-runner"),
    resolve(repoRoot, "target/debug/muzen-runner"),
  ]) {
    if (existsSync(candidate)) return candidate;
  }
  return "muzen-runner";
}

function gitDiff(repo, baseRef) {
  const ranges = [`${baseRef}...HEAD`, `${baseRef}..HEAD`];
  for (const range of ranges) {
    try {
      const output = execFileSync("git", ["diff", "--patch", "--diff-filter=ACMRTUXB", range], {
        cwd: repo,
        encoding: "utf8",
        maxBuffer: 32 * 1024 * 1024,
      });
      if (output.trim()) return output;
    } catch {
      // Try the next standard range form.
    }
  }
  return undefined;
}

function parseArgs(argv) {
  const parsed = {
    repo: ".",
    runnerPath: undefined,
    output: undefined,
    changedFiles: [],
    baseRef: undefined,
    apiKeyEnv: process.env.AI_API_KEY ? "AI_API_KEY" : "OPENAI_API_KEY",
    baseUrl: process.env.AI_BASE_URL ?? process.env.OPENAI_BASE_URL ?? "https://api.openai.com/v1",
    model: process.env.OPENAI_REVIEW_MODEL ?? process.env.OPENAI_MODEL ?? process.env.AI_MODEL ?? "gpt-4o-mini",
    maxOutputTokens: 2048,
    maxToolCalls: 8,
    maxFileKb: 200,
    maxSearchMatches: 120,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--repo") parsed.repo = requireValue(argv, ++index, arg);
    else if (arg === "--runner-path") parsed.runnerPath = requireValue(argv, ++index, arg);
    else if (arg === "--output") parsed.output = requireValue(argv, ++index, arg);
    else if (arg === "--changed-file") parsed.changedFiles.push(requireValue(argv, ++index, arg));
    else if (arg === "--base-ref") parsed.baseRef = requireValue(argv, ++index, arg);
    else if (arg === "--api-key-env") parsed.apiKeyEnv = requireValue(argv, ++index, arg);
    else if (arg === "--base-url") parsed.baseUrl = requireValue(argv, ++index, arg);
    else if (arg === "--model") parsed.model = requireValue(argv, ++index, arg);
    else if (arg === "--max-output-tokens") parsed.maxOutputTokens = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-tool-calls") parsed.maxToolCalls = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-file-kb") parsed.maxFileKb = positiveInt(requireValue(argv, ++index, arg), arg);
    else if (arg === "--max-search-matches") parsed.maxSearchMatches = positiveInt(requireValue(argv, ++index, arg), arg);
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (parsed.changedFiles.length === 0) throw new Error("at least one --changed-file is required");
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

main().then((code) => process.exit(code)).catch((error) => {
  console.error(error instanceof Error ? error.stack : String(error));
  process.exit(1);
});
