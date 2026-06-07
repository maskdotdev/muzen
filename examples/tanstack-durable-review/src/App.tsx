import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";
import type { ReviewEvent, ReviewResult, ReviewRole } from "@muzen/sdk";

import type {
  CreateReviewRequest,
  ReviewModelPreflightResponse,
  ReviewQualityReport,
  ReviewSnapshot,
  ReviewTargetKind,
} from "./shared.js";
import { reviewSourceKey } from "./shared.js";

const defaultRoles: ReviewRole[] = ["generalist"];
const roleChoices: ReviewRole[] = [
  "generalist",
  "correctness",
  "security",
  "performance",
  "maintainability",
  "architecture",
];
const maxActiveSessionChoices = [1, 2, 4, 8] as const;

export function NewReviewPage() {
  const navigate = useNavigate();
  const [sourceKind, setSourceKind] = useState<ReviewTargetKind>("local");
  const [repo, setRepo] = useState("../..");
  const [githubPullRequest, setGithubPullRequest] = useState(
    "https://github.com/maskdotdev/muzen/pull/1",
  );
  const [changedFiles, setChangedFiles] = useState("Cargo.toml");
  const [maxActiveSessions, setMaxActiveSessions] = useState(4);
  const [roles, setRoles] = useState<ReviewRole[]>(defaultRoles);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const parsedChangedFiles = changedFiles
        .split(/\r?\n|,/)
        .map((value) => value.trim())
        .filter(Boolean);
      if (sourceKind === "local" && parsedChangedFiles.length === 0) {
        throw new Error("Changed files must include at least one repo-relative path.");
      }
      const body: CreateReviewRequest = {
        sourceKind,
        repo: sourceKind === "local" ? repo : undefined,
        githubPullRequest:
          sourceKind === "github" ? githubPullRequest : undefined,
        changedFiles: parsedChangedFiles,
        maxActiveSessions,
        roles,
      };
      await assertModelPreflight();
      const response = await fetch("/api/reviews", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      const payload = (await response.json()) as { review: ReviewSnapshot };
      await navigate({
        to: "/reviews/$reviewId",
        params: { reviewId: payload.review.id },
      });
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <section className="layout">
      <form className="panel" onSubmit={submit}>
        <h1>Start One Durable Review</h1>
        <fieldset className="segmented">
          <legend>Target</legend>
          <label>
            <input
              type="radio"
              checked={sourceKind === "local"}
              onChange={() => {
                setSourceKind("local");
                setChangedFiles("Cargo.toml");
              }}
            />
            Local repo
          </label>
          <label>
            <input
              type="radio"
              checked={sourceKind === "github"}
              onChange={() => {
                setSourceKind("github");
                setChangedFiles("");
              }}
            />
            GitHub PR
          </label>
        </fieldset>
        {sourceKind === "local" ? (
          <label>
            Repo path on the service machine
            <input value={repo} onChange={(event) => setRepo(event.target.value)} />
          </label>
        ) : (
          <label>
            GitHub PR URL or source key
            <input
              value={githubPullRequest}
              onChange={(event) => setGithubPullRequest(event.target.value)}
            />
          </label>
        )}
        <label>
          Changed files {sourceKind === "github" ? "(optional)" : ""}
          <textarea
            rows={5}
            value={changedFiles}
            onChange={(event) => setChangedFiles(event.target.value)}
          />
        </label>
        <fieldset>
          <legend>Run concurrency</legend>
          {maxActiveSessionChoices.map((choice) => (
            <label className="check" key={choice}>
              <input
                type="radio"
                checked={maxActiveSessions === choice}
                onChange={() => setMaxActiveSessions(choice)}
              />
              {choice} active {choice === 1 ? "session" : "sessions"}
            </label>
          ))}
        </fieldset>
        <fieldset>
          <legend>Sessions inside this one Muzen run</legend>
          {roleChoices.map((role) => (
            <label className="check" key={role}>
              <input
                type="checkbox"
                checked={roles.includes(role)}
                onChange={(event) => {
                  setRoles((current) =>
                    event.target.checked
                      ? [...current, role]
                      : current.filter((item) => item !== role),
                  );
                }}
              />
              {role}
            </label>
          ))}
        </fieldset>
        <button disabled={submitting || roles.length === 0}>
          {submitting ? "Creating..." : "Create durable review"}
        </button>
        {error ? <p className="error">{error}</p> : null}
      </form>

      <aside className="panel secondary">
        <h2>Flow</h2>
        <ol>
          <li>Create a durable review id.</li>
          <li>The host worker claims and runs it.</li>
          <li>Muzen emits ordered events.</li>
          <li>The browser reads events by SSE cursor.</li>
          <li>The final result is fetched by id.</li>
        </ol>
      </aside>
    </section>
  );
}

export function ReviewPage() {
  const { reviewId } = useParams({ from: "/reviews/$reviewId" });
  const { events, quality, result, snapshot, error } = useReviewStream(reviewId);
  const terminal = snapshot?.status === "completed" || snapshot?.status === "failed";
  const reviewError = snapshot?.error ?? error;

  return (
    <section className="review">
      <div className="summary">
        <div>
          <span className="label">Review</span>
          <h1>{reviewId}</h1>
          {snapshot ? (
            <p className="source">
              <span className="label">Target</span>
              {reviewSourceKey(snapshot.source)}
            </p>
          ) : null}
        </div>
        <span className={`status ${snapshot?.status ?? "queued"}`}>
          {snapshot?.status ?? "queued"}
        </span>
      </div>

      {reviewError ? <p className="error">{reviewError}</p> : null}

      <WorkTrace events={events} result={result} snapshot={snapshot} />

      <div className="grid">
        <section className="panel">
          <h2>Events</h2>
          <EventList events={events} />
        </section>

        <section className="panel">
          <h2>Result</h2>
          {quality ? <QualityView quality={quality} /> : null}
          {result ? <ResultView result={result} /> : <p>{terminal ? "No result." : "Waiting..."}</p>}
        </section>
      </div>
    </section>
  );
}

async function assertModelPreflight(): Promise<void> {
  const response = await fetch("/api/model/preflight");
  const payload = (await response.json()) as ReviewModelPreflightResponse;
  if (response.ok && payload.ok) {
    return;
  }
  const label = [payload.provider, payload.model].filter(Boolean).join(" ");
  const status = payload.status ? `status ${payload.status}` : `HTTP ${response.status}`;
  throw new Error(
    payload.error ??
      `Model preflight failed${label ? ` for ${label}` : ""} (${status}): provider unavailable`,
  );
}

function useReviewStream(reviewId: string) {
  const [events, setEvents] = useState<ReviewEvent[]>([]);
  const [quality, setQuality] = useState<ReviewQualityReport | null>(null);
  const [snapshot, setSnapshot] = useState<ReviewSnapshot | null>(null);
  const [result, setResult] = useState<ReviewResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const cursor = useRef<string | null>(null);

  useEffect(() => {
    let closed = false;
    let stream: EventSource | undefined;

    void connect();

    async function connect() {
      const exists = await refreshSnapshot();
      if (!exists || closed) {
        return;
      }
      const after = cursor.current
        ? `?after=${encodeURIComponent(cursor.current)}`
        : "";
      stream = new EventSource(`/api/reviews/${reviewId}/events/stream${after}`);
      stream.onmessage = (message) => {
        const event = JSON.parse(message.data) as ReviewEvent;
        cursor.current = event.cursor;
        setEvents((current) => [...current, event]);
        if (isTerminalEvent(event)) {
          closed = true;
          stream?.close();
          void refreshSnapshot();
          void refreshResult();
          void refreshQuality();
        }
      };
      stream.onerror = () => {
        if (!closed) {
          setError("SSE disconnected. Refresh to replay from the stored cursor.");
        }
      };
    }

    async function refreshSnapshot(): Promise<boolean> {
      const response = await fetch(`/api/reviews/${reviewId}`);
      if (response.ok) {
        setSnapshot(((await response.json()) as { review: ReviewSnapshot }).review);
        return true;
      }
      if (response.status === 404) {
        setError(
          "Review not found. This demo uses an in-memory store, so old review ids disappear when the service restarts.",
        );
        return false;
      }
      setError(await response.text());
      return false;
    }

    async function refreshResult() {
      const response = await fetch(`/api/reviews/${reviewId}/result`);
      if (response.status === 204) {
        return;
      }
      if (response.ok) {
        setResult(((await response.json()) as { result: ReviewResult }).result);
      }
    }

    async function refreshQuality() {
      const response = await fetch(`/api/reviews/${reviewId}/quality`);
      if (response.ok) {
        setQuality(
          ((await response.json()) as { quality: ReviewQualityReport }).quality,
        );
      }
    }

    return () => {
      closed = true;
      stream?.close();
    };
  }, [reviewId]);

  return useMemo(
    () => ({ events, quality, snapshot, result, error, cursor: cursor.current }),
    [events, quality, snapshot, result, error],
  );
}

function EventList({ events }: { events: ReviewEvent[] }) {
  if (events.length === 0) {
    return <p>Waiting for the worker to emit the first event.</p>;
  }
  return (
    <ol className="events">
      {events.map((event) => (
        <li key={event.cursor}>
          <span>{event.cursor}</span>
          <strong>{event.type}</strong>
          <code>{event.payload ? JSON.stringify(event.payload) : "{}"}</code>
        </li>
      ))}
    </ol>
  );
}

function WorkTrace({
  events,
  result,
  snapshot,
}: {
  events: ReviewEvent[];
  result: ReviewResult | null;
  snapshot: ReviewSnapshot | null;
}) {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNowMs(Date.now()), 5_000);
    return () => window.clearInterval(timer);
  }, []);
  const trace = useMemo(
    () => projectWorkTrace(events, snapshot, result, nowMs),
    [events, snapshot, result, nowMs],
  );

  return (
    <section className="panel work-trace">
      <div className="panel-heading">
        <div>
          <h2>Work Trace</h2>
          <p className="muted">
            Evidence from runner events, requested scope, and snapshot capture.
          </p>
        </div>
        <span className="pill">{trace.totalToolCalls} tool calls</span>
      </div>

      <div className="metrics">
        <Metric label="Changed files" value={trace.changedFiles.length} />
        <Metric label="Changed files read" value={trace.changedFilesRead.length} />
        <Metric label="File reviews" value={trace.fileReviews.length} />
        <Metric label="Direct reads" value={trace.readFiles.length} />
        <Metric label="Search scans" value={trace.searches.length} />
        <Metric label="Snapshot skips" value={trace.skippedFiles} />
        <Metric label="Max active" value={trace.maxActiveSessions ?? 0} />
      </div>

      {trace.model ? (
        <p className="muted">
          Model: <strong>{trace.model}</strong>
        </p>
      ) : null}

      {trace.riskHints.length > 0 ? (
        <section>
          <h3>Diff risk hints</h3>
          <ul className="trace-list">
            {trace.riskHints.map((hint) => (
              <li key={hint}>{hint}</li>
            ))}
          </ul>
        </section>
      ) : null}

      {trace.traceGaps.length > 0 ? (
        <section>
          <h3>Trace gaps</h3>
          <ul className="trace-list">
            {trace.traceGaps.map((gap) => (
              <li key={gap}>{gap}</li>
            ))}
          </ul>
        </section>
      ) : null}

      {trace.activeModelCalls.length > 0 ? (
        <section>
          <h3>Active model calls</h3>
          <ul className="trace-list">
            {trace.activeModelCalls.map((call) => (
              <li key={`${call.sessionId}:${call.turn}`}>
                <strong>{call.sessionId}</strong>
                <span>turn {call.turn}</span>
                <small>{formatElapsed(call.elapsedSeconds)} elapsed</small>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      <div className="trace-grid">
        <TraceList
          empty="No direct file reads have been observed yet."
          items={trace.readFiles}
          title="Files read directly"
        />
        <TraceList
          empty={
            trace.changedFiles.length === 0
              ? "Waiting for changed-file metadata."
              : "Every changed file has a review verdict."
          }
          items={trace.changedFilesNotReviewed}
          title="Changed files without verdicts"
        />
      </div>

      <section>
        <h3>File review verdicts</h3>
        {trace.fileReviews.length === 0 ? (
          <p className="muted">No file review verdicts have been recorded yet.</p>
        ) : (
          <ul className="trace-list file-review-list">
            {trace.fileReviews.map((review) => (
              <li key={`${review.path}:${review.verdict}`}>
                <strong>{review.path}</strong>
                <span>{review.verdict}</span>
                {review.findingId ? <small>Finding: {review.findingId}</small> : null}
                <small>{review.summary}</small>
                {review.relatedPaths.length > 0 ? (
                  <small>Related: {review.relatedPaths.join(", ")}</small>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      <div className="trace-grid">
        <TraceList
          empty={
            trace.changedFiles.length === 0
              ? "Waiting for changed-file metadata."
              : "Every changed file was read directly."
          }
          items={trace.changedFilesNotRead}
          title="Changed files not directly read"
        />
        <TraceList
          empty="No requested files outside the observed changed-file set."
          items={trace.requestedFilesNotRead}
          title="Requested files not directly read"
        />
      </div>

      <div className="trace-grid">
        <section>
          <h3>Sessions</h3>
          {trace.sessions.length === 0 ? (
            <p className="muted">Waiting for agent sessions.</p>
          ) : (
            <ul className="trace-list">
              {trace.sessions.map((session) => (
                <li key={session.id}>
                  <strong>{session.id}</strong>
                  <span>{session.status ?? "running"}</span>
                  <small>
                    {session.evidence.join(", ") || "evidence context unavailable"}
                  </small>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section>
          <h3>Searches</h3>
          {trace.searches.length === 0 ? (
            <p className="muted">No search coverage has been observed yet.</p>
          ) : (
            <ul className="trace-list">
              {trace.searches.map((search, index) => (
                <li key={`${search.query}-${index}`}>
                  <strong>{search.query}</strong>
                  <span>
                    {search.searchedFiles} searched, {search.skippedFiles} skipped
                  </span>
                  <small>{search.returnedMatches} returned match(es)</small>
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>

      <details>
        <summary>Tool counts and artifact summaries</summary>
        <div className="trace-grid">
          <TraceList
            empty="No completed tools yet."
            items={trace.toolCounts.map(
              (tool) => `${tool.name}: ${tool.count}`,
            )}
            title="Tools"
          />
          <TraceList
            empty="No artifacts yet."
            items={trace.artifactSummaries}
            title="Artifacts"
          />
        </div>
      </details>
    </section>
  );
}

function Metric({ label, value }: { label: string; value: number }) {
  return (
    <div className="metric">
      <span className="label">{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function TraceList({
  empty,
  items,
  title,
}: {
  empty: string;
  items: string[];
  title: string;
}) {
  return (
    <section>
      <h3>{title}</h3>
      {items.length === 0 ? (
        <p className="muted">{empty}</p>
      ) : (
        <ul className="trace-list">
          {items.map((item) => (
            <li key={item}>{item}</li>
          ))}
        </ul>
      )}
    </section>
  );
}

function ResultView({ result }: { result: ReviewResult }) {
  return (
    <div className="result">
      <p>
        <span className="label">Conclusion</span>
        <strong>{result.conclusion}</strong>
      </p>
      <p>{result.summary}</p>
      <div className="metrics compact">
        <Metric label="Considered" value={result.coverage.filesConsidered} />
        <Metric label="Captured text" value={result.coverage.filesReviewed} />
        <Metric label="Capture skips" value={result.coverage.filesSkipped} />
      </div>
      <h3>Findings</h3>
      {result.findings.length === 0 ? (
        <p>No findings.</p>
      ) : (
        <ul>
          {result.findings.map((finding) => (
            <li key={finding.id}>
              <strong>{finding.severity}</strong> {finding.title}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function QualityView({ quality }: { quality: ReviewQualityReport }) {
  return (
    <section className={`quality ${quality.passed ? "passed" : "failed"}`}>
      <div className="panel-heading compact-heading">
        <h3>Quality gate</h3>
        <span className="pill">{quality.passed ? "passed" : "failed"}</span>
      </div>
      <div className="metrics compact">
        <Metric label="Findings" value={quality.metrics.findings} />
        <Metric label="File verdicts" value={quality.metrics.fileReviews} />
        <Metric label="Missing verdicts" value={quality.metrics.missingFileVerdicts} />
        <Metric label="Duplicate verdicts" value={quality.metrics.duplicateFileVerdicts} />
        <Metric label="Speculative" value={quality.metrics.speculativeFindings} />
        <Metric label="Mismatches" value={quality.metrics.verdictMismatches} />
        <Metric label="Failures" value={quality.failures.length} />
      </div>
      {quality.failures.length > 0 ? (
        <ul className="trace-list">
          {quality.failures.slice(0, 8).map((failure) => (
            <li key={failure}>{failure}</li>
          ))}
        </ul>
      ) : null}
    </section>
  );
}

function isTerminalEvent(event: ReviewEvent): boolean {
  return (
    event.type === "review.result_created" ||
    event.type === "session.failed" ||
    event.type === "session.cancelled"
  );
}

interface WorkTraceProjection {
  activeModelCalls: ActiveModelCallTrace[];
  artifactSummaries: string[];
  changedFiles: string[];
  changedFilesNotRead: string[];
  changedFilesNotReviewed: string[];
  changedFilesRead: string[];
  fileReviews: FileReviewTrace[];
  maxActiveSessions?: number;
  model?: string;
  readFiles: string[];
  requestedFiles: string[];
  requestedFilesNotRead: string[];
  riskHints: string[];
  searches: SearchTrace[];
  sessions: SessionTrace[];
  skippedFiles: number;
  traceGaps: string[];
  toolCounts: ToolCountTrace[];
  totalToolCalls: number;
}

interface ActiveModelCallTrace {
  elapsedSeconds: number;
  sessionId: string;
  turn: number;
}

interface SearchTrace {
  query: string;
  returnedMatches: number;
  searchedFiles: number;
  skippedFiles: number;
}

interface FileReviewTrace {
  findingId?: string;
  path: string;
  relatedPaths: string[];
  summary: string;
  verdict: string;
}

interface SessionTrace {
  id: string;
  status?: string;
  evidence: string[];
}

interface ToolCountTrace {
  name: string;
  count: number;
}

interface SessionDraft {
  id: string;
  status?: string;
  evidence: Set<string>;
}

interface ToolCallDraft {
  sessionId?: string;
  toolId: string;
}

function projectWorkTrace(
  events: ReviewEvent[],
  snapshot: ReviewSnapshot | null,
  result: ReviewResult | null,
  nowMs: number,
): WorkTraceProjection {
  const requestedFiles = new Set(snapshot?.changedFiles ?? []);
  const observedChangedFiles = new Set<string>();
  const readFiles = new Set<string>();
  const riskHints = new Set<string>();
  const searches: SearchTrace[] = [];
  const fileReviews = new Map<string, FileReviewTrace>();
  const artifactSummaries: string[] = [];
  const traceGaps = new Set<string>();
  const activeModelCalls = new Map<string, ActiveModelCallTrace>();
  const sessions = new Map<string, SessionDraft>();
  const toolCalls = new Map<string, ToolCallDraft>();
  const toolCounts = new Map<string, number>();
  const completedReadCalls = new Map<string, string>();
  const completedSearchCalls = new Set<string>();
  const readCallsWithKnownPath = new Set<string>();
  const searchCallsWithKnownQuery = new Set<string>();
  let manifestSkippedFiles = 0;
  let pendingSessionId: string | undefined;

  for (const event of events) {
    const payload = asRecord(event.payload);
    if (!payload) {
      continue;
    }

    const context = asRecord(payload.context);
    const sessionId = stringValue(context?.sessionId);

    const modelStarted = asRecord(payload.modelStarted);
    if (modelStarted) {
      const modelSessionId =
        stringValue(modelStarted.sessionId) ?? sessionId ?? "unknown";
      const turn = numberValue(modelStarted.turn);
      activeModelCalls.set(modelCallKey(modelSessionId, turn), {
        elapsedSeconds: elapsedSeconds(event.timestampUtc, nowMs),
        sessionId: modelSessionId,
        turn,
      });
      ensureSession(sessions, modelSessionId).status ??= "running";
    }

    const modelCompleted = asRecord(payload.modelCompleted);
    if (modelCompleted) {
      const modelSessionId =
        stringValue(modelCompleted.sessionId) ?? sessionId ?? "unknown";
      activeModelCalls.delete(
        modelCallKey(modelSessionId, numberValue(modelCompleted.turn)),
      );
    }

    const manifest = asRecord(payload.repoManifestCompleted);
    if (manifest) {
      manifestSkippedFiles += numberValue(manifest.skipped);
    }

    const sessionStarted = asRecord(payload.sessionStarted);
    if (sessionStarted) {
      ensureSession(sessions, stringValue(sessionStarted.sessionId)).status ??=
        "running";
    }

    const sessionFinished = asRecord(payload.sessionFinished);
    if (sessionFinished) {
      const session = ensureSession(sessions, stringValue(sessionFinished.sessionId));
      session.status = stringValue(sessionFinished.status) ?? "done";
      if (session.status !== "done") {
        traceGaps.add(`${session.id} ended with status ${session.status}`);
      }
    }

    const toolBatchStarted = asRecord(payload.toolBatchStarted);
    if (toolBatchStarted) {
      pendingSessionId = stringValue(toolBatchStarted.sessionId);
    }

    const toolCompleted = asRecord(payload.toolCallCompleted);
    if (toolCompleted) {
      const toolId = stringValue(toolCompleted.toolId) ?? "unknown";
      const callId = stringValue(toolCompleted.callId);
      const effectiveSessionId = sessionId ?? pendingSessionId;
      increment(toolCounts, toolId);
      if (callId) {
        toolCalls.set(callId, { sessionId: effectiveSessionId, toolId });
      }
      if (toolCompleted.ok === false) {
        const errorCode = stringValue(toolCompleted.errorCode) ?? "tool_error";
        const errorMessage = stringValue(toolCompleted.errorMessage);
        const details = asRecord(toolCompleted.details);
        const path = stringValue(details?.path);
        traceGaps.add(
          `${effectiveSessionId ?? "unknown"} ${toolId}${path ? ` ${path}` : ""} failed (${errorCode})${
            errorMessage ? `: ${errorMessage}` : ""
          }`,
        );
      }
      if (effectiveSessionId && toolCompleted.ok === true) {
        markEvidence(ensureSession(sessions, effectiveSessionId), toolId);
      }
      if (
        (toolId === "read_file" ||
          toolId === "read_file_range" ||
          toolId === "read_head_file" ||
          toolId === "read_base_file") &&
        toolCompleted.ok === true
      ) {
        if (callId) {
          completedReadCalls.set(callId, toolId);
        } else {
          traceGaps.add(`${toolId} completed without a tool call id or path metadata`);
        }
      }
      if (toolId === "search_text" && toolCompleted.ok === true) {
        if (callId) {
          completedSearchCalls.add(callId);
        } else {
          traceGaps.add("search_text completed without a tool call id or query metadata");
        }
      }
      pendingSessionId = undefined;
    }

    const toolDenied = asRecord(payload.toolCallDenied);
    if (toolDenied) {
      const toolId = stringValue(toolDenied.toolId) ?? "unknown";
      increment(toolCounts, `${toolId} denied`);
    }

    const searchBatch = asRecord(payload.searchBatchCompleted);
    if (searchBatch && !context?.toolCallId) {
      traceGaps.add(
        `search batch has coverage without query metadata (${numberValue(
          searchBatch.searchedFiles,
        )} searched, ${numberValue(searchBatch.skippedFiles)} skipped)`,
      );
    }

    const modelFailed = asRecord(payload.modelFailed);
    if (modelFailed) {
      const failedSessionId = stringValue(modelFailed.sessionId) ?? sessionId ?? "unknown";
      activeModelCalls.delete(
        modelCallKey(failedSessionId, numberValue(modelFailed.turn)),
      );
      const attempt = numberValue(modelFailed.attempt);
      const retrying = modelFailed.retrying === true ? "retrying" : "final";
      const message = stringValue(modelFailed.message) ?? "model failure";
      traceGaps.add(
        `${failedSessionId} model call failed (${retrying}, attempt ${attempt}): ${message}`,
      );
    }

    const artifact = asRecord(payload.artifactCreated);
    if (!artifact) {
      continue;
    }

    const toolId = stringValue(artifact.toolId) ?? "unknown";
    const summary = stringValue(artifact.summary);
    if (summary) {
      artifactSummaries.push(summary);
    }

    const details = asRecord(artifact.details);
    const callId = stringValue(artifact.toolCallId);
    const call = callId ? toolCalls.get(callId) : undefined;
    const artifactSessionId = call?.sessionId ?? sessionId ?? pendingSessionId;
    if (artifactSessionId) {
      markEvidence(ensureSession(sessions, artifactSessionId), toolId);
    }

    if (toolId === "list_changed_files") {
      if (!details) {
        traceGaps.add("list_changed_files artifact omitted changed-file metadata");
      } else {
        for (const changedFile of stringArray(details.changedFiles)) {
          const path = changedFilePath(changedFile);
          if (path) {
            observedChangedFiles.add(path);
          }
        }
      }
    }

    if (toolId === "read_diff") {
      for (const hint of stringArray(details?.riskHints)) {
        riskHints.add(hint);
      }
    }

    if (toolId === "record_file_review") {
      const path = stringValue(details?.path);
      if (!path) {
        traceGaps.add("record_file_review artifact omitted path metadata");
        continue;
      }
      fileReviews.set(path, {
        findingId: stringValue(details?.findingId),
        path,
        relatedPaths: stringArray(details?.relatedPaths),
        summary: stringValue(details?.summary) ?? "",
        verdict: stringValue(details?.verdict) ?? "unknown",
      });
    }

    if (
      toolId === "read_file" ||
      toolId === "read_file_range" ||
      toolId === "read_head_file" ||
      toolId === "read_base_file"
    ) {
      const path = stringValue(details?.path);
      if (path) {
        readFiles.add(path);
        if (callId) {
          readCallsWithKnownPath.add(callId);
        }
      } else {
        traceGaps.add(`${toolId} artifact omitted path metadata`);
      }
    }

    if (toolId === "search_text") {
      const query = stringValue(details?.query);
      if (!query) {
        traceGaps.add("search_text artifact omitted query metadata");
        continue;
      }
      const search: SearchTrace = {
        query,
        returnedMatches: numberValue(details?.returnedMatches),
        searchedFiles: numberValue(details?.searchedFiles),
        skippedFiles: numberValue(details?.skippedFiles),
      };
      searches.push(search);
      if (callId) {
        searchCallsWithKnownQuery.add(callId);
      }
    }
  }

  for (const [callId, toolId] of completedReadCalls) {
    if (!readCallsWithKnownPath.has(callId)) {
      traceGaps.add(`${toolId} completed without path metadata (${callId})`);
    }
  }
  for (const callId of completedSearchCalls) {
    if (!searchCallsWithKnownQuery.has(callId)) {
      traceGaps.add(`search_text completed without query metadata (${callId})`);
    }
  }

  const requested = sorted([...requestedFiles]);
  const changed = sorted([
    ...requestedFiles,
    ...observedChangedFiles,
  ]);
  const read = sorted([...readFiles]);
  const unread = requested.filter((file) => !readFiles.has(file));
  const changedUnread = changed.filter((file) => !readFiles.has(file));
  const changedRead = changed.filter((file) => readFiles.has(file));
  const reviewedFiles = new Set(fileReviews.keys());
  const changedNotReviewed = changed.filter((file) => !reviewedFiles.has(file));
  const coverageSkippedFiles = result?.coverage.filesSkipped ?? 0;
  const metadata = asRecord((result as { metadata?: unknown } | null)?.metadata);
  const model = stringValue(metadata?.model);

  return {
    activeModelCalls: [...activeModelCalls.values()].sort((left, right) =>
      left.sessionId.localeCompare(right.sessionId) || left.turn - right.turn,
    ),
    artifactSummaries: artifactSummaries.slice(-12),
    changedFiles: changed,
    changedFilesNotRead: changedUnread,
    changedFilesNotReviewed: changedNotReviewed,
    changedFilesRead: changedRead,
    fileReviews: [...fileReviews.values()].sort((left, right) =>
      left.path.localeCompare(right.path),
    ),
    maxActiveSessions: snapshot?.maxActiveSessions,
    model,
    readFiles: read,
    requestedFiles: requested,
    requestedFilesNotRead: unread,
    riskHints: sorted([...riskHints]),
    searches,
    sessions: [...sessions.values()]
      .map((session) => ({
        id: session.id,
        status: session.status,
        evidence: [...session.evidence],
      }))
      .sort((left, right) => left.id.localeCompare(right.id)),
    skippedFiles: Math.max(
      manifestSkippedFiles,
      coverageSkippedFiles,
    ),
    traceGaps: sorted([...traceGaps]),
    toolCounts: [...toolCounts.entries()]
      .map(([name, count]) => ({ name, count }))
      .sort((left, right) => left.name.localeCompare(right.name)),
    totalToolCalls: [...toolCounts.values()].reduce((sum, count) => sum + count, 0),
  };
}

function modelCallKey(sessionId: string, turn: number): string {
  return `${sessionId}:${turn}`;
}

function elapsedSeconds(timestampUtc: string, nowMs: number): number {
  const startedAt = Date.parse(timestampUtc);
  if (!Number.isFinite(startedAt)) {
    return 0;
  }
  return Math.max(0, Math.floor((nowMs - startedAt) / 1000));
}

function formatElapsed(seconds: number): string {
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return `${minutes}m ${remainder}s`;
}

function ensureSession(
  sessions: Map<string, SessionDraft>,
  id: string | undefined,
): SessionDraft {
  const sessionId = id ?? "unknown";
  const existing = sessions.get(sessionId);
  if (existing) {
    return existing;
  }
  const session: SessionDraft = {
    id: sessionId,
    evidence: new Set(),
  };
  sessions.set(sessionId, session);
  return session;
}

function markEvidence(session: SessionDraft, toolId: string): void {
  if (toolId === "read_diff") {
    session.evidence.add("diff");
  }
  if (toolId === "read_file" || toolId === "read_head_file") {
    session.evidence.add("file");
  }
  if (toolId === "search_text") {
    session.evidence.add("search");
  }
}

function increment(counts: Map<string, number>, key: string): void {
  counts.set(key, (counts.get(key) ?? 0) + 1);
}

function sorted(values: string[]): string[] {
  return [...new Set(values)].sort((left, right) => left.localeCompare(right));
}

function changedFilePath(summary: string): string | undefined {
  const trimmed = summary.trim();
  if (!trimmed) {
    return undefined;
  }
  const prefixed = trimmed.match(
    /^(Added|Modified|Deleted|Renamed|Copied|TypeChanged)\s+(.+)$/,
  );
  return prefixed?.[2] ?? trimmed;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : undefined;
}

function stringArray(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function numberValue(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
