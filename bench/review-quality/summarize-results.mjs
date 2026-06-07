#!/usr/bin/env node

import fs from "node:fs";

const files = process.argv.slice(2);
if (files.length === 0) {
  throw new Error("usage: node bench/review-quality/summarize-results.mjs <result.json>...");
}

const rows = files.map((file) => {
  const result = JSON.parse(fs.readFileSync(file, "utf8"));
  const toolCalls = result.review?.toolCounts
    ? Object.values(result.review.toolCounts).reduce((sum, count) => sum + count, 0)
    : null;
  return {
    file,
    model: result.inputs?.model ?? null,
    changed: result.inputs?.changedFileCount ?? null,
    goldens: result.inputs?.goldenIssueCount ?? null,
    hitRate: result.benchmark?.hitRate ?? null,
    hits: result.benchmark?.hits?.length ?? null,
    misses: result.benchmark?.misses?.length ?? null,
    falsePositives: result.benchmark?.falsePositiveCount ?? null,
    findings: result.review?.findings ?? null,
    candidates: result.benchmark?.candidateCount ?? null,
    rejected: result.benchmark?.rejectedCandidateCount ?? null,
    needsReview: result.benchmark?.needsReviewCount ?? null,
    modelCalls: result.review?.modelCalls ?? null,
    toolCalls,
    totalTokens: result.review?.tokens?.totalTokens ?? null,
  };
});

console.table(rows);
