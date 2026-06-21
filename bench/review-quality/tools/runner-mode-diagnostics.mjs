import fs from "node:fs";
import path from "node:path";

export function readTraceBundle(root, name, result = {}) {
  const auditPath = path.join(root, "traces", name, "audit-diagnostics.json");
  const audit = fs.existsSync(auditPath)
    ? readJson(auditPath).diagnostics
    : (result.audit?.diagnostics ?? {});
  const reviewEvents = readJsonl(path.join(root, "traces", name, "review-events.jsonl"));
  const runtimeEvents = readJsonl(path.join(root, "traces", name, "runtime-events.jsonl"));
  const runStart = readOptionalJson(path.join(root, "jobs", name, "run-start.json"));
  return {
    audit,
    reviewEvents,
    runtimeEvents,
    events: [...runtimeEvents, ...reviewEvents],
    runStart,
  };
}

export function primarySession(audit, sessionId = "review-orchestrator") {
  return audit?.sessions?.find((session) => session.sessionId === sessionId) ?? {};
}

export function runMaxToolCalls(runStart, fallback = 50) {
  return runStart?.sessions?.[0]?.budget?.maxToolCalls ?? fallback;
}

export function optionalInstrumentationSummary({
  audit,
  events,
  primarySessionId = "review-orchestrator",
}) {
  const session = primarySession(audit, primarySessionId);
  const modelLifecycle = [];
  const compactions = [];
  const candidateEvents = [];
  const finalizations = [];

  for (const record of events ?? []) {
    const normalized = normalizeEventRecord(record);
    if (!normalized.name) continue;
    const { name, payload, trace, details, sessionId, turn, timestampMs } = normalized;

    if (name === "modelStarted" || name === "modelCompleted" || name === "modelFailed") {
      if (sessionId === primarySessionId) {
        modelLifecycle.push(modelLifecycleEntry(name, payload, timestampMs));
      }
    }

    if (name === "sessionFinished" && sessionId === primarySessionId) {
      const explicitReason = firstString(
        payload.finalizationReason,
        payload.completionReason,
        payload.reason,
        payload.status,
      );
      finalizations.push({
        source: "sessionFinished",
        turn,
        reason: explicitReason,
        status: payload.status ?? null,
        completionKind: payload.completionKind ?? null,
      });
    }

    if (trace) {
      const traceKind = trace.traceKind ?? null;
      const traceSessionId = trace.sessionId ?? sessionId;
      const traceTurn = trace.turn ?? trace.turnId ?? turn;
      if (traceSessionId !== primarySessionId) continue;

      if (isFinalizationTrace(traceKind)) {
        finalizations.push({
          source: `agentTrace:${traceKind}`,
          turn: traceTurn,
          reason: firstString(
            details.finalizationReason,
            details.completionReason,
            details.reason,
            details.cause,
          ),
          status: details.status ?? null,
          completionKind: details.completionKind ?? null,
        });
      }

      if (traceKind === "transcript_compacted") {
        compactions.push({
          turn: traceTurn,
          evictedToolCallIds: arrayOrNull(details.evictedToolCallIds),
          evictedArtifactIds: arrayOrNull(details.evictedArtifactIds),
          evictedItemIds: arrayOrNull(details.evictedItemIds),
          retainedEvidenceIds: arrayOrNull(details.retainedEvidenceIds),
        });
      }

      if (traceKind?.startsWith("candidate_") || traceKind?.includes("candidate")) {
        candidateEvents.push({
          kind: traceKind,
          turn: traceTurn,
          timestampMs,
          candidateId: details.candidateId ?? null,
          validatorSessionId: details.validatorSessionId ?? null,
          decision: details.decision ?? null,
          phase: details.phase ?? null,
        });
      }
    }
  }

  const explicitFinalization = firstFinalization(finalizations, session);
  const lifecycleStats = modelLifecycleStats(modelLifecycle);
  const compactionStats = compactionProvenanceStats(compactions);
  const candidateStats = candidateLifecycleStats(candidateEvents);

  return {
    schemaVersion: "muzen.runner-mode-diagnostics.v1",
    finalization: {
      explicitReason: explicitFinalization?.reason ?? null,
      source: explicitFinalization?.source ?? null,
      status: explicitFinalization?.status ?? session.status ?? null,
      completionKind: explicitFinalization?.completionKind ?? session.completionKind ?? null,
    },
    modelLifecycle: lifecycleStats,
    compactionProvenance: compactionStats,
    candidateLifecycle: candidateStats,
    availability: {
      explicitFinalizationReason: Boolean(explicitFinalization?.reason),
      providerTimingSplit: lifecycleStats.providerTimingSplitCount > 0,
      providerRetryOrBackoff: lifecycleStats.retryCount.total > 0 || lifecycleStats.backoffMs.count > 0,
      transcriptCompactionIds: compactionStats.withAnyIds > 0,
      candidateLifecycleTimestamps: candidateStats.withTimestamp > 0,
    },
  };
}

export function compactOptionalInstrumentation(instrumentation) {
  return {
    finalizationReason: instrumentation.finalization.explicitReason,
    finalizationSource: instrumentation.finalization.source,
    providerTimingSplitCount: instrumentation.modelLifecycle.providerTimingSplitCount,
    retryCount: instrumentation.modelLifecycle.retryCount.total,
    backoffMs: instrumentation.modelLifecycle.backoffMs.total,
    compactionsWithIds: instrumentation.compactionProvenance.withAnyIds,
    candidateLifecycleEvents: instrumentation.candidateLifecycle.total,
    candidateLifecycleTimestamps: instrumentation.candidateLifecycle.withTimestamp,
    availability: instrumentation.availability,
  };
}

export function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

export function readOptionalJson(file) {
  return fs.existsSync(file) ? readJson(file) : null;
}

export function readJsonl(file) {
  if (!fs.existsSync(file)) return [];
  return fs
    .readFileSync(file, "utf8")
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line));
}

export function parseTimestampMs(timestamp) {
  if (!timestamp) return null;
  const numeric = /^(\d+(?:\.\d+)?)Z$/.exec(timestamp);
  if (numeric) return Number(numeric[1]) * 1000;
  const parsed = Date.parse(timestamp);
  return Number.isNaN(parsed) ? null : parsed;
}

function normalizeEventRecord(record) {
  const event = record.event ?? {};
  const name = Object.keys(event)[0] ?? null;
  const payload = name ? event[name] ?? {} : {};
  const trace = name === "agentTrace" ? payload : null;
  const sessionId =
    record.sessionId ??
    record.context?.sessionId ??
    record.context?.session_id ??
    payload.sessionId ??
    trace?.sessionId ??
    null;
  const turn =
    record.turn ??
    record.context?.turnId ??
    record.context?.turn_id ??
    payload.turn ??
    payload.turnId ??
    trace?.turn ??
    trace?.turnId ??
    null;
  return {
    name,
    payload,
    trace,
    details: trace?.details ?? {},
    sessionId,
    turn,
    timestampMs: parseTimestampMs(record.timestampUtc ?? record.atUtc),
  };
}

function modelLifecycleEntry(name, payload, timestampMs) {
  return {
    name,
    timestampMs,
    queuedAtMs: parseTimestampMs(payload.queuedAtUtc),
    requestStartedAtMs: parseTimestampMs(payload.requestStartedAtUtc),
    firstTokenAtMs: parseTimestampMs(payload.firstTokenAtUtc),
    completedAtMs: parseTimestampMs(payload.completedAtUtc),
    queuedMs: numberOrNull(payload.queuedMs ?? payload.queueWaitMs),
    requestMs: numberOrNull(payload.requestMs ?? payload.providerRequestMs),
    firstTokenMs: numberOrNull(payload.firstTokenMs ?? payload.timeToFirstTokenMs),
    retryCount: numberOrNull(payload.retryCount ?? payload.retries),
    backoffMs: numberOrNull(payload.rateLimitBackoffMs ?? payload.backoffMs),
    limiterWaitMs: numberOrNull(payload.limiterWaitMs),
  };
}

function modelLifecycleStats(entries) {
  const derivedQueueMs = entries.map((entry) =>
    entry.queuedMs ??
    nullableDelta(entry.requestStartedAtMs, entry.queuedAtMs) ??
    entry.limiterWaitMs,
  );
  const derivedRequestMs = entries.map((entry) =>
    entry.requestMs ?? nullableDelta(entry.completedAtMs, entry.requestStartedAtMs),
  );
  const firstTokenMs = entries.map((entry) =>
    entry.firstTokenMs ?? nullableDelta(entry.firstTokenAtMs, entry.requestStartedAtMs),
  );
  const retryCounts = entries.map((entry) => entry.retryCount);
  const backoffs = entries.map((entry) => entry.backoffMs);
  return {
    eventCount: entries.length,
    providerTimingSplitCount: entries.filter(
      (entry) =>
        entry.queuedAtMs != null ||
        entry.requestStartedAtMs != null ||
        entry.firstTokenAtMs != null ||
        entry.completedAtMs != null ||
        entry.queuedMs != null ||
        entry.requestMs != null ||
        entry.firstTokenMs != null,
    ).length,
    queueMs: stats(derivedQueueMs),
    requestMs: stats(derivedRequestMs),
    firstTokenMs: stats(firstTokenMs),
    retryCount: totalStats(retryCounts),
    backoffMs: totalStats(backoffs),
  };
}

function compactionProvenanceStats(compactions) {
  const withToolCallIds = compactions.filter((item) => item.evictedToolCallIds?.length).length;
  const withArtifactIds = compactions.filter((item) => item.evictedArtifactIds?.length).length;
  const withItemIds = compactions.filter((item) => item.evictedItemIds?.length).length;
  const withEvidenceIds = compactions.filter((item) => item.retainedEvidenceIds?.length).length;
  return {
    count: compactions.length,
    withToolCallIds,
    withArtifactIds,
    withItemIds,
    withEvidenceIds,
    withAnyIds: compactions.filter(
      (item) =>
        item.evictedToolCallIds?.length ||
        item.evictedArtifactIds?.length ||
        item.evictedItemIds?.length ||
        item.retainedEvidenceIds?.length,
    ).length,
  };
}

function candidateLifecycleStats(events) {
  return {
    total: events.length,
    withTimestamp: events.filter((event) => event.timestampMs != null).length,
    byKind: countObject(events.map((event) => event.kind)),
    decisions: countObject(events.map((event) => event.decision)),
    phases: countObject(events.map((event) => event.phase)),
  };
}

function firstFinalization(finalizations, session) {
  const sessionReason = firstString(
    session.finalizationReason,
    session.completionReason,
    session.reason,
  );
  if (sessionReason) {
    return {
      source: "audit.sessions",
      reason: sessionReason,
      status: session.status ?? null,
      completionKind: session.completionKind ?? null,
    };
  }
  return (
    finalizations.find((item) => item.reason) ??
    finalizations.find((item) => item.status || item.completionKind) ??
    null
  );
}

function isFinalizationTrace(traceKind) {
  return (
    traceKind === "session_finalized" ||
    traceKind === "session_finalization" ||
    traceKind === "model_turn_finalized" ||
    traceKind === "finalization_decision" ||
    traceKind === "review_finalization"
  );
}

function nullableDelta(left, right) {
  return Number.isFinite(left) && Number.isFinite(right) ? left - right : null;
}

function stats(values) {
  const clean = values.filter((value) => Number.isFinite(value)).sort((left, right) => left - right);
  if (clean.length === 0) {
    return { count: 0, min: null, p50: null, p95: null, max: null, mean: null };
  }
  return {
    count: clean.length,
    min: round(clean[0], 2),
    p50: round(percentile(clean, 0.5), 2),
    p95: round(percentile(clean, 0.95), 2),
    max: round(clean.at(-1), 2),
    mean: round(mean(clean), 2),
  };
}

function totalStats(values) {
  const basic = stats(values);
  return {
    ...basic,
    total: round(sum(values), 2),
  };
}

function percentile(sortedValues, percentileValue) {
  const index = Math.min(
    sortedValues.length - 1,
    Math.max(0, Math.ceil(sortedValues.length * percentileValue) - 1),
  );
  return sortedValues[index];
}

function mean(values) {
  const clean = values.filter((value) => Number.isFinite(value));
  return clean.length === 0 ? null : sum(clean) / clean.length;
}

function sum(values) {
  return values.reduce((total, value) => total + (Number.isFinite(value) ? value : 0), 0);
}

function countObject(values) {
  const counts = new Map();
  for (const value of values) {
    if (value == null) continue;
    counts.set(value, (counts.get(value) ?? 0) + 1);
  }
  return Object.fromEntries([...counts.entries()].sort(([left], [right]) => left.localeCompare(right)));
}

function round(value, digits) {
  return Number.isFinite(value) ? Number(value.toFixed(digits)) : value;
}

function firstString(...values) {
  return values.find((value) => typeof value === "string" && value.length > 0) ?? null;
}

function arrayOrNull(value) {
  return Array.isArray(value) ? value : null;
}

function numberOrNull(value) {
  const numeric = Number(value);
  return Number.isFinite(numeric) ? numeric : null;
}
