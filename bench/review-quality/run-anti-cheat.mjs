#!/usr/bin/env node

// Negative-control benchmark leg. Each fixture under bench/review-quality/anti-cheat
// mirrors the diff shape of a golden benchmark bug but with a safe implementation.
// The reviewer must produce zero findings on every fixture; any finding is a
// keyword-shaped false positive and fails the leg.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const FIXTURES_ROOT = "bench/review-quality/anti-cheat";

const args = parseArgs(process.argv.slice(2));
const runnerPath = args.runnerPath || "target/release/muzen-runner";
const outputDir = args.outputDir || "bench/results-review-quality/anti-cheat";
const model = args.model || process.env.MODEL || "gpt-5.4-mini";

fs.mkdirSync(outputDir, { recursive: true });
const fixtureIds = fs
  .readdirSync(FIXTURES_ROOT, { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name)
  .sort();

const results = [];
for (const fixtureId of fixtureIds) {
  const fixtureDir = path.join(FIXTURES_ROOT, fixtureId);
  const repo = materializeFixtureRepo(fixtureDir, fixtureId);
  const output = path.join(outputDir, `${fixtureId}-${model}.json`);
  const run = spawnSync(
    "node",
    [
      "bench/review-quality/run-production-review.mjs",
      "--repo",
      repo,
      "--base-ref",
      "HEAD~1",
      "--runner-path",
      runnerPath,
      "--model",
      model,
      "--output",
      output,
    ],
    {
      cwd: process.cwd(),
      env: process.env,
      encoding: "utf8",
      maxBuffer: 1024 * 1024 * 128,
    },
  );
  if (run.status !== 0) {
    process.stderr.write(run.stderr || run.stdout);
    process.exit(run.status ?? 1);
  }
  const report = JSON.parse(fs.readFileSync(output, "utf8"));
  const findings = report.findings || [];
  results.push({
    fixture: fixtureId,
    output,
    findings: findings.length,
    findingTitles: findings.map((finding) => finding.title),
    contractPackCount: report.benchmark?.contractPackCount || 0,
    passed: findings.length === 0,
  });
}

const summary = {
  generatedAtUtc: new Date().toISOString(),
  model,
  runnerPath,
  outputDir,
  passed: results.every((result) => result.passed),
  results,
};
const summaryPath = path.join(outputDir, `summary-${model}.json`);
fs.writeFileSync(summaryPath, `${JSON.stringify(summary, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`);
process.exitCode = summary.passed ? 0 : 1;

function materializeFixtureRepo(fixtureDir, fixtureId) {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), `muzen-anti-cheat-${fixtureId}-`));
  git(repo, ["init", "--quiet", "--initial-branch=main"]);
  copyTree(path.join(fixtureDir, "base"), repo);
  git(repo, ["add", "--all"]);
  commit(repo, "base");
  copyTree(path.join(fixtureDir, "head"), repo);
  git(repo, ["add", "--all"]);
  commit(repo, "head");
  return repo;
}

function copyTree(source, destination) {
  fs.cpSync(source, destination, { recursive: true });
}

function commit(repo, message) {
  git(repo, [
    "-c",
    "user.email=bench@muzen.invalid",
    "-c",
    "user.name=muzen-bench",
    "commit",
    "--quiet",
    "--no-verify",
    "-m",
    message,
  ]);
}

function git(cwd, gitArgs) {
  const result = spawnSync("git", gitArgs, { cwd, encoding: "utf8" });
  if (result.status !== 0) {
    throw new Error(`git ${gitArgs.join(" ")} failed: ${result.stderr || result.stdout}`);
  }
  return result.stdout;
}

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
