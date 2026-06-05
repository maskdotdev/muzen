export type ReviewStatus =
  | "created"
  | "queued"
  | "running"
  | "completed"
  | "failed"
  | "cancelled";

export type ReviewConclusion = "approved" | "commented" | "changes_requested";

export type ReviewRole =
  | "generalist"
  | "security"
  | "performance"
  | "maintainability"
  | "correctness"
  | "architecture"
  | "validator";

export type ReviewSource =
  | LocalReviewSource
  | GithubPullRequestSource
  | GitlabMergeRequestSource;

export type ReviewSourceLike = ReviewSource | string;

export interface LocalReviewSource {
  type: "local";
  repo: string;
  changedFiles?: string[];
}

export interface GithubPullRequestSource {
  type: "github_pull_request";
  owner: string;
  repo: string;
  number: number;
}

export interface GitlabMergeRequestSource {
  type: "gitlab_merge_request";
  owner: string;
  repo: string;
  number: number;
}

export type DedupePolicy =
  | "none"
  | "source"
  | "source_head"
  | { key: string };

export interface ReviewOptions {
  dedupe?: DedupePolicy;
  cancelSuperseded?: boolean;
  model?: string;
  scope?: ReviewScope;
  metadata?: Record<string, unknown>;
  sessions?: ReviewAgentSession[];
  limits?: ReviewLimits;
}

export interface ReviewScope {
  files?: string[];
  include?: string[];
  exclude?: string[];
}

export interface ReviewAgentSession {
  id: string;
  role: ReviewRole;
  objective: string;
  cwd?: string;
  modelProfileId?: string;
  budget?: ReviewAgentBudget;
}

export interface ReviewAgentBudget {
  maxTurns: number;
  maxToolCalls: number;
  maxPromptTokens: number;
  maxOutputTokens: number;
}

export interface ReviewLimits {
  maxActiveSessions?: number;
  maxFileBytes?: number;
  maxSearchMatches?: number;
}

export interface ReviewSessionSnapshot {
  id: string;
  status: ReviewStatus;
  source: ReviewSource;
  result?: ReviewResult;
}

export interface ReviewResult {
  reviewId: string;
  sessionId: string;
  status: ReviewStatus;
  conclusion: ReviewConclusion;
  summary: string;
  findings: ReviewFinding[];
  coverage: ReviewCoverage;
  metadata?: Record<string, unknown>;
}

export interface ReviewFinding {
  id: string;
  severity: "info" | "warning" | "error";
  category:
    | "bug"
    | "security"
    | "performance"
    | "maintainability"
    | "style"
    | "test"
    | "docs"
    | "other";
  title: string;
  message: string;
  location?: ReviewFindingLocation;
  suggestedFix?: ReviewSuggestedFix;
  confidence?: number;
}

export interface ReviewFindingLocation {
  path: string;
  startLine?: number;
  endLine?: number;
  startColumn?: number;
  endColumn?: number;
}

export interface ReviewSuggestedFix {
  description?: string;
  patch?: string;
}

export interface ReviewCoverage {
  filesConsidered: number;
  filesReviewed: number;
  filesSkipped: number;
}

export type ReviewEventType =
  | "session.created"
  | "session.queued"
  | "session.started"
  | "source.resolved"
  | "scope.inferred"
  | "scope.overridden"
  | "repo.materialized"
  | "plan.created"
  | "agent.started"
  | "agent.completed"
  | "tool.started"
  | "tool.completed"
  | "finding.created"
  | "finding.updated"
  | "review.result_created"
  | "session.completed"
  | "session.failed"
  | "session.cancelled"
  | "runner.event";

export interface ReviewEvent {
  cursor: string;
  type: ReviewEventType;
  reviewId: string;
  timestampUtc: string;
  payload?: unknown;
}

export interface ReviewCancelOptions {
  reason?: string;
}

export type ReviewArtifactView = "redacted" | "raw";

export interface ReviewArtifactReadOptions {
  view?: ReviewArtifactView;
}

export interface ReviewArtifactExportOptions {
  view?: ReviewArtifactView;
  artifactIds?: string[];
  maxArtifacts?: number;
  maxBytes?: number;
}

export interface ReviewArtifactExport {
  view: ReviewArtifactView;
  artifactCount: number;
  totalBytes: number;
  artifacts: ReviewArtifact[];
}

export interface ReviewArtifact {
  artifactId: string;
  bytes: number;
  contentHash: string;
  content: string;
}

export interface CreateMuzenOptions {
  runnerPath?: string;
  runnerArgs?: string[];
  clientName?: string;
  clientVersion?: string;
}

export interface CreateMuzenClientOptions {
  baseUrl: string;
  token?: string;
  fetch?: typeof fetch;
}

export interface CreateReviewSessionInput {
  source: ReviewSourceLike;
  options?: ReviewOptions;
  muzen?: CreateMuzenOptions;
}

export interface CreateReviewSessionResult {
  muzen: Muzen;
  review: ReviewSession;
}

export interface Muzen {
  review(source: ReviewSourceLike, options?: ReviewOptions): Promise<ReviewSession>;
  resumeReview(id: string): Promise<ReviewSession>;
  createReviewSession(input: {
    source: ReviewSourceLike;
    options?: ReviewOptions;
  }): Promise<ReviewSession>;
  close(): Promise<void>;
}

export interface ReviewSession {
  readonly id: string;
  readonly status: ReviewStatus;
  readonly source: ReviewSource;
  subscribe(
    listener: (event: ReviewEvent) => void,
    options?: { replay?: boolean },
  ): () => void;
  events(options?: {
    after?: string | null;
    signal?: AbortSignal;
  }): AsyncIterable<ReviewEvent>;
  eventsResponse(options?: {
    after?: string | null;
    signal?: AbortSignal;
  }): Response;
  wait(options?: {
    timeout?: string | number;
    signal?: AbortSignal;
  }): Promise<ReviewResult>;
  result(): Promise<ReviewResult | undefined>;
  readArtifact(
    artifactId: string,
    options?: ReviewArtifactReadOptions,
  ): Promise<ReviewArtifact>;
  exportArtifacts(
    options?: ReviewArtifactExportOptions,
  ): Promise<ReviewArtifactExport>;
  cancel(reason?: string | ReviewCancelOptions): Promise<void>;
  refresh(): Promise<ReviewSessionSnapshot>;
}
