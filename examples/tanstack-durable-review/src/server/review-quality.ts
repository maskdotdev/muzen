import type { ReviewEvent, ReviewFinding, ReviewResult } from "@muzen/sdk";

export interface ReviewQualityExpectation {
  requiredIssuePhrases?: string[];
  changedFiles?: string[];
}

export interface ReviewQualityReport {
  passed: boolean;
  failures: string[];
  metrics: {
    changedFiles: number;
    duplicateFileVerdicts: number;
    fileReviews: number;
    findings: number;
    matchedRequiredIssues: number;
    missingFileVerdicts: number;
    missingRequiredIssues: number;
    modelFailures: number;
    speculativeFindings: number;
    failedSessions: number;
    failedTools: number;
    verdictMismatches: number;
  };
}

export interface GoldenReviewRecord {
  comments: Array<{ comment: string }>;
  url: string;
}

interface FileReview {
  findingId?: string;
  path: string;
  verdict: string;
}

interface ModelFailure {
  attempt: number;
  message: string;
  retrying: boolean;
  sessionId: string;
}

interface FailedTool {
  errorCode: string;
  message?: string;
  sessionId: string;
  toolId: string;
}

export function evaluateReviewQuality(
  result: ReviewResult | undefined,
  events: ReviewEvent[],
  expectation: ReviewQualityExpectation = {},
): ReviewQualityReport {
  const failures: string[] = [];
  const changedFiles = new Set(
    expectation.changedFiles && expectation.changedFiles.length > 0
      ? expectation.changedFiles
      : changedFilesFromEvents(events),
  );
  const fileReviews = fileReviewsFromEvents(events);
  const failedSessions = failedSessionsFromEvents(events);
  const failedTools = failedToolsFromEvents(events, fileReviews);
  const modelFailures = modelFailuresFromEvents(events);
  const findings = result?.findings ?? [];
  const speculativeFindings = findings.filter(isSpeculativeFinding);
  const verdictMismatches = findingVerdictConsistencyFailures(findings, fileReviews);
  const duplicateFileVerdicts = duplicateFileVerdictsFor(fileReviews);
  const missingFileVerdicts = missingFileVerdictsFor(changedFiles, fileReviews);
  const requiredIssuePhrases = expectation.requiredIssuePhrases ?? [];
  const matchedRequiredIssues = requiredIssuePhrases.filter((phrase) =>
    findings.some((finding) => findingMatchesPhrase(finding, phrase)),
  );

  if (!result) {
    failures.push("review did not produce a result");
  } else if (result.status !== "completed") {
    failures.push(`review status is ${result.status}`);
  }
  for (const session of failedSessions) {
    failures.push(`${session.id} ended with status ${session.status}`);
  }
  for (const tool of failedTools) {
    failures.push(
      `${tool.sessionId} ${tool.toolId} failed (${tool.errorCode})${tool.message ? `: ${tool.message}` : ""}`,
    );
  }
  for (const failure of modelFailures.filter((failure) => !failure.retrying)) {
    failures.push(
      `${failure.sessionId} model failed attempt ${failure.attempt}: ${failure.message}`,
    );
  }
  for (const finding of speculativeFindings) {
    failures.push(`speculative finding: ${finding.title}`);
  }
  for (const phrase of requiredIssuePhrases) {
    if (!matchedRequiredIssues.includes(phrase)) {
      failures.push(`missing required issue phrase: ${phrase}`);
    }
  }
  for (const path of missingFileVerdicts) {
    failures.push(`missing file-review verdict: ${path}`);
  }
  for (const path of duplicateFileVerdicts) {
    failures.push(`duplicate file-review verdict: ${path}`);
  }
  for (const failure of verdictMismatches) {
    failures.push(failure);
  }

  return {
    passed: failures.length === 0,
    failures,
    metrics: {
      changedFiles: changedFiles.size,
      duplicateFileVerdicts: duplicateFileVerdicts.length,
      fileReviews: fileReviews.length,
      findings: findings.length,
      matchedRequiredIssues: matchedRequiredIssues.length,
      missingFileVerdicts: missingFileVerdicts.length,
      missingRequiredIssues:
        requiredIssuePhrases.length - matchedRequiredIssues.length,
      modelFailures: modelFailures.filter((failure) => !failure.retrying).length,
      speculativeFindings: speculativeFindings.length,
      failedSessions: failedSessions.length,
      failedTools: failedTools.length,
      verdictMismatches: verdictMismatches.length,
    },
  };
}

function duplicateFileVerdictsFor(fileReviews: FileReview[]): string[] {
  const counts = new Map<string, number>();
  for (const review of fileReviews) {
    counts.set(review.path, (counts.get(review.path) ?? 0) + 1);
  }
  return [...counts.entries()]
    .filter(([, count]) => count > 1)
    .map(([path]) => path)
    .sort();
}

function missingFileVerdictsFor(
  changedFiles: Set<string>,
  fileReviews: FileReview[],
): string[] {
  if (changedFiles.size === 0) {
    return [];
  }
  const reviewedPaths = new Set(fileReviews.map((review) => review.path));
  return [...changedFiles].filter((path) => !reviewedPaths.has(path));
}

function changedFilesFromEvents(events: ReviewEvent[]): string[] {
  const changedFiles = new Set<string>();
  for (const event of events) {
    const payload = asRecord(event.payload);
    const artifact = asRecord(payload?.artifactCreated);
    if (stringValue(artifact?.toolId) !== "list_changed_files") {
      continue;
    }
    const details = asRecord(artifact?.details);
    for (const entry of stringArray(details?.changedFiles)) {
      const path = changedFilePath(entry);
      if (path) {
        changedFiles.add(path);
      }
    }
  }
  return [...changedFiles].sort();
}

export function expectationFromGoldenRecord(
  record: GoldenReviewRecord,
): ReviewQualityExpectation {
  return {
    requiredIssuePhrases: record.comments
      .map((comment) => comment.comment.trim())
      .filter(Boolean),
  };
}

export function findGoldenRecord(
  records: GoldenReviewRecord[],
  prUrl: string,
): GoldenReviewRecord | undefined {
  const normalizedTarget = normalizePrUrl(prUrl);
  return records.find((record) => normalizePrUrl(record.url) === normalizedTarget);
}

export function qualityUrlForExpectation(
  baseUrl: string,
  reviewId: string,
  expectation: ReviewQualityExpectation,
): string {
  const url = new URL(
    `/api/reviews/${encodeURIComponent(reviewId)}/quality`,
    baseUrl,
  );
  for (const phrase of expectation.requiredIssuePhrases ?? []) {
    url.searchParams.append("requiredIssuePhrase", phrase);
  }
  for (const file of expectation.changedFiles ?? []) {
    url.searchParams.append("changedFile", file);
  }
  return url.toString();
}

function fileReviewsFromEvents(events: ReviewEvent[]): FileReview[] {
  const reviews: FileReview[] = [];
  for (const event of events) {
    const payload = asRecord(event.payload);
    const artifact = asRecord(payload?.artifactCreated);
    if (stringValue(artifact?.toolId) !== "record_file_review") {
      continue;
    }
    const details = asRecord(artifact?.details);
    const path = stringValue(details?.path);
    const verdict = stringValue(details?.verdict);
    if (path && verdict) {
      reviews.push({
        findingId: stringValue(details?.findingId),
        path,
        verdict,
      });
    }
  }
  return reviews;
}

function findingVerdictConsistencyFailures(
  findings: ReviewFinding[],
  fileReviews: FileReview[],
): string[] {
  const failures: string[] = [];
  const findingsById = new Map(findings.map((finding) => [finding.id, finding]));
  const findingIds = new Set(findingsById.keys());
  const issueReviews = fileReviews.filter((review) => review.verdict === "issue_found");
  for (const finding of findings) {
    const path = finding.location?.path;
    if (!path) {
      continue;
    }
    const matchingReview = issueReviews.find(
      (review) => review.path === path && review.findingId === finding.id,
    );
    if (!matchingReview) {
      failures.push(
        `finding ${finding.id} lacks matching issue_found file-review verdict: ${path}`,
      );
    }
  }
  for (const review of issueReviews) {
    if (!review.findingId) {
      failures.push(`issue_found file-review verdict missing finding_id: ${review.path}`);
      continue;
    }
    if (!findingIds.has(review.findingId)) {
      failures.push(
        `issue_found file-review verdict references unknown finding ${review.findingId}: ${review.path}`,
      );
      continue;
    }
    const findingPath = findingsById.get(review.findingId)?.location?.path;
    if (findingPath && findingPath !== review.path) {
      failures.push(
        `issue_found file-review verdict references finding ${review.findingId} on ${findingPath}, not ${review.path}`,
      );
    }
  }
  for (const review of fileReviews) {
    if (review.verdict === "clean") {
      const finding = findings.find((item) => item.location?.path === review.path);
      if (finding) {
        failures.push(
          `clean file-review verdict conflicts with finding ${finding.id}: ${review.path}`,
        );
      }
    }
  }
  return failures;
}

function failedSessionsFromEvents(
  events: ReviewEvent[],
): Array<{ id: string; status: string }> {
  const failed: Array<{ id: string; status: string }> = [];
  for (const event of events) {
    const payload = asRecord(event.payload);
    const session = asRecord(payload?.sessionFinished);
    const status = stringValue(session?.status);
    if (!status || status === "done") {
      continue;
    }
    failed.push({
      id: stringValue(session?.sessionId) ?? "unknown",
      status,
    });
  }
  return failed;
}

function modelFailuresFromEvents(events: ReviewEvent[]): ModelFailure[] {
  const failures: ModelFailure[] = [];
  for (const event of events) {
    const payload = asRecord(event.payload);
    const failure = asRecord(payload?.modelFailed);
    if (!failure) {
      continue;
    }
    const context = asRecord(payload?.context);
    failures.push({
      attempt: numberValue(failure.attempt),
      message: stringValue(failure.message) ?? "model provider error",
      retrying: failure.retrying === true,
      sessionId:
        stringValue(failure.sessionId) ??
        stringValue(context?.sessionId) ??
        "unknown",
    });
  }
  return failures;
}

function failedToolsFromEvents(
  events: ReviewEvent[],
  fileReviews: FileReview[],
): FailedTool[] {
  const failed: FailedTool[] = [];
  const skippedPaths = new Set(
    fileReviews
      .filter((review) => review.verdict === "skipped")
      .map((review) => review.path),
  );
  for (const event of events) {
    const payload = asRecord(event.payload);
    const tool = asRecord(payload?.toolCallCompleted);
    if (tool?.ok !== false) {
      continue;
    }
    const toolId = stringValue(tool.toolId) ?? "unknown";
    const errorCode = stringValue(tool.errorCode) ?? "tool_error";
    const details = asRecord(tool.details);
    const path = stringValue(details?.path);
    if (
      path &&
      skippedPaths.has(path) &&
      isReadTool(toolId) &&
      isUninspectableReadError(errorCode)
    ) {
      continue;
    }
    const context = asRecord(payload?.context);
    failed.push({
      errorCode,
      message: stringValue(tool.errorMessage),
      sessionId: stringValue(context?.sessionId) ?? "unknown",
      toolId,
    });
  }
  return failed;
}

function isReadTool(toolId: string): boolean {
  return [
    "read_base_file",
    "read_file",
    "read_file_range",
    "read_head_file",
  ].includes(toolId);
}

function isUninspectableReadError(errorCode: string): boolean {
  return ["not_found", "not_text", "path_denied", "too_large"].includes(errorCode);
}

function changedFilePath(entry: string): string | undefined {
  const path = [
    "Added ",
    "Copied ",
    "Deleted ",
    "Modified ",
    "Renamed ",
    "TypeChanged ",
  ].reduce((value, prefix) => value.replace(prefix, ""), entry).trim();
  return path.length > 0 ? path : undefined;
}

function isSpeculativeFinding(finding: ReviewFinding): boolean {
  const text = `${finding.title} ${finding.message}`.toLowerCase();
  return [
    "potential ",
    "possibly ",
    "may be ",
    "may have ",
    "might ",
    "verify ",
    "risk that",
    "if callers",
    "if users",
    "hypothetical",
  ].some((phrase) => text.includes(phrase));
}

function findingMatchesPhrase(finding: ReviewFinding, phrase: string): boolean {
  const phraseWords = significantWords(phrase);
  if (phraseWords.length === 0) {
    return true;
  }
  const text = normalizeText(`${finding.title} ${finding.message}`);
  const matched = phraseWords.filter((word) => text.includes(word));
  const ratio = matched.length / phraseWords.length;
  return ratio >= 0.6 || (matched.length >= 6 && ratio >= 0.3);
}

function normalizeText(value: string): string {
  return value
    .toLowerCase()
    .replace(/\bssrf\b/g, "server side request forgery")
    .replace(/[^a-z0-9_]+/g, " ")
    .trim();
}

function normalizePrUrl(value: string): string {
  return value.trim().replace(/\/+$/, "").toLowerCase();
}

function significantWords(value: string): string[] {
  const stopWords = new Set([
    "and",
    "are",
    "for",
    "from",
    "into",
    "that",
    "the",
    "this",
    "using",
    "with",
    "without",
  ]);
  return normalizeText(value)
    .split(/\s+/)
    .filter((word) => word.length > 2 && !stopWords.has(word));
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : undefined;
}

function numberValue(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function stringArray(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}
