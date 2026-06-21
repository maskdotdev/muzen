#!/usr/bin/env node

// Muzen Eval UI — a zero-dependency local server for launching review-quality
// evals and inspecting the full trace of a run in the browser.
//
// It serves a small single-page app (public/) and a JSON API over the
// review-quality result + trace artifacts written under
// bench/results-review-quality. It can also spawn a curated set of eval
// scripts and stream their output live.
//
// Scope: eval tooling only. It never touches Muzen core product code, never
// reads .env files, never echoes secret values, and binds to loopback only.
//
//   node bench/review-quality/eval-ui/server.mjs [--port 7777] [--host 127.0.0.1]

import http from "node:http";
import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const PUBLIC_DIR = path.join(__dirname, "public");
const RESULTS_ROOT = path.join(REPO_ROOT, "bench", "results-review-quality");
const REVIEW_QUALITY_DIR = path.join(REPO_ROOT, "bench", "review-quality");

// Filenames the trace harness writes into a trace directory.
const TRACE_MARKER = "agent-trace.json";
const COVERAGE_FILE = "event-coverage.json";
const AUDIT_FILE = "audit-diagnostics.json";
const EVENTS_FILE = "all-events.jsonl";

const args = parseCliArgs(process.argv.slice(2));
const PORT = Number(args.port || process.env.MUZEN_EVAL_UI_PORT || 7777);
const HOST = args.host || "127.0.0.1";

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

// A read is only ever allowed inside the results root. Resolve, then confirm
// the resolved path is contained — this blocks `..` traversal and absolute
// escapes (including any attempt to reach a .env file elsewhere in the repo).
function isInside(root, target) {
  const rel = path.relative(root, target);
  return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
}

// Canonical results root (symlinks resolved once) — the boundary we compare
// every resolved path against.
const REAL_RESULTS_ROOT = (() => {
  try {
    return fs.realpathSync(RESULTS_ROOT);
  } catch {
    return RESULTS_ROOT;
  }
})();

function safeResultsPath(relOrAbs) {
  const abs = path.isAbsolute(relOrAbs)
    ? path.resolve(relOrAbs)
    : path.resolve(RESULTS_ROOT, relOrAbs);
  // Lexical gate first (also covers paths that don't exist yet).
  if (!isInside(RESULTS_ROOT, abs)) return null;
  if (path.basename(abs).startsWith(".env")) return null;
  // If it exists, canonicalize and re-check so a symlink inside the results
  // dir can't point fs.readFile() at a target outside it (or at a .env file).
  if (fs.existsSync(abs)) {
    let real;
    try {
      real = fs.realpathSync(abs);
    } catch {
      return null;
    }
    if (!isInside(REAL_RESULTS_ROOT, real)) return null;
    if (path.basename(real).startsWith(".env")) return null;
    return real;
  }
  return abs;
}

// ---------------------------------------------------------------------------
// Run discovery (cheap: stat-only walk, cached; details loaded per page)
// ---------------------------------------------------------------------------

let runIndexCache = null; // { builtAt, traceDirs:[{rel,mtimeMs}], resultFiles:[{rel,mtimeMs}] }

async function buildRunIndex() {
  const traceDirs = [];
  const resultFiles = [];

  // Top-level result JSONs are the headline runs.
  let topEntries = [];
  try {
    topEntries = await fsp.readdir(RESULTS_ROOT, { withFileTypes: true });
  } catch {
    topEntries = [];
  }
  for (const entry of topEntries) {
    if (entry.isFile() && entry.name.endsWith(".json")) {
      const abs = path.join(RESULTS_ROOT, entry.name);
      const st = await statSafe(abs);
      if (st) resultFiles.push({ rel: path.relative(RESULTS_ROOT, abs), mtimeMs: st.mtimeMs });
    }
  }

  // Trace directories anywhere under the results root carry the rich timeline.
  await walkForTraceDirs(RESULTS_ROOT, traceDirs, 0);

  runIndexCache = { builtAt: Date.now(), traceDirs, resultFiles };
  return runIndexCache;
}

async function walkForTraceDirs(dir, out, depth) {
  if (depth > 6) return;
  let entries;
  try {
    entries = await fsp.readdir(dir, { withFileTypes: true });
  } catch {
    return;
  }
  const hasMarker = entries.some((e) => e.isFile() && e.name === TRACE_MARKER);
  if (hasMarker) {
    const st = await statSafe(dir);
    if (st) out.push({ rel: path.relative(RESULTS_ROOT, dir), mtimeMs: st.mtimeMs });
    return; // do not descend into a trace dir's own artifacts
  }
  for (const entry of entries) {
    if (!entry.isDirectory() || entry.isSymbolicLink()) continue;
    await walkForTraceDirs(path.join(dir, entry.name), out, depth + 1);
  }
}

async function statSafe(p) {
  try {
    return await fsp.stat(p);
  } catch {
    return null;
  }
}

async function getRunIndex(refresh) {
  if (!runIndexCache || refresh) return buildRunIndex();
  return runIndexCache;
}

// Lightweight per-run summary used for the list. Cached by mtime.
const summaryCache = new Map(); // id -> { mtimeMs, summary }

async function loadListEntry(kind, rel, mtimeMs) {
  const id = `${kind}::${rel}`;
  const cached = summaryCache.get(id);
  if (cached && cached.mtimeMs === mtimeMs) return cached.summary;

  let summary = { id, kind, rel, name: path.basename(rel), mtimeMs };
  if (kind === "trace") {
    const cov = await readJsonSafe(path.join(RESULTS_ROOT, rel, COVERAGE_FILE));
    const trace = cov ? null : await readJsonSafe(path.join(RESULTS_ROOT, rel, TRACE_MARKER));
    const c = cov || (trace && trace.report) || {};
    summary = {
      ...summary,
      title: deriveTitle(cov && cov.resultPath) || path.basename(rel),
      metrics: {
        events: c.eventCount ?? c.totalEvents ?? null,
        sessions: c.sessions ?? null,
        modelTurns: c.modelTurns ?? null,
        toolCalls: c.toolCallsCompleted ?? c.toolCallsRequested ?? null,
        findings: c.findingsRecorded ?? null,
      },
      resultPath: cov && cov.resultPath ? path.relative(RESULTS_ROOT, cov.resultPath) : null,
    };
  } else {
    const result = await readJsonSafe(path.join(RESULTS_ROOT, rel));
    if (!result || !isResultDoc(result)) {
      summaryCache.set(id, { mtimeMs, summary: null });
      return null;
    }
    const bench = result.benchmark || {};
    summary = {
      ...summary,
      title: path.basename(rel, ".json"),
      mode: result.mode || result.inputs?.runMode || null,
      model: result.inputs?.model || null,
      reviewValid: result.reviewValid ?? null,
      metrics: {
        findings: Array.isArray(result.findings) ? result.findings.length : null,
        hits: Array.isArray(bench.hits) ? bench.hits.length : null,
        misses: Array.isArray(bench.misses) ? bench.misses.length : null,
        hitRate: typeof bench.hitRate === "number" ? bench.hitRate : null,
        falsePositives: bench.falsePositiveCount ?? null,
      },
    };
  }
  summaryCache.set(id, { mtimeMs, summary });
  return summary;
}

function isResultDoc(doc) {
  return (
    doc &&
    typeof doc === "object" &&
    (Object.prototype.hasOwnProperty.call(doc, "reviewValid") ||
      (doc.benchmark && doc.findings) ||
      (typeof doc.schemaVersion === "string" && doc.schemaVersion.includes("review-quality")))
  );
}

function deriveTitle(resultPath) {
  if (!resultPath) return null;
  return path.basename(String(resultPath), ".json");
}

async function readJsonSafe(p) {
  const abs = safeResultsPath(p);
  if (!abs) return null;
  try {
    return JSON.parse(await fsp.readFile(abs, "utf8"));
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

async function apiRuns(url, res) {
  const refresh = url.searchParams.get("refresh") === "1";
  const q = (url.searchParams.get("q") || "").toLowerCase();
  const kindFilter = url.searchParams.get("kind") || "all";
  const offset = Math.max(0, Number(url.searchParams.get("offset") || 0));
  const limit = Math.min(200, Math.max(1, Number(url.searchParams.get("limit") || 40)));

  const index = await getRunIndex(refresh);
  let candidates = [];
  if (kindFilter === "all" || kindFilter === "trace") {
    candidates.push(...index.traceDirs.map((d) => ({ kind: "trace", ...d })));
  }
  if (kindFilter === "all" || kindFilter === "result") {
    candidates.push(...index.resultFiles.map((d) => ({ kind: "result", ...d })));
  }
  if (q) candidates = candidates.filter((c) => c.rel.toLowerCase().includes(q));
  candidates.sort((a, b) => b.mtimeMs - a.mtimeMs);

  const total = candidates.length;
  const page = candidates.slice(offset, offset + limit);
  const entries = [];
  for (const c of page) {
    const summary = await loadListEntry(c.kind, c.rel, c.mtimeMs);
    if (summary) entries.push(summary);
  }
  sendJson(res, 200, { total, offset, limit, builtAt: index.builtAt, entries });
}

async function apiRunDetail(url, res) {
  const id = url.searchParams.get("id") || "";
  const sep = id.indexOf("::");
  if (sep < 0) return sendJson(res, 400, { error: "bad id" });
  const kind = id.slice(0, sep);
  const rel = id.slice(sep + 2);

  if (kind === "trace") {
    const dir = safeResultsPath(rel);
    if (!dir) return sendJson(res, 400, { error: "bad path" });
    const traceReport = await readJsonSafe(path.join(dir, TRACE_MARKER));
    if (!traceReport) return sendJson(res, 404, { error: "trace not found" });
    const coverage = await readJsonSafe(path.join(dir, COVERAGE_FILE));
    const audit = await readJsonSafe(path.join(dir, AUDIT_FILE));
    let result = null;
    const resultPathAbs = coverage && coverage.resultPath ? safeResultsPath(coverage.resultPath) : null;
    if (resultPathAbs && fs.existsSync(resultPathAbs)) {
      result = await readJsonSafe(resultPathAbs);
    }
    const eventsPath = path.join(dir, EVENTS_FILE);
    const eventsExist = fs.existsSync(eventsPath);
    return sendJson(res, 200, {
      id,
      kind,
      rel,
      traceReport: traceReport.report || traceReport,
      coverage,
      audit: audit && audit.diagnostics ? audit.diagnostics : audit,
      result,
      hasEvents: eventsExist,
      resultRel: resultPathAbs ? path.relative(RESULTS_ROOT, resultPathAbs) : null,
    });
  }

  if (kind === "result") {
    const result = await readJsonSafe(rel);
    if (!result || !isResultDoc(result)) return sendJson(res, 404, { error: "result not found" });
    return sendJson(res, 200, { id, kind, rel, result });
  }
  return sendJson(res, 400, { error: "unknown kind" });
}

async function apiEvents(url, res) {
  const rel = url.searchParams.get("trace") || "";
  const offset = Math.max(0, Number(url.searchParams.get("offset") || 0));
  const limit = Math.min(2000, Math.max(1, Number(url.searchParams.get("limit") || 300)));
  const q = (url.searchParams.get("q") || "").toLowerCase();
  const dir = safeResultsPath(rel);
  if (!dir) return sendJson(res, 400, { error: "bad path" });
  const eventsPath = path.join(dir, EVENTS_FILE);
  if (!isInside(RESULTS_ROOT, eventsPath) || !fs.existsSync(eventsPath)) {
    return sendJson(res, 404, { error: "events not found" });
  }
  let raw;
  try {
    raw = await fsp.readFile(eventsPath, "utf8");
  } catch {
    return sendJson(res, 500, { error: "read failed" });
  }
  let lines = raw.split("\n").filter((l) => l.trim());
  if (q) lines = lines.filter((l) => l.toLowerCase().includes(q));
  const total = lines.length;
  const events = [];
  for (const line of lines.slice(offset, offset + limit)) {
    try {
      events.push(JSON.parse(line));
    } catch {
      /* skip malformed line */
    }
  }
  sendJson(res, 200, { total, offset, limit, events });
}

// ---------------------------------------------------------------------------
// Launcher
// ---------------------------------------------------------------------------

const LAUNCH_PRESETS = [
  {
    id: "check-local",
    label: "Local gate (no model)",
    description: "No-model harness self-tests, fixtures, comparator, anti-targeting guard.",
    script: "bench/review-quality/check-local.mjs",
    needsModel: false,
    needsRunner: false,
    fixtureSet: null,
    buildArgs: () => [],
  },
  {
    id: "synthetic-positive",
    label: "Synthetic positive (1 fixture)",
    description: "Domain-neutral positive control — should surface the planted bug.",
    script: "bench/review-quality/run-synthetic-positive.mjs",
    needsModel: true,
    needsRunner: true,
    fixtureSet: "synthetic",
    buildArgs: (ctx) => [
      "--fixture", ctx.fixture,
      "--mode", "direct_sessions",
      "--sessions", "1",
      "--runner-path", ctx.runnerPath,
      "--output-dir", ctx.outDir,
      "--trace-output-dir", ctx.traceDir,
    ],
  },
  {
    id: "anti-cheat",
    label: "Anti-cheat negative (1 fixture)",
    description: "Safe twin — should stay silent (precision tripwire).",
    script: "bench/review-quality/run-anti-cheat.mjs",
    needsModel: true,
    needsRunner: true,
    fixtureSet: "anti-cheat",
    buildArgs: (ctx) => [
      "--fixture", ctx.fixture,
      "--mode", "direct_sessions",
      "--sessions", "1",
      "--runner-path", ctx.runnerPath,
      "--output-dir", ctx.outDir,
      "--trace-output-dir", ctx.traceDir,
    ],
  },
  {
    id: "synthetic-suite",
    label: "Synthetic suite (positive + paired negative)",
    description: "Runs the positive fixture and its safe twin together.",
    script: "bench/review-quality/run-synthetic-suite.mjs",
    needsModel: true,
    needsRunner: true,
    fixtureSet: "synthetic",
    buildArgs: (ctx) => [
      "--positive-fixture", ctx.fixture,
      "--mode", "direct_sessions",
      "--sessions", "1",
      "--runner-path", ctx.runnerPath,
      "--output-dir", ctx.outDir,
      "--trace-output-dir", ctx.traceDir,
    ],
  },
];

const jobs = new Map(); // jobId -> { lines, status, code, traceDir, listeners:Set, startedAt }
const MAX_JOB_LINES = 5000; // keep the tail of very chatty runs, bound memory
const JOB_TTL_MS = 10 * 60 * 1000; // drop finished jobs 10 min after completion

function readManifestFixtures(file) {
  try {
    const j = JSON.parse(fs.readFileSync(path.join(REVIEW_QUALITY_DIR, file), "utf8"));
    return (j.fixtures || []).map((f) => f.fixtureDir || f.id || f.name).filter(Boolean);
  } catch {
    return [];
  }
}

function detectRunners() {
  const out = [];
  for (const rel of ["target/release/muzen-runner", "target/debug/muzen-runner"]) {
    const abs = path.join(REPO_ROOT, rel);
    if (fs.existsSync(abs)) out.push(rel);
  }
  return out;
}

function apiSuites(res) {
  const runners = detectRunners();
  sendJson(res, 200, {
    presets: LAUNCH_PRESETS.map((p) => ({
      id: p.id,
      label: p.label,
      description: p.description,
      needsModel: p.needsModel,
      needsRunner: p.needsRunner,
      fixtureSet: p.fixtureSet,
    })),
    fixtures: {
      synthetic: readManifestFixtures("synthetic-fixtures.json"),
      "anti-cheat": readManifestFixtures("anti-cheat-fixtures.json"),
    },
    env: {
      runners,
      defaultRunner: runners[0] || "target/release/muzen-runner",
      hasModelEnv: Boolean(process.env.MODEL),
      modelEnv: process.env.MODEL || null,
      hasOpenAiKey: Boolean(process.env.OPENAI_API_KEY),
      hasOpenAiBaseUrl: Boolean(process.env.OPENAI_BASE_URL),
    },
  });
}

const SAFE_TOKEN = /^[\w.\-:/]+$/;

async function apiLaunch(req, res) {
  const body = await readBody(req);
  let payload;
  try {
    payload = JSON.parse(body || "{}");
  } catch {
    return sendJson(res, 400, { error: "invalid json" });
  }
  const preset = LAUNCH_PRESETS.find((p) => p.id === payload.presetId);
  if (!preset) return sendJson(res, 400, { error: "unknown preset" });

  const ctx = {};
  if (preset.fixtureSet) {
    const allowed = readManifestFixtures(
      preset.fixtureSet === "synthetic" ? "synthetic-fixtures.json" : "anti-cheat-fixtures.json",
    );
    if (!allowed.includes(payload.fixture)) {
      return sendJson(res, 400, { error: "fixture not in manifest" });
    }
    ctx.fixture = payload.fixture;
  }

  if (preset.needsRunner) {
    const runners = detectRunners();
    const requested = payload.runnerPath || runners[0];
    if (!requested || !runners.includes(requested)) {
      return sendJson(res, 400, { error: "runner not built — run: cargo build --release --bin muzen-runner" });
    }
    ctx.runnerPath = requested;
  }

  const stamp = new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "Z");
  const slug = `${preset.id}${ctx.fixture ? "-" + ctx.fixture : ""}`.replace(/[^\w.-]/g, "-");
  const runDir = path.join("eval-ui-runs", `${slug}-${stamp}`);
  ctx.outDir = path.join(RESULTS_ROOT, runDir);
  ctx.traceDir = path.join(RESULTS_ROOT, runDir, "trace");

  const env = { ...process.env };
  if (preset.needsModel) {
    const model = payload.model || process.env.MODEL || "gpt-5.4-mini";
    if (!SAFE_TOKEN.test(model)) return sendJson(res, 400, { error: "invalid model" });
    env.MODEL = model;
    if (payload.useProxy) {
      env.OPENAI_BASE_URL = "http://127.0.0.1:4141/v1";
      env.OPENAI_API_KEY = env.OPENAI_API_KEY || "muzen-codex-proxy";
    }
  }

  const cmdArgs = [preset.script, ...preset.buildArgs(ctx)];
  const jobId = randomUUID();
  const job = {
    id: jobId,
    lines: [],
    nextSeq: 0,
    status: "running",
    code: null,
    startedAt: Date.now(),
    traceDir: preset.needsRunner ? path.relative(RESULTS_ROOT, ctx.traceDir) : null,
    resultRel: preset.needsRunner ? path.relative(RESULTS_ROOT, path.join(ctx.outDir)) : null,
    command: `node ${cmdArgs.join(" ")}`,
    listeners: new Set(),
  };
  jobs.set(jobId, job);

  const child = spawn("node", cmdArgs, { cwd: REPO_ROOT, env });
  pushJobLine(job, `$ MODEL=${env.MODEL || "(none)"} node ${cmdArgs.join(" ")}`, "cmd");
  child.stdout.on("data", (d) => splitLines(d).forEach((l) => pushJobLine(job, l, "out")));
  child.stderr.on("data", (d) => splitLines(d).forEach((l) => pushJobLine(job, l, "err")));
  child.on("error", (e) => {
    pushJobLine(job, `spawn error: ${e.message}`, "err");
    finishJob(job, -1);
  });
  child.on("close", (code) => finishJob(job, code));

  sendJson(res, 200, { jobId, traceDir: job.traceDir, command: job.command });
}

function splitLines(buf) {
  return buf.toString("utf8").split(/\r?\n/).filter((l) => l.length > 0);
}

function pushJobLine(job, text, stream) {
  const line = { seq: job.nextSeq++, text, stream, t: Date.now() };
  job.lines.push(line);
  if (job.lines.length > MAX_JOB_LINES) job.lines.shift(); // bound buffered scrollback
  for (const send of job.listeners) send({ type: "line", line });
}

function finishJob(job, code) {
  if (job.status !== "running") return;
  job.status = code === 0 ? "completed" : "failed";
  job.code = code;
  // Did the run produce a usable trace?
  let traceId = null;
  if (job.traceDir) {
    const marker = path.join(RESULTS_ROOT, job.traceDir, TRACE_MARKER);
    if (fs.existsSync(marker)) traceId = `trace::${job.traceDir}`;
  }
  job.traceId = traceId;
  runIndexCache = null; // new run; force reindex on next list
  for (const send of job.listeners) send({ type: "done", status: job.status, code, traceId });
  // Reclaim the job after a grace window so late SSE reconnects still resolve.
  setTimeout(() => jobs.delete(job.id), JOB_TTL_MS).unref();
}

function apiLaunchStream(url, req, res) {
  const jobId = url.searchParams.get("jobId");
  const job = jobs.get(jobId);
  if (!job) {
    res.writeHead(404, { "content-type": "text/plain" });
    res.end("unknown job");
    return;
  }
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  const send = (obj) => {
    try {
      res.write(`data: ${JSON.stringify(obj)}\n\n`);
    } catch {
      /* client gone */
    }
  };
  // Replay buffered output, then live-subscribe.
  for (const line of job.lines) send({ type: "line", line });
  if (job.status !== "running") {
    send({ type: "done", status: job.status, code: job.code, traceId: job.traceId || null });
    res.end();
    return;
  }
  job.listeners.add(send);
  const wrapped = (obj) => {
    send(obj);
    if (obj.type === "done") {
      job.listeners.delete(wrapped);
      try {
        res.end();
      } catch {
        /* noop */
      }
    }
  };
  job.listeners.delete(send);
  job.listeners.add(wrapped);
  req.on("close", () => job.listeners.delete(wrapped));
}

// ---------------------------------------------------------------------------
// Static + routing
// ---------------------------------------------------------------------------

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
};

async function serveStatic(pathname, res) {
  const clean = pathname === "/" ? "/index.html" : pathname;
  const abs = path.join(PUBLIC_DIR, path.normalize(clean));
  if (!isInside(PUBLIC_DIR, abs)) return notFound(res);
  try {
    const data = await fsp.readFile(abs);
    res.writeHead(200, { "content-type": MIME[path.extname(abs)] || "application/octet-stream" });
    res.end(data);
  } catch {
    notFound(res);
  }
}

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url, `http://${req.headers.host}`);
    const { pathname } = url;

    if (pathname === "/api/health") return sendJson(res, 200, { ok: true });
    if (pathname === "/api/runs") return await apiRuns(url, res);
    if (pathname === "/api/run") return await apiRunDetail(url, res);
    if (pathname === "/api/events") return await apiEvents(url, res);
    if (pathname === "/api/suites") return apiSuites(res);
    if (pathname === "/api/launch" && req.method === "POST") return await apiLaunch(req, res);
    if (pathname === "/api/launch/stream") return apiLaunchStream(url, req, res);
    if (pathname.startsWith("/api/")) return sendJson(res, 404, { error: "not found" });

    return await serveStatic(pathname, res);
  } catch (err) {
    sendJson(res, 500, { error: String(err && err.message ? err.message : err) });
  }
});

server.listen(PORT, HOST, () => {
  // eslint-disable-next-line no-console
  console.log(`\n  Muzen Eval UI  →  http://${HOST}:${PORT}\n`);
  console.log(`  results root:  ${path.relative(REPO_ROOT, RESULTS_ROOT)}`);
  console.log(`  runners:       ${detectRunners().join(", ") || "(none built)"}\n`);
});

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

function sendJson(res, status, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(status, { "content-type": "application/json; charset=utf-8" });
  res.end(body);
}

function notFound(res) {
  res.writeHead(404, { "content-type": "text/plain" });
  res.end("not found");
}

function readBody(req) {
  return new Promise((resolve) => {
    let data = "";
    req.on("data", (c) => {
      data += c;
      if (data.length > 1_000_000) req.destroy();
    });
    req.on("end", () => resolve(data));
    req.on("error", () => resolve(data));
  });
}

function parseCliArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      const key = a.slice(2).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
      const next = argv[i + 1];
      if (next && !next.startsWith("--")) {
        out[key] = next;
        i++;
      } else {
        out[key] = true;
      }
    }
  }
  return out;
}
