import type {
  ReviewEvent,
  ReviewResult,
  ReviewRole,
  ReviewSource,
  ReviewStatus,
} from "@muzen/sdk";

export interface CreateReviewRequest {
  repo: string;
  changedFiles: string[];
  roles: ReviewRole[];
}

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
