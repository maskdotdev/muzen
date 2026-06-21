#!/usr/bin/env node

import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const DEFAULT_BENCHMARK_ROOT = "/tmp/code-review-benchmark";
const DEFAULT_CASE_SOURCE =
  "/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-real-50-shared-c5/summary.json";
const DEFAULT_GOLDEN_DIR = "/tmp/code-review-benchmark/offline/results/muzen/gpt-5.5-low-direct/goldens";
const DEFAULT_WORKTREE_ROOT = "/tmp/muzen-hagent-martian-worktrees";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  usage();
  process.exit(0);
}

const model = args.model || process.env.MODEL || "gpt-5.5";
const judgeModel = args.judgeModel || process.env.MARTIAN_JUDGE_MODEL || "gpt-5.4-mini";
const openAiBaseUrl = args.openaiBaseUrl || process.env.OPENAI_BASE_URL || "http://127.0.0.1:4141/v1";
const openAiApiKey = args.openaiApiKey || process.env.OPENAI_API_KEY || "muzen-codex-proxy";
const runId = args.runId || timestamp();
const outputRoot = path.resolve(
  args.outputRoot ||
    `/tmp/code-review-benchmark/offline/results/muzen/${sanitizeLabel(model)}-martian-full-circle-${runId}`,
);
const reviewOutputDir = path.join(outputRoot, "reviews");
const step3Dir = path.join(outputRoot, "martian-step3");
const evaluationsFile = path.join(step3Dir, "evaluations-muzen.json");
const finalSummaryFile = path.join(outputRoot, "summary.json");

const config = {
  benchmarkRoot: path.resolve(args.benchmarkRoot || DEFAULT_BENCHMARK_ROOT),
  caseSource: path.resolve(args.caseSource || DEFAULT_CASE_SOURCE),
  goldenDir: path.resolve(args.goldenDir || DEFAULT_GOLDEN_DIR),
  worktreeRoot: path.resolve(args.worktreeRoot || DEFAULT_WORKTREE_ROOT),
  runnerPath: path.resolve(args.runnerPath || "target/release/muzen-runner"),
  runnerMode: args.runnerMode || "shared",
  concurrency: args.concurrency || "2",
  limit: args.limit || "5",
  sessions: args.sessions || "0",
  maxActive: args.maxActive || "1",
  maxTurns: args.maxTurns || "60",
  maxToolCalls: args.maxToolCalls || "50",
  skipSemantic: args.skipSemantic || "true",
  model,
  judgeModel,
  openAiBaseUrl,
  openAiApiKey,
  outputRoot,
  reviewOutputDir,
  evaluationsFile,
};

fs.mkdirSync(outputRoot, { recursive: true });
printConfig(config);

if (args.skipBuild !== "true") {
  await spawnChecked("cargo", ["build", "--release", "--bin", "muzen-runner"], {
    env: process.env,
  });
}

await spawnChecked(
  "node",
  [
    "bench/review-quality/tools/run-muzen-martian-concurrent.mjs",
    "--case-source",
    config.caseSource,
    "--golden-dir",
    config.goldenDir,
    "--worktree-root",
    config.worktreeRoot,
    "--runner-path",
    config.runnerPath,
    "--runner-mode",
    config.runnerMode,
    "--output-dir",
    reviewOutputDir,
    "--concurrency",
    config.concurrency,
    "--limit",
    config.limit,
    "--sessions",
    config.sessions,
    "--max-active",
    config.maxActive,
    "--max-turns",
    config.maxTurns,
    "--max-tool-calls",
    config.maxToolCalls,
    "--model",
    config.model,
    "--skip-semantic",
    config.skipSemantic,
    "--progress",
    "true",
  ],
  {
    env: {
      ...process.env,
      OPENAI_BASE_URL: openAiBaseUrl,
      OPENAI_API_KEY: openAiApiKey,
      MODEL: model,
    },
  },
);

fs.mkdirSync(step3Dir, { recursive: true });
await spawnChecked(
  "node",
  [
    "bench/review-quality/tools/run-martian-official-step3.mjs",
    "--benchmark-root",
    config.benchmarkRoot,
    "--tool",
    "muzen",
    "--summary-file",
    path.join(reviewOutputDir, "summary.json"),
    "--force",
    "true",
    "--evaluations-file",
    evaluationsFile,
    "--judge-model",
    judgeModel,
  ],
  {
    env: {
      ...process.env,
      OPENAI_BASE_URL: openAiBaseUrl,
      OPENAI_API_KEY: openAiApiKey,
      MARTIAN_JUDGE_MODEL: judgeModel,
    },
  },
);

const summary = buildFullCircleSummary({
  config,
  reviewSummaryFile: path.join(reviewOutputDir, "summary.json"),
  evaluationsFile,
});
fs.writeFileSync(finalSummaryFile, `${JSON.stringify(summary, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`);

function buildFullCircleSummary({ config, reviewSummaryFile, evaluationsFile }) {
  const reviewSummary = readJson(reviewSummaryFile);
  const evaluations = readJson(evaluationsFile);
  const martian = summarizeMartianEvaluations(evaluations, "muzen");
  return {
    schemaVersion: "muzen.martian-full-circle.v1",
    generatedAtUtc: new Date().toISOString(),
    model: config.model,
    judgeModel: config.judgeModel,
    outputRoot: config.outputRoot,
    artifacts: {
      reviews: config.reviewOutputDir,
      reviewSummary: reviewSummaryFile,
      evaluations: evaluationsFile,
      summary: finalSummaryFile,
    },
    review: {
      caseCount: reviewSummary.caseCount,
      validCount: reviewSummary.validCount,
      wallMs: reviewSummary.wallMs,
      totals: reviewSummary.totals,
    },
    martian,
  };
}

function summarizeMartianEvaluations(evaluations, tool) {
  const totals = {
    reviews: 0,
    candidates: 0,
    golden: 0,
    tp: 0,
    fp: 0,
    fn: 0,
    errors: 0,
  };
  const cases = [];
  for (const [prUrl, tools] of Object.entries(evaluations)) {
    const result = tools?.[tool];
    if (!result) continue;
    totals.reviews += 1;
    totals.candidates += Number(result.total_candidates || 0);
    totals.golden += Number(result.total_golden || 0);
    totals.tp += Number(result.tp || 0);
    totals.fp += Number(result.fp || 0);
    totals.fn += Number(result.fn || 0);
    totals.errors += Number(result.errors_count || 0);
    cases.push({
      prUrl,
      candidates: result.total_candidates || 0,
      golden: result.total_golden || 0,
      tp: result.tp || 0,
      fp: result.fp || 0,
      fn: result.fn || 0,
      errors: result.errors_count || 0,
    });
  }
  const precision = totals.tp + totals.fp > 0 ? totals.tp / (totals.tp + totals.fp) : 0;
  const recall = totals.tp + totals.fn > 0 ? totals.tp / (totals.tp + totals.fn) : 0;
  const f1 = precision + recall > 0 ? (2 * precision * recall) / (precision + recall) : 0;
  return {
    ...totals,
    precision,
    recall,
    f1,
    cases,
  };
}

function spawnChecked(command, commandArgs, options) {
  return new Promise((resolve, reject) => {
    process.stderr.write(`[full-circle] ${command} ${commandArgs.join(" ")}\n`);
    const child = spawn(command, commandArgs, {
      cwd: process.cwd(),
      stdio: "inherit",
      ...options,
    });
    child.on("error", reject);
    child.on("exit", (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`${command} exited with ${signal || code}`));
    });
  });
}

function printConfig(value) {
  process.stderr.write(`[full-circle] ${JSON.stringify(value, null, 2)}\n`);
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function sanitizeLabel(value) {
  return String(value)
    .trim()
    .replaceAll("/", "_")
    .replace(/[^a-zA-Z0-9_.-]+/g, "-");
}

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
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
  node bench/review-quality/tools/run-muzen-martian-full-circle.mjs [options]

Options:
  --limit <n>               Number of Martian cases to review and judge (default: 5)
  --concurrency <n>         Concurrent Muzen review jobs (default: 2)
  --model <model>           Muzen review model (default: MODEL or gpt-5.5)
  --judge-model <model>     Official step 3 judge model (default: MARTIAN_JUDGE_MODEL or gpt-5.4-mini)
  --output-root <dir>       Output root for reviews, evaluations, and summary
  --case-source <file>      Source summary with Martian PR URLs
  --golden-dir <dir>        Muzen golden directory
  --worktree-root <dir>     Materialized Martian worktree root
  --runner-path <file>      muzen-runner path (default: target/release/muzen-runner)
  --benchmark-root <dir>    code-review-benchmark checkout (default: ${DEFAULT_BENCHMARK_ROOT})
  --skip-build true         Do not run cargo build first
`);
}
