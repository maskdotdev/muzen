import assert from "node:assert/strict";
import test from "node:test";

import type { ReviewEvent, ReviewOptions, ReviewResult } from "@muzen/sdk";

import { DurableReviewStore } from "./store.js";

test("surfaces model provider failures as review errors", () => {
  const store = new DurableReviewStore();
  const review = store.create(
    { type: "local", repo: ".", changedFiles: ["src/app.ts"] },
    reviewOptions(),
  );

  store.appendRunnerEvent(review.id, modelFailedEvent(review.id));
  store.complete(review.id, failedResult(review.id));

  const snapshot = store.snapshot(review.id);
  assert.equal(snapshot.status, "failed");
  assert.equal(
    snapshot.error,
    "correctness model call failed attempt 1: provider error: Some(401)",
  );
});

test("prefers final non-retryable model failures over retry noise", () => {
  const store = new DurableReviewStore();
  const review = store.create(
    { type: "local", repo: ".", changedFiles: ["src/app.ts"] },
    reviewOptions(),
  );

  store.appendRunnerEvent(
    review.id,
    modelFailedEvent(review.id, {
      attempt: 1,
      retrying: true,
      message: "provider error: Some(429): rate limit",
    }),
  );
  store.appendRunnerEvent(
    review.id,
    modelFailedEvent(review.id, {
      attempt: 1,
      retrying: false,
      message: "provider error: Some(429): insufficient_quota",
    }),
  );
  store.complete(review.id, failedResult(review.id));

  const snapshot = store.snapshot(review.id);
  assert.equal(
    snapshot.error,
    "correctness model call failed attempt 1: provider error: Some(429): insufficient_quota",
  );
});

function reviewOptions(): ReviewOptions {
  return {
    change: {
      kind: "revision_range",
      changedFiles: [{ path: "src/app.ts" }],
      reviewTarget: "local:.",
    },
    scope: { files: ["src/app.ts"] },
    sessions: [
      {
        id: "correctness",
        role: "correctness",
        objective: "Review the change.",
      },
    ],
  };
}

function modelFailedEvent(
  reviewId: string,
  failure: {
    attempt: number;
    message: string;
    retrying: boolean;
  } = {
    attempt: 1,
    message: "provider error: Some(401)",
    retrying: false,
  },
): ReviewEvent {
  return {
    cursor: "runner-1",
    reviewId,
    timestampUtc: "2026-06-06T00:00:00.000Z",
    type: "runner.event",
    payload: {
      modelFailed: {
        sessionId: "correctness",
        turn: 0,
        attempt: failure.attempt,
        retrying: failure.retrying,
        message: failure.message,
      },
      context: {
        sessionId: "correctness",
        turn: 0,
      },
    },
  };
}

function failedResult(reviewId: string): ReviewResult {
  return {
    reviewId,
    sessionId: reviewId,
    status: "failed",
    conclusion: "changes_requested",
    summary: "review failed",
    findings: [],
    coverage: {
      filesConsidered: 0,
      filesReviewed: 0,
      filesSkipped: 0,
    },
  };
}
