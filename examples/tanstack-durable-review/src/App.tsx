import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";
import { sourceKey } from "@muzen/sdk";
import type { ReviewEvent, ReviewResult, ReviewRole } from "@muzen/sdk";

import type {
  CreateReviewRequest,
  ReviewSnapshot,
  ReviewTargetKind,
} from "./shared.js";

const defaultRoles: ReviewRole[] = ["correctness", "security"];
const roleChoices: ReviewRole[] = [
  "generalist",
  "correctness",
  "security",
  "performance",
  "maintainability",
  "architecture",
];

export function NewReviewPage() {
  const navigate = useNavigate();
  const [sourceKind, setSourceKind] = useState<ReviewTargetKind>("local");
  const [repo, setRepo] = useState("../..");
  const [githubPullRequest, setGithubPullRequest] = useState(
    "https://github.com/maskdotdev/muzen/pull/1",
  );
  const [changedFiles, setChangedFiles] = useState("Cargo.toml");
  const [roles, setRoles] = useState<ReviewRole[]>(defaultRoles);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const body: CreateReviewRequest = {
        sourceKind,
        repo: sourceKind === "local" ? repo : undefined,
        githubPullRequest:
          sourceKind === "github" ? githubPullRequest : undefined,
        changedFiles: changedFiles
          .split(/\r?\n|,/)
          .map((value) => value.trim())
          .filter(Boolean),
        roles,
      };
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
  const { events, result, snapshot, error } = useReviewStream(reviewId);
  const terminal = snapshot?.status === "completed" || snapshot?.status === "failed";

  return (
    <section className="review">
      <div className="summary">
        <div>
          <span className="label">Review</span>
          <h1>{reviewId}</h1>
          {snapshot ? (
            <p className="source">
              <span className="label">Target</span>
              {sourceKey(snapshot.source)}
            </p>
          ) : null}
        </div>
        <span className={`status ${snapshot?.status ?? "queued"}`}>
          {snapshot?.status ?? "queued"}
        </span>
      </div>

      {error ? <p className="error">{error}</p> : null}

      <div className="grid">
        <section className="panel">
          <h2>Events</h2>
          <EventList events={events} />
        </section>

        <section className="panel">
          <h2>Result</h2>
          {result ? <ResultView result={result} /> : <p>{terminal ? "No result." : "Waiting..."}</p>}
        </section>
      </div>
    </section>
  );
}

function useReviewStream(reviewId: string) {
  const [events, setEvents] = useState<ReviewEvent[]>([]);
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

    return () => {
      closed = true;
      stream?.close();
    };
  }, [reviewId]);

  return useMemo(
    () => ({ events, snapshot, result, error, cursor: cursor.current }),
    [events, snapshot, result, error],
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

function ResultView({ result }: { result: ReviewResult }) {
  return (
    <div className="result">
      <p>
        <span className="label">Conclusion</span>
        <strong>{result.conclusion}</strong>
      </p>
      <p>{result.summary}</p>
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

function isTerminalEvent(event: ReviewEvent): boolean {
  return (
    event.type === "review.result_created" ||
    event.type === "session.failed" ||
    event.type === "session.cancelled"
  );
}
