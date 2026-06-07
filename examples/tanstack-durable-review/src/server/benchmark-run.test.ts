import assert from "node:assert/strict";
import test from "node:test";

import type { ReviewQualityReport } from "../shared.js";

import {
  BenchmarkRunFailure,
  createReviewRequest,
  formatBenchmarkFailureSummary,
  formatBenchmarkSummary,
  parseBenchmarkRunArgs,
} from "./benchmark-run.js";

test("parses benchmark run arguments", () => {
  const options = parseBenchmarkRunArgs([
    "--pr",
    "https://github.com/org/repo/pull/4",
    "--golden",
    "/tmp/golden.json",
    "--base-url",
    "http://localhost:4106",
    "--max-active-sessions",
    "8",
    "--role",
    "security",
    "--changed-file",
    "src/a.ts",
    "--poll-ms",
    "100",
    "--timeout-ms",
    "1000",
  ]);

  assert.deepEqual(options, {
    baseUrl: "http://localhost:4106",
    changedFiles: ["src/a.ts"],
    goldenPath: "/tmp/golden.json",
    maxActiveSessions: 8,
    pollIntervalMs: 100,
    prUrl: "https://github.com/org/repo/pull/4",
    roles: ["security"],
    skipPreflight: false,
    timeoutMs: 1000,
  });
});

test("requires benchmark run essentials", () => {
  assert.throws(
    () => parseBenchmarkRunArgs(["--golden", "/tmp/golden.json"]),
    /--pr is required/,
  );
  assert.throws(
    () => parseBenchmarkRunArgs(["--pr", "https://github.com/org/repo/pull/4"]),
    /--golden is required/,
  );
  assert.throws(
    () =>
      parseBenchmarkRunArgs([
        "--pr",
        "https://github.com/org/repo/pull/4",
        "--golden",
        "/tmp/golden.json",
        "--poll-ms",
        "0",
      ]),
    /--poll-ms must be a positive integer/,
  );
  assert.throws(
    () =>
      parseBenchmarkRunArgs([
        "--pr",
        "https://github.com/org/repo/pull/4",
        "--golden",
        "/tmp/golden.json",
        "--max-active-sessions",
        "9",
      ]),
    /--max-active-sessions must be an integer from 1 to 8/,
  );
});

test("builds GitHub review creation request", () => {
  const options = parseBenchmarkRunArgs([
    "--pr",
    "https://github.com/org/repo/pull/4",
    "--golden",
    "/tmp/golden.json",
    "--changed-file",
    "src/a.ts",
  ]);

  assert.deepEqual(createReviewRequest(options), {
    sourceKind: "github",
    githubPullRequest: "https://github.com/org/repo/pull/4",
    changedFiles: ["src/a.ts"],
    maxActiveSessions: 4,
    roles: ["generalist"],
  });
});

test("parses explicit preflight skip", () => {
  const options = parseBenchmarkRunArgs([
    "--pr",
    "https://github.com/org/repo/pull/4",
    "--golden",
    "/tmp/golden.json",
    "--skip-preflight",
  ]);

  assert.equal(options.skipPreflight, true);
});

test("formats passing benchmark summaries", () => {
  assert.equal(
    formatBenchmarkSummary({
      review: reviewSnapshot("review-1", "completed"),
      quality: qualityReport(true, []),
    }),
    [
      "Benchmark quality: PASS",
      "Review: review-1 (completed)",
      "changed=2 findings=1 fileVerdicts=2 missingVerdicts=0 duplicateVerdicts=0 mismatches=0 speculative=0 modelFailures=0 failedTools=0",
    ].join("\n"),
  );
});

test("formats failing benchmark summaries with top failures", () => {
  const summary = formatBenchmarkSummary({
    review: reviewSnapshot("review-2", "failed"),
    quality: qualityReport(false, [
      "security model failed attempt 1: provider error",
      "missing file-review verdict: src/a.ts",
    ], {
      failedTools: 1,
      missingFileVerdicts: 1,
      modelFailures: 1,
      verdictMismatches: 2,
    }),
  });

  assert(summary.includes("Benchmark quality: FAIL"));
  assert(summary.includes("Review: review-2 (failed)"));
  assert(summary.includes("mismatches=2"));
  assert(summary.includes("missingVerdicts=1"));
  assert(summary.includes("modelFailures=1"));
  assert(summary.includes("- security model failed attempt 1: provider error"));
  assert(summary.includes("- missing file-review verdict: src/a.ts"));
});

test("formats preflight provider failures", () => {
  const summary = formatBenchmarkFailureSummary(
    "model_preflight",
    503,
    {
      error: "Model preflight failed for openai gpt-5.4-mini (status 429): insufficient_quota",
      model: "gpt-5.4-mini",
      provider: "openai",
      status: 429,
    },
    "{}",
  );

  assert.equal(
    summary,
    [
      "Benchmark quality: FAIL",
      "Step: model_preflight",
      "HTTP status: 503",
      "Model: openai gpt-5.4-mini",
      "Provider status: 429",
      "Failure: Model preflight failed for openai gpt-5.4-mini (status 429): insufficient_quota",
    ].join("\n"),
  );
});

test("benchmark run failures parse provider payloads", () => {
  const failure = new BenchmarkRunFailure(
    "create_review",
    503,
    JSON.stringify({
      error: "provider unavailable",
      model: "gpt-5.4-mini",
      provider: "openai",
      status: 401,
    }),
  );

  assert.equal(failure.step, "create_review");
  assert.equal(failure.status, 503);
  assert.equal(failure.providerFailure?.status, 401);
  assert(failure.message.includes("Benchmark quality: FAIL"));
  assert(failure.message.includes("Model: openai gpt-5.4-mini"));
  assert(failure.message.includes("Provider status: 401"));
});

function reviewSnapshot(id: string, status: "completed" | "failed") {
  return {
    id,
    status,
    source: {
      type: "github_pull_request",
      owner: "org",
      repo: "repo",
      number: 4,
    } as const,
    changedFiles: ["src/a.ts", "src/b.ts"],
  };
}

function qualityReport(
  passed: boolean,
  failures: string[],
  metrics: Partial<ReviewQualityReport["metrics"]> = {},
): ReviewQualityReport {
  return {
    passed,
    failures,
    metrics: {
      changedFiles: 2,
      duplicateFileVerdicts: 0,
      failedSessions: 0,
      failedTools: 0,
      fileReviews: 2,
      findings: 1,
      matchedRequiredIssues: 1,
      missingFileVerdicts: 0,
      missingRequiredIssues: 0,
      modelFailures: 0,
      speculativeFindings: 0,
      verdictMismatches: 0,
      ...metrics,
    },
  };
}
