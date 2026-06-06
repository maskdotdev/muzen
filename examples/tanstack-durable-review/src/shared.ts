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

export function reviewSourceKey(source: ReviewSource): string {
  switch (source.type) {
    case "local":
      return `local:${source.repo}`;
    case "raw_snapshot":
      return `raw:${source.root}`;
    case "github_pull_request":
      return `github:${source.owner}/${source.repo}#${source.number}`;
    case "gitlab_merge_request":
      return `gitlab:${source.owner}/${source.repo}!${source.number}`;
    case "perforce_changelist":
      return `perforce:${source.server}@${source.changelist}`;
    case "custom":
      return `custom:${source.provider}:${source.id}`;
  }
}
