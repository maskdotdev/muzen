import { readFile } from "node:fs/promises";

import {
  expectationFromGoldenRecord,
  findGoldenRecord,
  qualityUrlForExpectation,
  type GoldenReviewRecord,
  type ReviewQualityReport,
} from "./review-quality.js";

const [reviewId, prUrl, goldenPath, baseUrl = "http://localhost:4077"] =
  process.argv.slice(2);

if (!reviewId || !prUrl || !goldenPath) {
  console.error(
    "usage: tsx src/server/score-quality.ts <review-id> <pr-url> <golden-json-path> [base-url]",
  );
  process.exit(2);
}

const records = JSON.parse(await readFile(goldenPath, "utf8")) as GoldenReviewRecord[];
const record = findGoldenRecord(records, prUrl);
if (!record) {
  console.error(`golden record not found for ${prUrl}`);
  process.exit(2);
}

const url = qualityUrlForExpectation(
  baseUrl,
  reviewId,
  expectationFromGoldenRecord(record),
);
const response = await fetch(url);
if (!response.ok) {
  console.error(`quality endpoint failed: ${response.status} ${await response.text()}`);
  process.exit(1);
}

const payload = (await response.json()) as { quality: ReviewQualityReport };
console.log(JSON.stringify(payload.quality, null, 2));
process.exit(payload.quality.passed ? 0 : 1);
