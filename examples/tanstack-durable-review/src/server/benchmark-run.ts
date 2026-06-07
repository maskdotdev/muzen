import { readFile } from "node:fs/promises";

import type { ReviewRole } from "@muzen/sdk";

import type { CreateReviewRequest, ReviewQualityReport, ReviewSnapshot } from "../shared.js";
import {
  expectationFromGoldenRecord,
  findGoldenRecord,
  qualityUrlForExpectation,
  type GoldenReviewRecord,
} from "./review-quality.js";

export interface BenchmarkRunOptions {
  baseUrl: string;
  changedFiles: string[];
  goldenPath: string;
  maxActiveSessions: number;
  pollIntervalMs: number;
  prUrl: string;
  roles: ReviewRole[];
  skipPreflight: boolean;
  timeoutMs: number;
}

interface CreateReviewResponse {
  review: ReviewSnapshot;
}

interface QualityResponse {
  quality: ReviewQualityReport;
}

interface ProviderFailurePayload {
  error?: string;
  model?: string;
  provider?: string;
  status?: number;
}

const defaultOptions: Pick<
  BenchmarkRunOptions,
  | "baseUrl"
  | "changedFiles"
  | "maxActiveSessions"
  | "pollIntervalMs"
  | "roles"
  | "skipPreflight"
  | "timeoutMs"
> = {
  baseUrl: "http://localhost:4077",
  changedFiles: [],
  maxActiveSessions: 4,
  pollIntervalMs: 5_000,
  roles: ["generalist"],
  skipPreflight: false,
  timeoutMs: 30 * 60_000,
};

export function parseBenchmarkRunArgs(args: string[]): BenchmarkRunOptions {
  const options: Partial<BenchmarkRunOptions> = { ...defaultOptions };
  let explicitRoles = false;
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    const next = () => {
      const value = args[index + 1];
      if (!value) {
        throw new Error(`${arg} requires a value`);
      }
      index += 1;
      return value;
    };
    switch (arg) {
      case "--base-url":
        options.baseUrl = next();
        break;
      case "--changed-file":
        options.changedFiles = [...(options.changedFiles ?? []), next()];
        break;
      case "--golden":
        options.goldenPath = next();
        break;
      case "--max-active-sessions":
        options.maxActiveSessions = boundedInteger(next(), arg, 1, 8);
        break;
      case "--poll-ms":
        options.pollIntervalMs = positiveInteger(next(), arg);
        break;
      case "--pr":
        options.prUrl = next();
        break;
      case "--role":
        options.roles = [
          ...(explicitRoles ? (options.roles ?? []) : []),
          next() as ReviewRole,
        ];
        explicitRoles = true;
        break;
      case "--skip-preflight":
        options.skipPreflight = true;
        break;
      case "--timeout-ms":
        options.timeoutMs = positiveInteger(next(), arg);
        break;
      default:
        throw new Error(`unknown argument ${arg}`);
    }
  }
  if (!options.prUrl) {
    throw new Error("--pr is required");
  }
  if (!options.goldenPath) {
    throw new Error("--golden is required");
  }
  return options as BenchmarkRunOptions;
}

export function createReviewRequest(options: BenchmarkRunOptions): CreateReviewRequest {
  return {
    sourceKind: "github",
    githubPullRequest: options.prUrl,
    changedFiles: options.changedFiles,
    maxActiveSessions: options.maxActiveSessions,
    roles: options.roles,
  };
}

export async function runBenchmarkReview(options: BenchmarkRunOptions): Promise<{
  quality: ReviewQualityReport;
  review: ReviewSnapshot;
}> {
  if (!options.skipPreflight) {
    await assertModelPreflight(options.baseUrl);
  }
  const review = await createReview(options);
  const terminal = await waitForTerminalReview(options.baseUrl, review.id, {
    pollIntervalMs: options.pollIntervalMs,
    timeoutMs: options.timeoutMs,
  });
  const quality = await fetchQuality(options, terminal.id);
  return { review: terminal, quality };
}

export function formatBenchmarkSummary(result: {
  quality: ReviewQualityReport;
  review: ReviewSnapshot;
}): string {
  const { quality, review } = result;
  const status = quality.passed ? "PASS" : "FAIL";
  const lines = [
    `Benchmark quality: ${status}`,
    `Review: ${review.id} (${review.status})`,
    [
      `changed=${quality.metrics.changedFiles}`,
      `findings=${quality.metrics.findings}`,
      `fileVerdicts=${quality.metrics.fileReviews}`,
      `missingVerdicts=${quality.metrics.missingFileVerdicts}`,
      `duplicateVerdicts=${quality.metrics.duplicateFileVerdicts}`,
      `mismatches=${quality.metrics.verdictMismatches}`,
      `speculative=${quality.metrics.speculativeFindings}`,
      `modelFailures=${quality.metrics.modelFailures}`,
      `failedTools=${quality.metrics.failedTools}`,
    ].join(" "),
  ];
  if (quality.failures.length > 0) {
    lines.push("Top failures:");
    for (const failure of quality.failures.slice(0, 8)) {
      lines.push(`- ${failure}`);
    }
  }
  return lines.join("\n");
}

export class BenchmarkRunFailure extends Error {
  readonly body: string;
  readonly status: number;
  readonly step: "create_review" | "model_preflight";
  readonly providerFailure?: ProviderFailurePayload;

  constructor(
    step: "create_review" | "model_preflight",
    status: number,
    body: string,
  ) {
    const providerFailure = parseProviderFailurePayload(body);
    super(formatBenchmarkFailureSummary(step, status, providerFailure, body));
    this.name = "BenchmarkRunFailure";
    this.body = body;
    this.status = status;
    this.step = step;
    this.providerFailure = providerFailure;
  }
}

export function formatBenchmarkFailureSummary(
  step: "create_review" | "model_preflight",
  status: number,
  payload: ProviderFailurePayload | undefined,
  body: string,
): string {
  const lines = [
    "Benchmark quality: FAIL",
    `Step: ${step}`,
    `HTTP status: ${status}`,
  ];
  const providerLabel = [payload?.provider, payload?.model].filter(Boolean).join(" ");
  if (providerLabel) {
    lines.push(`Model: ${providerLabel}`);
  }
  if (payload?.status) {
    lines.push(`Provider status: ${payload.status}`);
  }
  lines.push(`Failure: ${payload?.error ?? truncate(body, 1_000)}`);
  return lines.join("\n");
}

export async function assertModelPreflight(baseUrl: string): Promise<void> {
  const response = await fetch(new URL("/api/model/preflight", baseUrl));
  const text = await response.text();
  if (!response.ok) {
    throw new BenchmarkRunFailure("model_preflight", response.status, text);
  }
}

async function createReview(options: BenchmarkRunOptions): Promise<ReviewSnapshot> {
  const response = await fetch(new URL("/api/reviews", options.baseUrl), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(createReviewRequest(options)),
  });
  if (!response.ok) {
    throw new BenchmarkRunFailure(
      "create_review",
      response.status,
      await response.text(),
    );
  }
  return ((await response.json()) as CreateReviewResponse).review;
}

async function waitForTerminalReview(
  baseUrl: string,
  reviewId: string,
  options: Pick<BenchmarkRunOptions, "pollIntervalMs" | "timeoutMs">,
): Promise<ReviewSnapshot> {
  const started = Date.now();
  while (Date.now() - started <= options.timeoutMs) {
    const response = await fetch(new URL(`/api/reviews/${reviewId}`, baseUrl));
    if (!response.ok) {
      throw new Error(`fetch review failed: ${response.status} ${await response.text()}`);
    }
    const review = ((await response.json()) as CreateReviewResponse).review;
    if (review.status === "completed" || review.status === "failed") {
      return review;
    }
    await sleep(options.pollIntervalMs);
  }
  throw new Error(`timed out waiting for review ${reviewId}`);
}

async function fetchQuality(
  options: BenchmarkRunOptions,
  reviewId: string,
): Promise<ReviewQualityReport> {
  const records = JSON.parse(await readFile(options.goldenPath, "utf8")) as GoldenReviewRecord[];
  const record = findGoldenRecord(records, options.prUrl);
  if (!record) {
    throw new Error(`golden record not found for ${options.prUrl}`);
  }
  const response = await fetch(
    qualityUrlForExpectation(
      options.baseUrl,
      reviewId,
      expectationFromGoldenRecord(record),
    ),
  );
  if (!response.ok) {
    throw new Error(`quality endpoint failed: ${response.status} ${await response.text()}`);
  }
  return ((await response.json()) as QualityResponse).quality;
}

function positiveInteger(value: string, flag: string): number {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error(`${flag} must be a positive integer`);
  }
  return parsed;
}

function boundedInteger(value: string, flag: string, min: number, max: number): number {
  const parsed = positiveInteger(value, flag);
  if (parsed < min || parsed > max) {
    throw new Error(`${flag} must be an integer from ${min} to ${max}`);
  }
  return parsed;
}

function parseProviderFailurePayload(body: string): ProviderFailurePayload | undefined {
  try {
    const parsed = JSON.parse(body) as ProviderFailurePayload;
    return typeof parsed === "object" && parsed !== null ? parsed : undefined;
  } catch {
    return undefined;
  }
}

function truncate(value: string, maxLength: number): string {
  return value.length <= maxLength ? value : `${value.slice(0, maxLength)}[truncated]`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    const result = await runBenchmarkReview(parseBenchmarkRunArgs(process.argv.slice(2)));
    console.log(formatBenchmarkSummary(result));
    console.log(JSON.stringify(result, null, 2));
    process.exit(result.quality.passed ? 0 : 1);
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  }
}
