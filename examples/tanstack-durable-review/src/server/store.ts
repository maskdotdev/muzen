import { randomUUID } from "node:crypto";

import type {
  ReviewEvent,
  ReviewOptions,
  ReviewResult,
  ReviewSource,
  ReviewStatus,
} from "@muzen/sdk";

import type { ReviewSnapshot } from "../shared.js";

type EventListener = (event: ReviewEvent) => void;

export interface StoredReview {
  id: string;
  status: ReviewStatus;
  source: ReviewSource;
  options: ReviewOptions;
  events: ReviewEvent[];
  nextCursor: number;
  result?: ReviewResult;
  error?: string;
  createdAt: string;
  updatedAt: string;
}

export class DurableReviewStore {
  private readonly reviews = new Map<string, StoredReview>();
  private readonly listeners = new Map<string, Set<EventListener>>();

  create(source: ReviewSource, options: ReviewOptions): ReviewSnapshot {
    const id = `review-${randomUUID()}`;
    const now = timestamp();
    const review: StoredReview = {
      id,
      status: "queued",
      source,
      options,
      events: [],
      nextCursor: 1,
      createdAt: now,
      updatedAt: now,
    };
    this.reviews.set(id, review);
    this.appendSystemEvent(id, "session.queued", { status: "queued" });
    return this.snapshot(id);
  }

  get(id: string): StoredReview {
    const review = this.reviews.get(id);
    if (!review) {
      throw new Error(`unknown review ${id}`);
    }
    return review;
  }

  has(id: string): boolean {
    return this.reviews.has(id);
  }

  snapshot(id: string): ReviewSnapshot {
    const review = this.get(id);
    return {
      id: review.id,
      status: review.status,
      source: review.source,
      changedFiles: requestedChangedFiles(review),
      maxActiveSessions: review.options.limits?.maxActiveSessions,
      result: review.result,
      error: review.error ?? inferredReviewError(review),
    };
  }

  markRunning(id: string): void {
    const review = this.get(id);
    review.status = "running";
    review.updatedAt = timestamp();
    this.appendSystemEvent(id, "session.started", { status: "running" });
  }

  complete(id: string, result: ReviewResult): void {
    const review = this.get(id);
    const projectedResult: ReviewResult = {
      ...result,
      reviewId: id,
      sessionId: id,
      metadata: {
        ...result.metadata,
        runnerRunId: result.metadata?.runnerRunId ?? result.reviewId,
      },
    };
    const error = inferredResultError(review, projectedResult);
    review.status = projectedResult.status;
    review.result = projectedResult;
    review.error = error;
    review.updatedAt = timestamp();
    this.appendSystemEvent(id, terminalEventType(projectedResult.status), {
      status: projectedResult.status,
      ...(error ? { error } : {}),
    });
    this.appendSystemEvent(id, "review.result_created", {
      conclusion: projectedResult.conclusion,
      findings: projectedResult.findings.length,
    });
  }

  fail(id: string, error: string): void {
    const review = this.get(id);
    review.status = "failed";
    review.error = error;
    review.updatedAt = timestamp();
    this.appendSystemEvent(id, "session.failed", { error });
  }

  appendRunnerEvent(id: string, event: ReviewEvent): void {
    this.appendEvent(id, {
      type: event.type,
      payload: event.payload,
    });
  }

  appendSystemEvent(
    id: string,
    type: ReviewEvent["type"],
    payload?: unknown,
  ): void {
    this.appendEvent(id, { type, payload });
  }

  eventsAfter(id: string, after?: string | null): ReviewEvent[] {
    const cursor = after ? Number(after) : 0;
    return this.get(id).events.filter((event) => Number(event.cursor) > cursor);
  }

  subscribe(id: string, listener: EventListener): () => void {
    if (!this.listeners.has(id)) {
      this.listeners.set(id, new Set());
    }
    this.listeners.get(id)!.add(listener);
    return () => {
      this.listeners.get(id)?.delete(listener);
    };
  }

  private appendEvent(
    id: string,
    input: Pick<ReviewEvent, "type" | "payload">,
  ): void {
    const review = this.get(id);
    const event: ReviewEvent = {
      cursor: String(review.nextCursor++),
      type: input.type,
      reviewId: id,
      timestampUtc: timestamp(),
      payload: input.payload,
    };
    review.events.push(event);
    for (const listener of this.listeners.get(id) ?? []) {
      listener(event);
    }
  }
}

function timestamp(): string {
  return new Date().toISOString();
}

function terminalEventType(status: ReviewStatus): ReviewEvent["type"] {
  if (status === "cancelled") {
    return "session.cancelled";
  }
  if (status === "failed") {
    return "session.failed";
  }
  return "session.completed";
}

function requestedChangedFiles(review: StoredReview): string[] {
  return review.options.scope?.files ?? [];
}

function inferredReviewError(review: StoredReview): string | undefined {
  if (!review.result) {
    return undefined;
  }
  return inferredResultError(review, review.result);
}

function inferredResultError(
  review: StoredReview,
  result: ReviewResult,
): string | undefined {
  if (result.status !== "failed" && result.status !== "cancelled") {
    return undefined;
  }
  const failures = modelFailureMessages(review.events);
  if (failures.length > 0) {
    return failures.join("; ");
  }
  return result.summary;
}

function modelFailureMessages(events: ReviewEvent[]): string[] {
  const failures: Array<{ final: boolean; text: string }> = [];
  for (const event of events) {
    const payload = asRecord(event.payload);
    const modelFailed = asRecord(payload?.modelFailed);
    if (!modelFailed) {
      continue;
    }
    const context = asRecord(payload?.context);
    const sessionId =
      stringValue(modelFailed.sessionId) ??
      stringValue(context?.sessionId) ??
      "unknown";
    const message = stringValue(modelFailed.message) ?? "model provider error";
    const attempt = numberValue(modelFailed.attempt);
    const attemptText = attempt > 0 ? ` attempt ${attempt}` : "";
    failures.push({
      final: modelFailed.retrying !== true,
      text: `${sessionId} model call failed${attemptText}: ${message}`,
    });
  }
  const finalFailures = failures.filter((failure) => failure.final);
  return [...new Set((finalFailures.length > 0 ? finalFailures : failures).map((failure) => failure.text))];
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : undefined;
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function numberValue(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
