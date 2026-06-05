import type {
  ModelProfile,
  MuzenWorkerRun,
  ProviderProfile,
  ReviewArtifact,
  ReviewArtifactExport,
  ReviewEvent,
  ReviewResult,
  ReviewSessionSnapshot,
  ReviewSource,
  ReviewStatus,
} from "./types.js";

export function unwrapReviewSnapshot(value: unknown): ReviewSessionSnapshot {
  const snapshot = isRecord(value) && isRecord(value.review) ? value.review : value;
  if (!isReviewSessionSnapshot(snapshot)) {
    throw new Error("Muzen remote returned an invalid review session snapshot");
  }
  return snapshot;
}

export function unwrapWorkerRun(value: unknown): MuzenWorkerRun {
  if (!isRecord(value)) {
    throw new Error("invalid worker run response from muzen-runner");
  }
  const run = value as Record<string, unknown>;
  if (
    typeof run.workerId !== "string" ||
    typeof run.claimed !== "number" ||
    typeof run.completed !== "number" ||
    typeof run.retried !== "number" ||
    typeof run.failed !== "number" ||
    typeof run.skipped !== "number"
  ) {
    throw new Error("invalid worker run response from muzen-runner");
  }
  return {
    workerId: run.workerId,
    claimed: run.claimed,
    completed: run.completed,
    retried: run.retried,
    failed: run.failed,
    skipped: run.skipped,
  };
}

export function unwrapOptionalReviewResult(value: unknown): ReviewResult | undefined {
  if (value === undefined || value === null) {
    return undefined;
  }
  const result = isRecord(value) && "result" in value ? value.result : value;
  if (result === undefined || result === null) {
    return undefined;
  }
  if (!isReviewResult(result)) {
    throw new Error("Muzen remote returned an invalid review result");
  }
  return result;
}

export function unwrapReviewEvents(value: unknown): ReviewEvent[] {
  const events = isRecord(value) && Array.isArray(value.events) ? value.events : value;
  if (!Array.isArray(events) || !events.every(isReviewEvent)) {
    throw new Error("Muzen remote returned invalid review events");
  }
  return events;
}

export function unwrapReviewArtifact(value: unknown): ReviewArtifact {
  const artifact = isRecord(value) && isRecord(value.artifact) ? value.artifact : value;
  if (!isReviewArtifact(artifact)) {
    throw new Error("Muzen remote returned an invalid review artifact");
  }
  return artifact;
}

export function unwrapReviewArtifactExport(value: unknown): ReviewArtifactExport {
  if (!isReviewArtifactExport(value)) {
    throw new Error("Muzen remote returned an invalid artifact export");
  }
  return value;
}

export function unwrapModelProfile(value: unknown): ModelProfile {
  const profile = isRecord(value) && isRecord(value.profile) ? value.profile : value;
  if (!isModelProfile(profile)) {
    throw new Error("Muzen remote returned an invalid model profile");
  }
  return profile;
}

export function unwrapModelProfiles(value: unknown): ModelProfile[] {
  const profiles = isRecord(value) && Array.isArray(value.profiles) ? value.profiles : value;
  if (!Array.isArray(profiles) || !profiles.every(isModelProfile)) {
    throw new Error("Muzen remote returned invalid model profiles");
  }
  return profiles;
}

export function unwrapProviderProfile(value: unknown): ProviderProfile {
  const profile = isRecord(value) && isRecord(value.profile) ? value.profile : value;
  if (!isProviderProfile(profile)) {
    throw new Error("Muzen remote returned an invalid provider profile");
  }
  return profile;
}

export function unwrapProviderProfiles(value: unknown): ProviderProfile[] {
  const profiles = isRecord(value) && Array.isArray(value.profiles) ? value.profiles : value;
  if (!Array.isArray(profiles) || !profiles.every(isProviderProfile)) {
    throw new Error("Muzen remote returned invalid provider profiles");
  }
  return profiles;
}

export function unwrapWebhookHttpResponse(value: unknown): {
  statusCode: number;
  headers: Record<string, string>;
  body: string;
} {
  if (
    isRecord(value) &&
    typeof value.statusCode === "number" &&
    isRecord(value.headers) &&
    typeof value.body === "string"
  ) {
    return {
      statusCode: value.statusCode,
      headers: stringRecord(value.headers),
      body: value.body,
    };
  }
  throw new Error("muzen-runner returned an invalid webhook response");
}

function stringRecord(value: Record<string, unknown>): Record<string, string> {
  const result: Record<string, string> = {};
  for (const [key, item] of Object.entries(value)) {
    if (typeof item === "string") {
      result[key] = item;
    }
  }
  return result;
}

function isReviewSessionSnapshot(value: unknown): value is ReviewSessionSnapshot {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    isReviewStatus(value.status) &&
    isReviewSource(value.source) &&
    (value.result === undefined || isReviewResult(value.result))
  );
}

function isReviewSource(value: unknown): value is ReviewSource {
  return (
    isRecord(value) &&
    (value.type === "local" ||
      value.type === "github_pull_request" ||
      value.type === "gitlab_merge_request")
  );
}

function isReviewResult(value: unknown): value is ReviewResult {
  return (
    isRecord(value) &&
    typeof value.reviewId === "string" &&
    typeof value.sessionId === "string" &&
    isReviewStatus(value.status) &&
    (value.conclusion === "approved" ||
      value.conclusion === "commented" ||
      value.conclusion === "changes_requested") &&
    typeof value.summary === "string" &&
    Array.isArray(value.findings) &&
    isRecord(value.coverage)
  );
}

function isReviewEvent(value: unknown): value is ReviewEvent {
  return (
    isRecord(value) &&
    typeof value.cursor === "string" &&
    typeof value.type === "string" &&
    typeof value.reviewId === "string" &&
    typeof value.timestampUtc === "string"
  );
}

function isReviewArtifact(value: unknown): value is ReviewArtifact {
  return (
    isRecord(value) &&
    typeof value.artifactId === "string" &&
    typeof value.bytes === "number" &&
    typeof value.contentHash === "string" &&
    typeof value.content === "string"
  );
}

function isReviewArtifactExport(value: unknown): value is ReviewArtifactExport {
  return (
    isRecord(value) &&
    (value.view === "redacted" || value.view === "raw") &&
    typeof value.artifactCount === "number" &&
    typeof value.totalBytes === "number" &&
    Array.isArray(value.artifacts) &&
    value.artifacts.every(isReviewArtifact)
  );
}

function isReviewStatus(value: unknown): value is ReviewStatus {
  return (
    value === "created" ||
    value === "queued" ||
    value === "running" ||
    value === "completed" ||
    value === "failed" ||
    value === "cancelled"
  );
}

function isModelProfile(value: unknown): value is ModelProfile {
  return (
    isRecord(value) &&
    typeof value.workspaceId === "string" &&
    typeof value.name === "string" &&
    typeof value.version === "string" &&
    isModelProviderKind(value.provider) &&
    typeof value.model === "string" &&
    typeof value.updatedAtUtc === "string"
  );
}

function isProviderProfile(value: unknown): value is ProviderProfile {
  return (
    isRecord(value) &&
    typeof value.workspaceId === "string" &&
    typeof value.name === "string" &&
    typeof value.version === "string" &&
    isSourceProviderKind(value.provider) &&
    typeof value.updatedAtUtc === "string"
  );
}

function isModelProviderKind(value: unknown): value is ModelProfile["provider"] {
  return (
    value === "openai" ||
    value === "anthropic" ||
    value === "openai_compatible"
  );
}

function isSourceProviderKind(value: unknown): value is ProviderProfile["provider"] {
  return value === "github" || value === "gitlab";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
