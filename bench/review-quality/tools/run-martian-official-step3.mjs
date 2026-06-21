#!/usr/bin/env node

import { spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const DEFAULT_PROXY_BASE_URL = "http://127.0.0.1:4141/v1";
const DEFAULT_CANDIDATE_MODEL = "openai/gpt-5.2";
const DEFAULT_BENCHMARK_ROOTS = [
  "/tmp/code-review-benchmark",
  "/Users/mask/code/heimdall/.tmp/code-review-benchmark",
];

const args = parseArgs(process.argv.slice(2));

if (args.help) {
  usage();
  process.exit(0);
}

if (!args.tool) {
  throw new Error("--tool is required, for example --tool muzen or --tool hagent");
}
const tool = args.tool;
const judgeModel =
  args.judgeModel ||
  process.env.MARTIAN_JUDGE_MODEL ||
  process.env.ANTI_CHEAT_MODEL ||
  process.env.MODEL ||
  "gpt-5.4-mini";
const proxyBaseUrl = trimTrailingSlash(
  args.proxyBaseUrl || process.env.OPENAI_BASE_URL || DEFAULT_PROXY_BASE_URL,
);
const proxyApiKey = args.proxyApiKey || process.env.OPENAI_API_KEY || "muzen-codex-proxy";
const sourceOfflineDir = resolveOfflineDir(args);
const isolation =
  args.summaryFile && args.isolate !== "false"
    ? createIsolatedOfflineDir(sourceOfflineDir, judgeModel)
    : null;
const offlineDir = isolation?.offlineDir || sourceOfflineDir;
const benchmarkDataFile = path.join(offlineDir, "results", "benchmark_data.json");
const candidateModel = args.candidateModel || DEFAULT_CANDIDATE_MODEL;
const targetModelDir = path.join(offlineDir, "results", sanitizeModelName(judgeModel));
const evaluationsFile = path.resolve(
  args.evaluationsFile || path.join(targetModelDir, `evaluations-${tool}.json`),
);

if (!fs.existsSync(benchmarkDataFile)) {
  throw new Error(`missing official Martian benchmark data: ${benchmarkDataFile}`);
}
if (!fs.existsSync(path.join(offlineDir, "code_review_benchmark", "step3_judge_comments.py"))) {
  throw new Error(`missing official step3_judge_comments module under ${offlineDir}`);
}

stageCandidates({
  offlineDir,
  tool,
  judgeModel,
  candidateModel,
  candidatesFile: args.candidatesFile,
  summaryFile: args.summaryFile,
  benchmarkDataFile,
});
assertToolExists({ benchmarkDataFile, tool });

if (args.dryRun === "true") {
  printPlan({ offlineDir, tool, judgeModel, proxyBaseUrl, evaluationsFile });
  process.exit(0);
}

const bridge = await startChatCompletionsBridge({ proxyBaseUrl, proxyApiKey, judgeModel });
try {
  printPlan({ offlineDir, tool, judgeModel, proxyBaseUrl, evaluationsFile, bridgeBaseUrl: bridge.baseUrl });
  const commandArgs = [
    "run",
    "python",
    "-m",
    "code_review_benchmark.step3_judge_comments",
    "--tool",
    tool,
    "--evaluations-file",
    evaluationsFile,
  ];
  if (args.limit) commandArgs.push("--limit", args.limit);
  if (args.force === "true") commandArgs.push("--force");
  if (args.structured === "true") commandArgs.push("--structured");
  if (args.dedupGroups) commandArgs.push("--dedup-groups", path.resolve(args.dedupGroups));

  await spawnChecked("uv", commandArgs, {
    cwd: offlineDir,
    env: {
      ...process.env,
      OPENAI_BASE_URL: proxyBaseUrl,
      OPENAI_API_KEY: proxyApiKey,
      MARTIAN_API_KEY: "muzen-codex-proxy",
      MARTIAN_BASE_URL: bridge.baseUrl,
      MARTIAN_MODEL: judgeModel,
    },
  });
} finally {
  await bridge.close();
}

function resolveOfflineDir(parsedArgs) {
  if (parsedArgs.offlineDir) return path.resolve(parsedArgs.offlineDir);
  const root = parsedArgs.benchmarkRoot || process.env.MARTIAN_BENCHMARK_ROOT;
  if (root) {
    const resolved = path.resolve(root);
    return path.basename(resolved) === "offline" ? resolved : path.join(resolved, "offline");
  }
  for (const candidate of DEFAULT_BENCHMARK_ROOTS) {
    const offline = path.join(candidate, "offline");
    if (fs.existsSync(path.join(offline, "pyproject.toml"))) return offline;
  }
  throw new Error(
    `could not find code-review-benchmark offline checkout; pass --benchmark-root or set MARTIAN_BENCHMARK_ROOT`,
  );
}

function createIsolatedOfflineDir(sourceOfflineDir, judgeModel) {
  const parent = fs.mkdtempSync(path.join(os.tmpdir(), "muzen-martian-step3-"));
  const offlineDir = path.join(parent, "offline");
  fs.mkdirSync(offlineDir, { recursive: true });
  for (const entry of fs.readdirSync(sourceOfflineDir)) {
    const source = path.join(sourceOfflineDir, entry);
    const target = path.join(offlineDir, entry);
    if (entry === "results") {
      fs.mkdirSync(path.join(target, sanitizeModelName(judgeModel)), { recursive: true });
      fs.copyFileSync(
        path.join(source, "benchmark_data.json"),
        path.join(target, "benchmark_data.json"),
      );
      const sourceCandidates = path.join(source, sanitizeModelName(judgeModel), "candidates.json");
      if (fs.existsSync(sourceCandidates)) {
        fs.copyFileSync(
          sourceCandidates,
          path.join(target, sanitizeModelName(judgeModel), "candidates.json"),
        );
      }
      continue;
    }
    const stat = fs.lstatSync(source);
    fs.symlinkSync(source, target, stat.isDirectory() ? "dir" : "file");
  }
  process.stderr.write(`[martian-step3] isolated offline dir: ${offlineDir}\n`);
  return { offlineDir, sourceOfflineDir };
}

function stageCandidates({ offlineDir, tool, judgeModel, candidateModel, candidatesFile, summaryFile, benchmarkDataFile }) {
  if (summaryFile) {
    stageSummaryCandidates({
      summaryFile: path.resolve(summaryFile),
      benchmarkDataFile,
      targetCandidatesFile: path.join(offlineDir, "results", sanitizeModelName(judgeModel), "candidates.json"),
      tool,
    });
    return;
  }

  const source =
    candidatesFile && path.resolve(candidatesFile) ||
    findCandidatesFile({ offlineDir, tool, preferredModel: candidateModel });
  if (!source) {
    throw new Error(
      `could not find candidates.json containing tool ${JSON.stringify(tool)}; pass --candidates-file`,
    );
  }

  const targetDir = path.join(offlineDir, "results", sanitizeModelName(judgeModel));
  const target = path.join(targetDir, "candidates.json");
  const sourceData = readJson(source);
  const sourceCount = countToolCandidateReviews(sourceData, tool);
  if (sourceCount === 0) {
    throw new Error(`${source} does not contain candidates for tool ${JSON.stringify(tool)}`);
  }

  fs.mkdirSync(targetDir, { recursive: true });
  const targetData = fs.existsSync(target) ? readJson(target) : {};
  for (const [goldenUrl, tools] of Object.entries(sourceData)) {
    if (!tools || !Array.isArray(tools[tool])) continue;
    targetData[goldenUrl] = {
      ...(targetData[goldenUrl] || {}),
      [tool]: tools[tool],
    };
  }
  fs.writeFileSync(target, `${JSON.stringify(targetData, null, 2)}\n`);
  process.stderr.write(
    `[martian-step3] staged ${sourceCount} ${tool} candidate review(s): ${source} -> ${target}\n`,
  );
}

function stageSummaryCandidates({ summaryFile, benchmarkDataFile, targetCandidatesFile, tool }) {
  if (!fs.existsSync(summaryFile)) throw new Error(`missing concurrent summary file: ${summaryFile}`);
  const summary = readJson(summaryFile);
  if (!Array.isArray(summary.results)) {
    throw new Error(`${summaryFile} does not look like a run-muzen-martian-concurrent summary`);
  }
  const benchmarkData = readJson(benchmarkDataFile);
  const candidates = fs.existsSync(targetCandidatesFile) ? readJson(targetCandidatesFile) : {};
  let reviewRows = 0;
  let candidateCount = 0;

  for (const entry of Object.values(benchmarkData)) {
    entry.reviews = (entry.reviews || []).filter((review) => review.tool !== tool);
  }
  for (const tools of Object.values(candidates)) {
    if (tools && typeof tools === "object") delete tools[tool];
  }

  for (const result of summary.results) {
    if (!result.prUrl || !benchmarkData[result.prUrl]) continue;
    const reportPath = path.resolve(result.outputPath || "");
    if (!reportPath || !fs.existsSync(reportPath)) {
      throw new Error(`missing result report for ${result.prUrl}: ${result.outputPath}`);
    }
    const report = readJson(reportPath);
    const findingCandidates = (report.findings || [])
      .map((finding, index) => findingToCandidate(finding, index))
      .filter((candidate) => candidate.text);

    candidates[result.prUrl] = {
      ...(candidates[result.prUrl] || {}),
      [tool]: findingCandidates,
    };
    candidateCount += findingCandidates.length;

    const entry = benchmarkData[result.prUrl];
    entry.reviews = (entry.reviews || []).filter((review) => review.tool !== tool);
    entry.reviews.push({
      tool,
      repo_name: `local-${tool}`,
      pr_url: result.prUrl,
      review_comments: [],
    });
    reviewRows += 1;
  }

  if (reviewRows === 0) {
    throw new Error(`${summaryFile} did not match any PR URLs in ${benchmarkDataFile}`);
  }

  fs.mkdirSync(path.dirname(targetCandidatesFile), { recursive: true });
  fs.writeFileSync(targetCandidatesFile, `${JSON.stringify(candidates, null, 2)}\n`);
  fs.writeFileSync(benchmarkDataFile, `${JSON.stringify(benchmarkData, null, 2)}\n`);
  process.stderr.write(
    `[martian-step3] materialized ${reviewRows} ${tool} review row(s) and ${candidateCount} candidate(s) from ${summaryFile}\n`,
  );
}

function findingToCandidate(finding, index) {
  const text = [
    finding.title,
    finding.claim,
    finding.behaviorBefore ? `Before: ${finding.behaviorBefore}` : null,
    finding.behaviorAfter ? `After: ${finding.behaviorAfter}` : null,
    Array.isArray(finding.evidenceTrail) && finding.evidenceTrail.length
      ? `Evidence: ${finding.evidenceTrail.join("\n")}`
      : null,
  ]
    .filter(Boolean)
    .join("\n\n")
    .trim();
  return {
    text,
    path: finding.path || finding.location?.path || null,
    line: finding.location?.startLine || finding.location?.line || null,
    source: finding.source || `summary_finding_${index + 1}`,
  };
}

function findCandidatesFile({ offlineDir, tool, preferredModel }) {
  const preferred = path.join(offlineDir, "results", sanitizeModelName(preferredModel), "candidates.json");
  if (fs.existsSync(preferred) && countToolCandidateReviews(readJson(preferred), tool) > 0) return preferred;

  const resultsDir = path.join(offlineDir, "results");
  if (!fs.existsSync(resultsDir)) return null;
  for (const entry of fs.readdirSync(resultsDir).sort()) {
    const candidate = path.join(resultsDir, entry, "candidates.json");
    if (fs.existsSync(candidate) && countToolCandidateReviews(readJson(candidate), tool) > 0) {
      return candidate;
    }
  }
  return null;
}

function assertToolExists({ benchmarkDataFile, tool }) {
  const benchmarkData = readJson(benchmarkDataFile);
  let reviewCount = 0;
  for (const entry of Object.values(benchmarkData)) {
    if ((entry.reviews || []).some((review) => review.tool === tool)) reviewCount += 1;
  }
  if (reviewCount === 0) {
    throw new Error(`${benchmarkDataFile} does not contain official review entries for tool ${tool}`);
  }
  process.stderr.write(`[martian-step3] official benchmark_data contains ${reviewCount} ${tool} review(s)\n`);
}

function countToolCandidateReviews(candidates, tool) {
  let count = 0;
  for (const tools of Object.values(candidates || {})) {
    if (Array.isArray(tools?.[tool]) && tools[tool].length > 0) count += 1;
  }
  return count;
}

async function startChatCompletionsBridge({ proxyBaseUrl, proxyApiKey, judgeModel }) {
  const port = await freePort();
  const host = "127.0.0.1";
  const server = http.createServer(async (request, response) => {
    try {
      const requestUrl = new URL(request.url || "/", `http://${host}:${port}`);
      if (request.method === "GET" && requestUrl.pathname === "/health") {
        writeJson(response, 200, { ok: true });
        return;
      }
      if (
        request.method !== "POST" ||
        !["/v1/chat/completions", "/chat/completions"].includes(requestUrl.pathname)
      ) {
        writeJson(response, 404, { error: "expected POST /v1/chat/completions" });
        return;
      }
      const body = JSON.parse(await readRequestBody(request));
      const upstream = await fetch(`${proxyBaseUrl}/responses`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${proxyApiKey}`,
        },
        body: JSON.stringify(toResponsesRequest(body, judgeModel)),
      });
      const text = await upstream.text();
      if (!upstream.ok) {
        response.writeHead(upstream.status, {
          "Content-Type": upstream.headers.get("content-type") || "text/plain; charset=utf-8",
        });
        response.end(text);
        return;
      }
      const upstreamJson = JSON.parse(text);
      const content = extractResponseText(upstreamJson);
      writeJson(response, 200, {
        id: upstreamJson.id || `chatcmpl_${Date.now()}`,
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: body.model || judgeModel,
        choices: [
          {
            index: 0,
            message: { role: "assistant", content },
            finish_reason: "stop",
          },
        ],
        usage: upstreamJson.usage || undefined,
      });
    } catch (error) {
      writeJson(response, 500, { error: error instanceof Error ? error.message : String(error) });
    }
  });
  await new Promise((resolve, reject) => {
    server.listen(port, host, resolve);
    server.once("error", reject);
  });
  return {
    baseUrl: `http://${host}:${port}/v1`,
    close: () => new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve()))),
  };
}

function toResponsesRequest(body, judgeModel) {
  const messages = Array.isArray(body.messages) ? body.messages : [];
  const instructions = messages
    .filter((message) => message.role === "system")
    .map((message) => normalizeMessageContent(message.content))
    .filter(Boolean)
    .join("\n\n");
  const request = {
    model: body.model || judgeModel,
    instructions,
    input: messages.filter((message) => message.role !== "system").map((message) => ({
      role: message.role === "assistant" ? "assistant" : "user",
      content: normalizeMessageContent(message.content),
    })),
    store: false,
  };
  if (body.response_format?.type === "json_schema") {
    request.text = {
      format: body.response_format,
    };
  } else if (body.response_format?.type === "json_object") {
    request.text = {
      format: { type: "json_object" },
    };
  }
  return request;
}

function normalizeMessageContent(content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part) => {
        if (typeof part === "string") return part;
        if (part?.type === "text") return part.text || "";
        return "";
      })
      .join("");
  }
  return String(content ?? "");
}

function extractResponseText(response) {
  if (typeof response.output_text === "string" && response.output_text.trim()) return response.output_text;
  const chunks = [];
  for (const item of response.output || []) {
    if (item.type === "message") {
      for (const part of item.content || []) {
        if (typeof part.text === "string") chunks.push(part.text);
      }
    }
    if (typeof item.text === "string") chunks.push(item.text);
  }
  return chunks.join("").trim();
}

function spawnChecked(command, commandArgs, options) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, commandArgs, {
      ...options,
      stdio: "inherit",
    });
    child.on("error", reject);
    child.on("exit", (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`${command} exited with ${signal || code}`));
    });
  });
}

function freePort() {
  const server = net.createServer();
  server.unref();
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => resolve(address.port));
    });
  });
}

function readRequestBody(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("error", reject);
    request.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
  });
}

function writeJson(response, status, payload) {
  response.writeHead(status, { "Content-Type": "application/json; charset=utf-8" });
  response.end(`${JSON.stringify(payload)}\n`);
}

function printPlan({ offlineDir, tool, judgeModel, proxyBaseUrl, evaluationsFile, bridgeBaseUrl = null }) {
  process.stderr.write(
    [
      `[martian-step3] offline dir: ${offlineDir}`,
      `[martian-step3] tool: ${tool}`,
      `[martian-step3] judge model: ${judgeModel}`,
      `[martian-step3] responses proxy: ${proxyBaseUrl}`,
      bridgeBaseUrl ? `[martian-step3] chat bridge: ${bridgeBaseUrl}` : null,
      `[martian-step3] evaluations: ${evaluationsFile}`,
    ]
      .filter(Boolean)
      .join("\n") + "\n",
  );
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function sanitizeModelName(model) {
  return String(model).trim().replaceAll("/", "_");
}

function trimTrailingSlash(value) {
  return String(value).replace(/\/+$/, "");
}

function parseArgs(argv) {
  const parsed = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
      continue;
    }
    if (!arg.startsWith("--")) throw new Error(`unexpected argument: ${arg}`);
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
  node bench/review-quality/tools/run-martian-official-step3.mjs [options]

Options:
  --tool <name>                 Required official benchmark tool to evaluate, for example muzen or hagent
  --benchmark-root <dir>        code-review-benchmark checkout root
  --offline-dir <dir>           code-review-benchmark/offline directory
  --proxy-base-url <url>        Codex ChatGPT Responses proxy URL (default: OPENAI_BASE_URL or ${DEFAULT_PROXY_BASE_URL})
  --judge-model <model>         Subscription proxy judge model (default: MARTIAN_JUDGE_MODEL, ANTI_CHEAT_MODEL, MODEL, or gpt-5.4-mini)
  --candidate-model <model>     Source model directory for candidates (default: ${DEFAULT_CANDIDATE_MODEL})
  --candidates-file <file>      Explicit source candidates.json
  --summary-file <file>         Materialize candidates/review rows from a concurrent summary.json
  --isolate false               Disable per-run offline isolation for --summary-file staging
  --evaluations-file <file>     Output evaluations file
  --dedup-groups <file>         Optional official dedup groups file
  --limit <n>                   Forwarded to step3_judge_comments
  --force true                  Re-evaluate this tool
  --structured true             Forward structured-output mode
  --dry-run true                Stage candidates and print the resolved launch plan
`);
}
