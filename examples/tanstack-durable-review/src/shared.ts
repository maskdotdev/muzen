import type {
  ReviewEvent,
  ReviewResult,
  ReviewRole,
  ReviewSource,
  ReviewStatus,
} from "@muzen/sdk";

export interface CreateReviewRequest {
  sourceKind: ReviewTargetKind;
  repo?: string;
  githubPullRequest?: string;
  changedFiles: string[];
  roles: ReviewRole[];
}

export type ReviewTargetKind = "local" | "github";

export interface ReviewSnapshot {
  id: string;
  status: ReviewStatus;
  source: ReviewSource;
  result?: ReviewResult;
  error?: string;
}

export interface ReviewEventsResponse {
  events: ReviewEvent[];
}
