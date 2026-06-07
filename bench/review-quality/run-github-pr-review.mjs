#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const DEFAULT_WORKTREE_ROOT = "/tmp/muzen-review-quality-worktrees";

function main() {
  const args = parseArgs(process.argv.slice(2));
  const repoSlug = required(args.repoSlug, "--repo-slug is required, e.g. calcom/cal.com");
  const prNumber = required(args.pr, "--pr is required");
  const runnerPath = args.runnerPath || "target/release/muzen";
  const golden = args.golden || `bench/review-quality/goldens/cal-pr-${prNumber}.json`;
  const worktreeRoot = path.resolve(args.worktreeRoot || DEFAULT_WORKTREE_ROOT);
  const worktree = path.join(worktreeRoot, safeName(`${repoSlug}-pr-${prNumber}`));
  const metadata = githubPr(repoSlug, prNumber);
  fs.mkdirSync(worktreeRoot, { recursive: true });
  ensureWorktree(repoSlug, worktree);
  git(worktree, ["fetch", "origin", metadata.baseSha, metadata.headSha]);
  const baseRef = `muzen-bench/pr-${prNumber}-base`;
  git(worktree, ["branch", "-f", baseRef, metadata.baseSha]);
  git(worktree, ["checkout", "--detach", metadata.headSha]);

  const output =
    args.output ||
    `bench/results-review-quality/cal-pr-${prNumber}-${timestamp()}.json`;
  const harnessArgs = [
    "bench/review-quality/run-production-review.mjs",
    "--repo",
    worktree,
    "--base-ref",
    baseRef,
    "--runner-path",
    runnerPath,
    "--golden",
    golden,
    "--sessions",
    args.sessions || "11",
    "--max-active",
    args.maxActive || "4",
    "--max-turns",
    args.maxTurns || "7",
    "--max-tool-calls",
    args.maxToolCalls || "14",
    "--output",
    output,
  ];
  if (args.model) {
    harnessArgs.push("--model", args.model);
  }
  const run = spawnSync("node", harnessArgs, {
    cwd: process.cwd(),
    encoding: "utf8",
    env: process.env,
    maxBuffer: 1024 * 1024 * 256,
  });
  process.stdout.write(run.stdout || "");
  process.stderr.write(run.stderr || "");
  process.exitCode = run.status ?? 1;
}

function ensureWorktree(repoSlug, worktree) {
  if (fs.existsSync(path.join(worktree, ".git"))) return;
  fs.rmSync(worktree, { recursive: true, force: true });
  git(process.cwd(), [
    "clone",
    "--no-checkout",
    "--filter=blob:none",
    `https://github.com/${repoSlug}.git`,
    worktree,
  ]);
}

function githubPr(repoSlug, prNumber) {
  const result = spawnSync(
    "gh",
    [
      "api",
      `repos/${repoSlug}/pulls/${prNumber}`,
      "--jq",
      "{baseRef:.base.ref,baseSha:.base.sha,headSha:.head.sha,title:.title,state:.state}",
    ],
    { encoding: "utf8" },
  );
  if (result.status !== 0) {
    throw new Error(`gh api failed: ${result.stderr || result.stdout}`);
  }
  return JSON.parse(result.stdout);
}

function git(cwd, args) {
  const result = spawnSync("git", args, {
    cwd,
    encoding: "utf8",
    maxBuffer: 1024 * 1024 * 64,
  });
  if (result.status !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr || result.stdout}`);
  }
  return result.stdout;
}

function parseArgs(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      if (index + 1 >= argv.length) throw new Error(`${arg} requires a value`);
      return argv[++index];
    };
    if (arg === "--repo-slug") args.repoSlug = next();
    else if (arg === "--pr") args.pr = next();
    else if (arg === "--runner-path") args.runnerPath = next();
    else if (arg === "--golden") args.golden = next();
    else if (arg === "--worktree-root") args.worktreeRoot = next();
    else if (arg === "--sessions") args.sessions = next();
    else if (arg === "--max-active") args.maxActive = next();
    else if (arg === "--max-turns") args.maxTurns = next();
    else if (arg === "--max-tool-calls") args.maxToolCalls = next();
    else if (arg === "--model") args.model = next();
    else if (arg === "--output") args.output = next();
    else throw new Error(`unknown argument: ${arg}`);
  }
  return args;
}

function required(value, message) {
  if (!value) throw new Error(message);
  return value;
}

function safeName(value) {
  return value.replace(/[^A-Za-z0-9._-]+/g, "-");
}

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
}

main();
