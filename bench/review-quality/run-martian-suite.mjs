#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const CASES = [
  { name: "cal-pr-14943", repoSlug: "calcom/cal.com", pr: 14943, minHits: 1, maxFalsePositives: 0 },
  { name: "cal-pr-8330", repoSlug: "calcom/cal.com", pr: 8330, minHits: 2, maxFalsePositives: 1 },
  { name: "cal-pr-11059", repoSlug: "calcom/cal.com", pr: 11059, minHits: 3, maxFalsePositives: 1 },
];

const args = parseArgs(process.argv.slice(2));
const runnerPath = args.runnerPath || "target/release/muzen-runner";
const outputDir = args.outputDir || "bench/results-review-quality/martian-suite";
const model = args.model || process.env.MODEL || "gpt-5.4-mini";
const sessions = args.sessions || "0";
const maxActive = args.maxActive || "8";
const maxTurns = args.maxTurns || "10";
const maxToolCalls = args.maxToolCalls || "32";

fs.mkdirSync(outputDir, { recursive: true });
const results = [];

for (const testCase of CASES) {
  const output = path.join(outputDir, `${testCase.name}-${model}.json`);
  const golden = `bench/review-quality/goldens/${testCase.name}.json`;
  const command = [
    "bench/review-quality/run-github-pr-review.mjs",
    "--repo-slug",
    testCase.repoSlug,
    "--pr",
    String(testCase.pr),
    "--runner-path",
    runnerPath,
    "--golden",
    golden,
    "--sessions",
    sessions,
    "--max-active",
    maxActive,
    "--max-turns",
    maxTurns,
    "--max-tool-calls",
    maxToolCalls,
    "--output",
    output,
  ];
  const run = spawnSync("node", command, {
    cwd: process.cwd(),
    env: { ...process.env, MODEL: model },
    encoding: "utf8",
    maxBuffer: 1024 * 1024 * 128,
  });
  if (run.status !== 0) {
    process.stderr.write(run.stderr || run.stdout);
    process.exit(run.status ?? 1);
  }
  const result = JSON.parse(fs.readFileSync(output, "utf8"));
  const hits = result.benchmark?.hits?.length || 0;
  const falsePositives = result.benchmark?.falsePositiveCount || 0;
  const passed = hits >= testCase.minHits && falsePositives <= testCase.maxFalsePositives;
  results.push({
    name: testCase.name,
    output,
    hits,
    goldenIssues: result.inputs?.goldenIssueCount || 0,
    falsePositives,
    contractRiskUnits: result.benchmark?.contractRiskUnits || 0,
    contractPackCount: result.benchmark?.contractPackCount || 0,
    passed,
  });
}

const antiCheatRun = spawnSync(
  "node",
  [
    "bench/review-quality/run-anti-cheat.mjs",
    "--runner-path",
    runnerPath,
    "--model",
    model,
    "--output-dir",
    path.join(outputDir, "anti-cheat"),
  ],
  {
    cwd: process.cwd(),
    env: { ...process.env, MODEL: model },
    encoding: "utf8",
    maxBuffer: 1024 * 1024 * 128,
  },
);
if (antiCheatRun.stdout) process.stderr.write(antiCheatRun.stdout);
if (antiCheatRun.stderr) process.stderr.write(antiCheatRun.stderr);
const antiCheat = JSON.parse(
  fs.readFileSync(path.join(outputDir, "anti-cheat", `summary-${model}.json`), "utf8"),
);

const summary = {
  generatedAtUtc: new Date().toISOString(),
  model,
  runnerPath,
  outputDir,
  passed: results.every((result) => result.passed) && antiCheat.passed,
  results,
  antiCheat: {
    passed: antiCheat.passed,
    results: antiCheat.results,
  },
};
const summaryPath = path.join(outputDir, `summary-${model}.json`);
fs.writeFileSync(summaryPath, `${JSON.stringify(summary, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`);
process.exitCode = summary.passed ? 0 : 1;

function parseArgs(rawArgs) {
  const parsed = {};
  for (let index = 0; index < rawArgs.length; index += 1) {
    const arg = rawArgs[index];
    if (!arg.startsWith("--")) continue;
    parsed[arg.slice(2).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())] =
      rawArgs[index + 1];
    index += 1;
  }
  return parsed;
}
