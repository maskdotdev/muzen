import type { ReviewEvent, ReviewEventType } from "./types.js";

export type ReviewProgressPhase =
  | "queued"
  | "resolving_source"
  | "materializing"
  | "planning"
  | "analyzing"
  | "mapping"
  | "saving"
  | "done"
  | "failed"
  | "cancelled";

export interface ReviewProgressProjectionOptions<Stage extends string = string> {
  stageMap?: Partial<Record<ReviewProgressPhase, Stage>>;
}

export interface ReviewProgressProjection<Stage extends string = string> {
  reviewId: string;
  cursor: string;
  timestampUtc: string;
  phase: ReviewProgressPhase;
  stage?: Stage;
  percent: number;
  terminal: boolean;
  message: string;
  eventType: ReviewEventType;
}

export function projectReviewProgress<Stage extends string = string>(
  input: ReviewEvent | readonly ReviewEvent[],
  options: ReviewProgressProjectionOptions<Stage> = {},
): ReviewProgressProjection<Stage> {
  const event = isEventList(input) ? lastEvent(input) : input;
  const phase = phaseForEvent(event.type);
  return {
    reviewId: event.reviewId,
    cursor: event.cursor,
    timestampUtc: event.timestampUtc,
    phase,
    stage: options.stageMap?.[phase],
    percent: percentForPhase(phase),
    terminal: terminalPhase(phase),
    message: messageForPhase(phase),
    eventType: event.type,
  };
}

function isEventList(
  input: ReviewEvent | readonly ReviewEvent[],
): input is readonly ReviewEvent[] {
  return Array.isArray(input);
}

export function projectReviewProgressTimeline<Stage extends string = string>(
  events: readonly ReviewEvent[],
  options: ReviewProgressProjectionOptions<Stage> = {},
): ReviewProgressProjection<Stage>[] {
  return events.map((event) => projectReviewProgress(event, options));
}

function lastEvent(events: readonly ReviewEvent[]): ReviewEvent {
  if (events.length === 0) {
    throw new Error("cannot project progress from an empty event list");
  }
  return events[events.length - 1];
}

function phaseForEvent(type: ReviewEventType): ReviewProgressPhase {
  switch (type) {
    case "session.created":
    case "session.queued":
      return "queued";
    case "session.started":
    case "source.resolved":
      return "resolving_source";
    case "scope.inferred":
    case "scope.overridden":
    case "repo.materialized":
      return "materializing";
    case "plan.created":
      return "planning";
    case "agent.started":
    case "agent.completed":
    case "tool.started":
    case "tool.completed":
    case "finding.created":
    case "finding.updated":
    case "runner.event":
      return "analyzing";
    case "review.result_created":
      return "saving";
    case "session.completed":
      return "done";
    case "session.failed":
      return "failed";
    case "session.cancelled":
      return "cancelled";
  }
}

function percentForPhase(phase: ReviewProgressPhase): number {
  switch (phase) {
    case "queued":
      return 5;
    case "resolving_source":
      return 15;
    case "materializing":
      return 30;
    case "planning":
      return 40;
    case "analyzing":
      return 65;
    case "mapping":
      return 80;
    case "saving":
      return 90;
    case "done":
    case "failed":
    case "cancelled":
      return 100;
  }
}

function terminalPhase(phase: ReviewProgressPhase): boolean {
  return phase === "done" || phase === "failed" || phase === "cancelled";
}

function messageForPhase(phase: ReviewProgressPhase): string {
  switch (phase) {
    case "queued":
      return "Review is queued.";
    case "resolving_source":
      return "Resolving review source.";
    case "materializing":
      return "Materializing repository snapshot.";
    case "planning":
      return "Planning review sessions.";
    case "analyzing":
      return "Analyzing the change.";
    case "mapping":
      return "Mapping findings.";
    case "saving":
      return "Saving review results.";
    case "done":
      return "Review completed.";
    case "failed":
      return "Review failed.";
    case "cancelled":
      return "Review was cancelled.";
  }
}
