import assert from "node:assert/strict";
import test from "node:test";

import type { ReviewEvent, ReviewFinding, ReviewResult } from "@muzen/sdk";

import {
  evaluateReviewQuality,
  expectationFromGoldenRecord,
  findGoldenRecord,
  qualityUrlForExpectation,
} from "./review-quality.js";

test("passes when required issue phrases and file verdicts are present", () => {
  const report = evaluateReviewQuality(
    completedResult([
      finding(
        "SSRF vulnerability using open(url)",
        "The new retriever calls open(url) with attacker-controlled embed URLs before validating the host.",
      ),
    ]),
    [
      fileReviewEvent("app/jobs/regular/retrieve_topic.rb", "issue_found", {
        findingId: "ssrf-vulnerability-using-open-url-",
      }),
    ],
    {
      changedFiles: ["app/jobs/regular/retrieve_topic.rb"],
      requiredIssuePhrases: ["SSRF vulnerability using open(url) without validation"],
    },
  );

  assert.equal(report.passed, true);
  assert.deepEqual(report.failures, []);
  assert.equal(report.metrics.matchedRequiredIssues, 1);
});

test("matches golden issue phrases by shared technical terms", () => {
  const report = evaluateReviewQuality(
    completedResult([
      finding(
        "Unsafe substring origin check in embed postMessage handler",
        "The handler uses discourseUrl.indexOf(e.origin) and lets a different origin pass the check.",
      ),
    ]),
    [
      fileReviewEvent("app/jobs/regular/retrieve_topic.rb", "issue_found", {
        findingId: "unsafe-substring-origin-check-in-embed-postmessage-handler-",
      }),
    ],
    {
      changedFiles: ["app/jobs/regular/retrieve_topic.rb"],
      requiredIssuePhrases: [
        "The current origin validation using indexOf is insufficient and can be bypassed. An attacker could use a malicious domain like evil-discourseUrl.com to pass this check.",
      ],
    },
  );

  assert.equal(report.metrics.matchedRequiredIssues, 1);
  assert.equal(report.metrics.missingRequiredIssues, 0);
});

test("passes clean reviews when every changed file has a clean verdict", () => {
  const report = evaluateReviewQuality(
    completedResult([]),
    [fileReviewEvent("src/readme.md", "clean")],
    {
      changedFiles: ["src/readme.md"],
    },
  );

  assert.equal(report.passed, true);
  assert.deepEqual(report.failures, []);
});

test("matches SSRF acronym to server-side request forgery and allows concrete may-also wording", () => {
  const report = evaluateReviewQuality(
    completedResult([
      finding(
        "Scheduled feed polling performs unrestricted server-side fetch",
        "The job calls open(url) for a configured feed without a host allowlist, causing server-side request forgery. It will fetch internal hosts and may also follow unsafe redirects.",
      ),
    ]),
    [
      fileReviewEvent("app/jobs/regular/retrieve_topic.rb", "issue_found", {
        findingId: "scheduled-feed-polling-performs-unrestricted-server-side-fetch",
      }),
    ],
    {
      changedFiles: ["app/jobs/regular/retrieve_topic.rb"],
      requiredIssuePhrases: ["SSRF vulnerability using open(url) without validation"],
    },
  );

  assert.equal(report.passed, true);
  assert.equal(report.metrics.matchedRequiredIssues, 1);
  assert.equal(report.metrics.speculativeFindings, 0);
});

test("fails findings without matching issue_found file verdicts", () => {
  const report = evaluateReviewQuality(
    completedResult([
      finding(
        "Dropped async cleanup promises",
        "The changed cleanup path starts deletion promises but never awaits them.",
      ),
    ]),
    [fileReviewEvent("app/jobs/regular/retrieve_topic.rb", "clean")],
    {
      changedFiles: ["app/jobs/regular/retrieve_topic.rb"],
    },
  );

  assert.equal(report.passed, false);
  assert.equal(report.metrics.verdictMismatches, 2);
  assert(
    report.failures.some((failure) =>
      failure.includes("lacks matching issue_found file-review verdict"),
    ),
  );
  assert(
    report.failures.some((failure) =>
      failure.includes("clean file-review verdict conflicts with finding"),
    ),
  );
});

test("fails issue_found verdicts with missing or unknown finding ids", () => {
  const missing = evaluateReviewQuality(
    completedResult([]),
    [fileReviewEvent("src/a.ts", "issue_found")],
    {
      changedFiles: ["src/a.ts"],
    },
  );
  const unknown = evaluateReviewQuality(
    completedResult([]),
    [fileReviewEvent("src/a.ts", "issue_found", { findingId: "finding-missing" })],
    {
      changedFiles: ["src/a.ts"],
    },
  );

  assert.equal(missing.passed, false);
  assert.equal(missing.metrics.verdictMismatches, 1);
  assert(
    missing.failures.some((failure) =>
      failure.includes("issue_found file-review verdict missing finding_id"),
    ),
  );
  assert.equal(unknown.passed, false);
  assert.equal(unknown.metrics.verdictMismatches, 1);
  assert(
    unknown.failures.some((failure) =>
      failure.includes("references unknown finding finding-missing"),
    ),
  );
});

test("fails issue_found verdicts that reference a finding on another path", () => {
  const report = evaluateReviewQuality(
    completedResult([
      finding(
        "Remote topic import performs unrestricted server-side fetches",
        "The model calls open(url) without restricting the destination host.",
      ),
    ]),
    [
      fileReviewEvent("app/views/layouts/embed.html.erb", "issue_found", {
        findingId: "remote-topic-import-performs-unrestricted-server-side-fetches",
      }),
    ],
    {
      changedFiles: ["app/views/layouts/embed.html.erb"],
    },
  );

  assert.equal(report.passed, false);
  assert(
    report.failures.some((failure) =>
      failure.includes("not app/views/layouts/embed.html.erb"),
    ),
  );
});

test("fails missing golden issue, speculative finding, failed session, failed tool, and missing verdict", () => {
  const report = evaluateReviewQuality(
    completedResult([
      finding(
        "Potential origin handling issue",
        "This might be risky if callers pass unusual URLs.",
      ),
    ]),
    [
      sessionFinishedEvent("security-batch-01", "failed"),
      toolFailedEvent("security-batch-01", "read_head_file", "not_text"),
    ],
    {
      changedFiles: ["app/controllers/embed_controller.rb"],
      requiredIssuePhrases: ["postMessage targetOrigin should be the origin"],
    },
  );

  assert.equal(report.passed, false);
  assert.deepEqual(report.metrics, {
    changedFiles: 1,
    duplicateFileVerdicts: 0,
    failedSessions: 1,
    failedTools: 1,
    fileReviews: 0,
    findings: 1,
    matchedRequiredIssues: 0,
    missingFileVerdicts: 1,
    missingRequiredIssues: 1,
    modelFailures: 0,
    speculativeFindings: 1,
    verdictMismatches: 1,
  });
  assert(report.failures.some((failure) => failure.includes("missing required issue phrase")));
  assert(report.failures.some((failure) => failure.includes("speculative finding")));
  assert(report.failures.some((failure) => failure.includes("ended with status failed")));
  assert(report.failures.some((failure) => failure.includes("read_head_file failed")));
  assert(report.failures.some((failure) => failure.includes("missing file-review verdict")));
});

test("fails duplicate file-review verdicts", () => {
  const report = evaluateReviewQuality(
    completedResult([]),
    [fileReviewEvent("src/a.ts", "clean"), fileReviewEvent("src/a.ts", "clean")],
    {
      changedFiles: ["src/a.ts"],
    },
  );

  assert.equal(report.passed, false);
  assert.equal(report.metrics.duplicateFileVerdicts, 1);
  assert(
    report.failures.some((failure) =>
      failure.includes("duplicate file-review verdict: src/a.ts"),
    ),
  );
});

test("does not fail justified unreadable read tools when the file was skipped", () => {
  const report = evaluateReviewQuality(
    completedResult([]),
    [
      fileReviewEvent("assets/logo.bin", "skipped"),
      toolFailedEvent("security-batch-01", "read_head_file", "not_text", {
        path: "assets/logo.bin",
      }),
    ],
    {
      changedFiles: ["assets/logo.bin"],
    },
  );

  assert.equal(report.passed, true);
  assert.equal(report.metrics.failedTools, 0);
});

test("infers changed files and model failures from runner events", () => {
  const report = evaluateReviewQuality(
    undefined,
    [
      listChangedFilesEvent(["Modified src/a.ts", "Added src/b.ts"]),
      fileReviewEvent("src/a.ts", "clean"),
      modelFailedEvent("security-batch-01", 1, false, "provider error: Some(429)"),
    ],
  );

  assert.equal(report.passed, false);
  assert.equal(report.metrics.changedFiles, 2);
  assert.equal(report.metrics.modelFailures, 1);
  assert(report.failures.some((failure) => failure.includes("review did not produce a result")));
  assert(report.failures.some((failure) => failure.includes("model failed attempt 1")));
  assert(report.failures.some((failure) => failure.includes("missing file-review verdict: src/b.ts")));
});

test("ignores retrying model failures until a final failure appears", () => {
  const report = evaluateReviewQuality(
    completedResult([]),
    [modelFailedEvent("security", 1, true, "provider retry")],
  );

  assert.equal(report.metrics.modelFailures, 0);
  assert.equal(report.passed, true);
});

test("fails incomplete review results", () => {
  const report = evaluateReviewQuality(
    { ...completedResult([]), status: "failed" },
    [],
  );

  assert.equal(report.passed, false);
  assert.deepEqual(report.failures, ["review status is failed"]);
});

test("builds quality expectations from golden review records", () => {
  const record = {
    url: "https://github.com/org/repo/pull/4",
    comments: [
      { comment: " SSRF vulnerability using open(url) without validation " },
      { comment: "" },
      { comment: "postMessage targetOrigin should be the origin" },
    ],
  };

  assert.deepEqual(expectationFromGoldenRecord(record), {
    requiredIssuePhrases: [
      "SSRF vulnerability using open(url) without validation",
      "postMessage targetOrigin should be the origin",
    ],
  });
});

test("finds golden records and builds quality endpoint URLs", () => {
  const records = [
    { url: "https://github.com/org/repo/pull/3", comments: [] },
    {
      url: "https://github.com/org/repo/pull/4/",
      comments: [{ comment: "SSRF vulnerability using open(url)" }],
    },
  ];
  const record = findGoldenRecord(records, "https://github.com/org/repo/pull/4");

  assert.equal(record, records[1]);
  assert.equal(
    qualityUrlForExpectation("http://localhost:4106", "review-1", {
      changedFiles: ["app/embed.rb"],
      requiredIssuePhrases: ["SSRF vulnerability using open(url)"],
    }),
    "http://localhost:4106/api/reviews/review-1/quality?requiredIssuePhrase=SSRF+vulnerability+using+open%28url%29&changedFile=app%2Fembed.rb",
  );
});

function completedResult(findings: ReviewFinding[]): ReviewResult {
  return {
    reviewId: "review-1",
    sessionId: "review-1",
    status: "completed",
    conclusion: findings.length > 0 ? "commented" : "approved",
    summary: "Review completed.",
    findings,
    coverage: {
      filesConsidered: 1,
      filesReviewed: 1,
      filesSkipped: 0,
    },
  };
}

function finding(title: string, message: string): ReviewFinding {
  return {
    id: title.toLowerCase().replace(/\W+/g, "-"),
    title,
    message,
    category: "security",
    severity: "error",
    confidence: 0.9,
    validationStatus: "validated",
    location: {
      path: "app/jobs/regular/retrieve_topic.rb",
      startLine: 10,
      endLine: 10,
    },
    evidence: [],
    discoveredBy: ["security"],
    validatedBy: [],
  };
}

function fileReviewEvent(
  path: string,
  verdict: string,
  options: { findingId?: string } = {},
): ReviewEvent {
  return event({
    artifactCreated: {
      toolId: "record_file_review",
      details: {
        findingId: options.findingId,
        path,
        verdict,
      },
    },
  });
}

function sessionFinishedEvent(sessionId: string, status: string): ReviewEvent {
  return event({
    sessionFinished: {
      sessionId,
      status,
    },
  });
}

function listChangedFilesEvent(changedFiles: string[]): ReviewEvent {
  return event({
    artifactCreated: {
      toolId: "list_changed_files",
      details: { changedFiles },
    },
  });
}

function modelFailedEvent(
  sessionId: string,
  attempt: number,
  retrying: boolean,
  message: string,
): ReviewEvent {
  return event({
    context: { sessionId },
    modelFailed: {
      attempt,
      message,
      retrying,
      sessionId,
      turn: 0,
    },
  });
}

function toolFailedEvent(
  sessionId: string,
  toolId: string,
  errorCode: string,
  details?: Record<string, unknown>,
): ReviewEvent {
  return event({
    context: { sessionId },
    toolCallCompleted: {
      toolId,
      ok: false,
      errorCode,
      details,
    },
  });
}

function event(payload: unknown): ReviewEvent {
  return {
    cursor: Math.random().toString(),
    reviewId: "review-1",
    timestampUtc: "2026-06-06T00:00:00.000Z",
    type: "runner.event",
    payload,
  };
}
