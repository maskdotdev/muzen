"use strict";

// Muzen Eval UI — vanilla SPA. Renders the run list, full trace inspection,
// and a launcher with live streamed output. No build step, no dependencies.

const state = {
  entries: [],
  total: 0,
  offset: 0,
  limit: 40,
  kind: "all",
  q: "",
  selectedId: null,
  detail: null,
  tab: "summary",
  events: null,
  suites: null,
};

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]),
  );
}

function num(n) {
  return n == null ? "—" : typeof n === "number" ? n.toLocaleString() : esc(n);
}

function pct(x) {
  return typeof x === "number" ? `${Math.round(x * 100)}%` : "—";
}

function ago(ms) {
  if (!ms) return "";
  const d = (Date.now() - ms) / 1000;
  if (d < 60) return `${Math.round(d)}s ago`;
  if (d < 3600) return `${Math.round(d / 60)}m ago`;
  if (d < 86400) return `${Math.round(d / 3600)}h ago`;
  return `${Math.round(d / 86400)}d ago`;
}

async function api(path) {
  const r = await fetch(path);
  if (!r.ok) throw new Error(`${r.status} ${path}`);
  return r.json();
}

// ---- Sidebar / run list ---------------------------------------------------

async function fetchRuns(reset) {
  if (reset) state.offset = 0;
  const params = new URLSearchParams({
    offset: state.offset,
    limit: state.limit,
    kind: state.kind,
    q: state.q,
  });
  const data = await api(`/api/runs?${params}`);
  state.total = data.total;
  if (reset || state.offset === 0) state.entries = data.entries;
  else state.entries = state.entries.concat(data.entries);
  renderSidebar();
}

function renderSidebar() {
  const list = $("#run-list");
  if (!state.entries.length) {
    list.innerHTML = `<div class="empty dim" style="padding:40px 16px">No runs match.</div>`;
  } else {
    list.innerHTML = state.entries.map(runItemHtml).join("");
  }
  $("#run-count").textContent = `${state.entries.length} / ${state.total}`;
  const more = state.entries.length < state.total;
  const foot = $(".list-foot");
  let moreBtn = $("#more-btn");
  if (more && !moreBtn) {
    moreBtn = document.createElement("button");
    moreBtn.id = "more-btn";
    moreBtn.className = "btn ghost";
    moreBtn.textContent = "Load more";
    moreBtn.onclick = () => {
      state.offset += state.limit;
      fetchRuns(false);
    };
    foot.insertBefore(moreBtn, foot.lastElementChild);
  } else if (!more && moreBtn) {
    moreBtn.remove();
  }
}

function runItemHtml(e) {
  const active = e.id === state.selectedId ? " active" : "";
  let meta = "";
  if (e.kind === "trace") {
    const m = e.metrics || {};
    meta = `${num(m.sessions)} sess · ${num(m.modelTurns)} turns · ${num(m.toolCalls)} tools`;
  } else {
    const m = e.metrics || {};
    const hr = m.hitRate != null ? ` · ${pct(m.hitRate)} hit` : "";
    meta = `${num(m.findings)} findings${hr}`;
  }
  return `<button class="run-item${active}" data-id="${esc(e.id)}">
    <div class="ri-title">${esc(e.title || e.name)}</div>
    <div class="ri-meta">
      <span class="kind-badge ${e.kind}">${e.kind}</span>
      <span>${meta}</span>
      <span style="margin-left:auto">${ago(e.mtimeMs)}</span>
    </div>
  </button>`;
}

async function selectRun(id) {
  state.selectedId = id;
  state.tab = "summary";
  state.events = null;
  renderSidebar();
  $("#main").innerHTML = `<div class="empty"><div class="kicker">loading trace…</div></div>`;
  try {
    state.detail = await api(`/api/run?id=${encodeURIComponent(id)}`);
    renderDetail();
  } catch (err) {
    $("#main").innerHTML = `<div class="empty"><p class="dim">Failed to load run.<br>${esc(err.message)}</p></div>`;
  }
}

// ---- Detail view ----------------------------------------------------------

function detailTabs() {
  const d = state.detail;
  const tabs = [];
  const bench = (d.result && d.result.benchmark) || {};
  const findings = (d.result && d.result.findings) || [];
  tabs.push(["summary", "Summary", null]);
  if (d.result) tabs.push(["findings", "Findings", findings.length]);
  if (d.kind === "trace") {
    const sessions = (d.traceReport && d.traceReport.sessions) || [];
    tabs.push(["trace", "Trace", sessions.length]);
    if (d.coverage) tabs.push(["coverage", "Coverage", null]);
    if (d.hasEvents) tabs.push(["events", "Raw events", d.coverage ? d.coverage.eventCount : null]);
  }
  tabs.push(["json", "JSON", null]);
  void bench;
  return tabs;
}

function renderDetail() {
  const d = state.detail;
  const result = d.result;
  const status = statusOf(d);
  const model = (result && result.inputs && result.inputs.model) || null;
  const mode = (result && (result.mode || (result.inputs && result.inputs.runMode))) || null;

  const tabs = detailTabs();
  if (!tabs.some((t) => t[0] === state.tab)) state.tab = "summary";

  const head = `
    <div class="page-head">
      <div class="head-title">
        <div class="kicker">${d.kind === "trace" ? "trace run" : "result"}</div>
        <h1>${esc(d.result ? titleFromRel(d.resultRel || d.rel) : titleFromRel(d.rel))}</h1>
        <div class="sub">${esc(d.rel)}</div>
      </div>
      <div class="head-meta">
        ${status.html}
        ${model ? `<span class="pill"><span class="mono">${esc(model)}</span></span>` : ""}
        ${mode ? `<span class="pill faint">${esc(mode)}</span>` : ""}
      </div>
    </div>
    <div class="tabs">
      ${tabs
        .map(
          ([id, label, count]) =>
            `<button class="tab${id === state.tab ? " on" : ""}" data-tab="${id}">${label}${
              count != null ? `<span class="count">${num(count)}</span>` : ""
            }</button>`,
        )
        .join("")}
    </div>
    <div id="tab-body"></div>`;
  $("#main").innerHTML = head;
  renderTabBody();
}

function renderTabBody() {
  const body = $("#tab-body");
  const d = state.detail;
  switch (state.tab) {
    case "summary":
      body.innerHTML = renderSummary(d);
      break;
    case "findings":
      body.innerHTML = renderFindings(d.result);
      break;
    case "trace":
      body.innerHTML = renderTrace(d.traceReport);
      break;
    case "coverage":
      body.innerHTML = renderCoverage(d.coverage, d.audit);
      break;
    case "events":
      renderEventsTab(body);
      break;
    case "json":
      body.innerHTML = `<div class="panel"><pre style="margin:0;max-height:70vh;overflow:auto;font-family:var(--font-mono);font-size:11px;white-space:pre-wrap;word-break:break-word">${esc(
        JSON.stringify(d.result || d.traceReport, null, 2),
      )}</pre></div>`;
      break;
  }
}

function statusOf(d) {
  const r = d.result;
  if (r) {
    if (r.reviewValid === true) return { html: `<span class="pill good"><span class="dot"></span>valid</span>` };
    if (r.reviewValid === false) return { html: `<span class="pill bad"><span class="dot"></span>invalid</span>` };
  }
  if (d.kind === "trace") return { html: `<span class="pill run"><span class="dot"></span>trace</span>` };
  return { html: `<span class="pill">unknown</span>` };
}

function titleFromRel(rel) {
  if (!rel) return "run";
  const base = rel.split("/").pop();
  return base.replace(/\.json$/, "");
}

// ---- Summary --------------------------------------------------------------

function renderSummary(d) {
  const r = d.result;
  const cov = d.coverage || (r && r.audit && r.audit.eventCoverage) || {};
  const bench = (r && r.benchmark) || {};
  const review = (r && r.review) || {};

  const cards = [];
  if (bench.hitRate != null) cards.push(metric("hit rate", pct(bench.hitRate), bench.hitRate > 0 ? "good" : "bad"));
  if (Array.isArray(bench.hits)) cards.push(metric("golden hits", bench.hits.length, "good"));
  if (Array.isArray(bench.misses)) cards.push(metric("golden misses", bench.misses.length, bench.misses.length ? "bad" : "good"));
  if (bench.falsePositiveCount != null)
    cards.push(metric("false positives", bench.falsePositiveCount, bench.falsePositiveCount ? "bad" : "good"));
  if (r && Array.isArray(r.findings)) cards.push(metric("findings", r.findings.length));
  if (cov.modelTurns != null || review.modelCalls != null) cards.push(metric("model turns", cov.modelTurns ?? review.modelCalls));
  if (cov.toolCallsCompleted != null || review.toolCalls != null)
    cards.push(metric("tool calls", cov.toolCallsCompleted ?? review.toolCalls));
  if (cov.sessions != null || review.sessions != null) cards.push(metric("sessions", cov.sessions ?? review.sessions));
  if (cov.eventCount != null) cards.push(metric("events", cov.eventCount));
  if (cov.peakRssBytes != null) cards.push(metric("peak RSS", `${(cov.peakRssBytes / 1048576).toFixed(1)} MiB`));
  if (review.elapsedMs != null || bench.elapsedMs != null)
    cards.push(metric("elapsed", `${(((review.elapsedMs ?? bench.elapsedMs) || 0) / 1000).toFixed(1)}s`));
  if (review.tokens != null && typeof review.tokens === "object") {
    const totalTokens = review.tokens.totalTokens ?? review.tokens.total;
    if (totalTokens != null) cards.push(metric("tokens", totalTokens));
  }

  let html = `<div class="panel"><div class="kicker">metrics</div><div class="scan-line"></div>
    <div class="metrics">${cards.join("") || `<span class="dim">No metrics.</span>`}</div></div>`;

  // Inputs
  if (r && r.inputs) {
    html += `<div class="panel"><h2>Inputs</h2><dl class="kv">${kvRows(r.inputs)}</dl></div>`;
  }

  // Golden coverage
  if (Array.isArray(bench.hits) || Array.isArray(bench.misses)) {
    html += `<div class="grid2">
      <div class="panel"><h2>Golden hits <span class="dim mono">${(bench.hits || []).length}</span></h2>${goldenList(
      bench.hits,
      "hit",
    )}</div>
      <div class="panel"><h2>Golden misses <span class="dim mono">${(bench.misses || []).length}</span></h2>${goldenList(
      bench.misses,
      "miss",
    )}</div>
    </div>`;
  }

  if (r && Array.isArray(r.validationFailures) && r.validationFailures.length) {
    html += `<div class="panel"><h2>Validation failures</h2><pre style="white-space:pre-wrap;font-family:var(--font-mono);font-size:11px;color:var(--bad)">${esc(
      JSON.stringify(r.validationFailures, null, 2),
    )}</pre></div>`;
  }
  return html;
}

function metric(label, value, cls) {
  return `<div class="metric"><span class="label">${esc(label)}</span><span class="value ${cls || "muted"}">${num(
    value,
  )}</span></div>`;
}

function kvRows(obj) {
  return Object.entries(obj)
    .map(([k, v]) => `<dt>${esc(k)}</dt><dd class="mono">${esc(typeof v === "object" ? JSON.stringify(v) : v)}</dd>`)
    .join("");
}

function goldenList(items, kind) {
  if (!items || !items.length) return `<div class="dim" style="padding:6px 0">none</div>`;
  return items
    .map(
      (g) => `<div class="finding ${kind}" style="margin-bottom:8px;padding:10px 12px">
      <div class="f-title" style="font-size:13px">${esc(g.title || g.issueId || g.id || "issue")}</div>
      ${g.path ? `<div class="f-loc">${esc(g.path)}</div>` : ""}
    </div>`,
    )
    .join("");
}

// ---- Findings -------------------------------------------------------------

function renderFindings(result) {
  const findings = (result && result.findings) || [];
  if (!findings.length) return `<div class="panel"><div class="dim">No findings recorded.</div></div>`;
  return `<div class="panel">${findings.map(findingHtml).join("")}</div>`;
}

function findingHtml(f) {
  const cls =
    f.challengeStatus === "refuted"
      ? "refuted"
      : f.publishable === false
      ? "miss"
      : f.challengeStatus === "confirmed" || f.publishable === true
      ? "hit"
      : "";
  const loc = f.location
    ? typeof f.location === "string"
      ? f.location
      : `${f.location.path || ""}${f.location.startLine ? `:${f.location.startLine}` : ""}`
    : (f.relatedPaths || [])[0] || "";
  const tags = [];
  if (f.severity) tags.push(["", f.severity]);
  if (f.confidence != null) tags.push(["", `conf ${f.confidence}`]);
  if (f.challengeStatus) tags.push([f.challengeStatus === "refuted" ? "bad" : "good", f.challengeStatus]);
  if (f.validationStatus) tags.push(["", `val:${f.validationStatus}`]);
  if (f.publishable != null) tags.push([f.publishable ? "good" : "bad", f.publishable ? "publishable" : "not published"]);
  if (f.evidenceCount != null) tags.push(["", `${f.evidenceCount} evidence`]);
  if (Array.isArray(f.discoveredBy) && f.discoveredBy.length) tags.push(["", `by ${f.discoveredBy.join(",")}`]);

  return `<div class="finding ${cls}">
    <div class="f-head">
      <div style="min-width:0">
        <div class="f-title">${esc(f.title || "(untitled finding)")}</div>
        ${loc ? `<div class="f-loc">${esc(loc)}</div>` : ""}
      </div>
    </div>
    ${f.claim ? `<div class="f-claim">${esc(f.claim)}</div>` : ""}
    <div class="tags">${tags.map(([c, t]) => `<span class="tag ${c}">${esc(t)}</span>`).join("")}</div>
    ${
      Array.isArray(f.evidence) && f.evidence.length
        ? `<details style="margin-top:10px"><summary class="mono dim" style="cursor:pointer;font-size:11px">${f.evidence.length} evidence record(s)</summary><pre style="margin-top:8px;font-family:var(--font-mono);font-size:10.5px;white-space:pre-wrap;word-break:break-word;max-height:300px;overflow:auto;background:var(--void-900);border:1px solid var(--line-soft);border-radius:6px;padding:10px">${esc(
            JSON.stringify(f.evidence, null, 2),
          )}</pre></details>`
        : ""
    }
  </div>`;
}

// ---- Trace timeline -------------------------------------------------------

const KIND_COLOR = {
  model_turn_prepared: "var(--hive-300)",
  "model_turn_completed.text": "var(--hive-200)",
  "model_turn_completed.tool_calls": "var(--hive-400)",
  tool_calls_requested: "var(--hive-500)",
  tool_batch_planned: "var(--hive-600)",
  candidate_finding_decision: "var(--good)",
  candidate_synthesis_summary: "var(--warn)",
  resource_sample: "var(--text-faint)",
  unit_exploration_trace: "var(--text-dim)",
  unit_result_parsed: "var(--hive-200)",
  explore_worker_started: "var(--hive-300)",
  explore_worker_completed: "var(--hive-200)",
  explore_worker_planned: "var(--hive-500)",
  explore_worker_merged: "var(--good)",
};

function renderTrace(report) {
  const sessions = (report && report.sessions) || [];
  if (!sessions.length) return `<div class="panel"><div class="dim">No session trace.</div></div>`;
  const totals = report || {};
  const head = `<div class="panel"><div class="kicker">trace totals</div><div class="scan-line"></div>
    <div class="metrics">
      ${metric("sessions", sessions.length)}
      ${metric("model turns", totals.modelTurns)}
      ${metric("tools requested", totals.toolCallsRequested)}
      ${metric("tools completed", totals.toolCallsCompleted)}
      ${metric("tools denied", totals.toolCallsDenied, totals.toolCallsDenied ? "bad" : "muted")}
      ${metric("findings", totals.findingsRecorded)}
    </div></div>`;
  return head + sessions.map(sessionHtml).join("");
}

function sessionHtml(s) {
  const turns = s.turns || [];
  const turnEntries = turns.reduce((a, t) => a + (t.entries ? t.entries.length : 0), 0);
  const sessionEvents = s.sessionEvents || [];
  const body =
    turns.map(turnHtml).join("") +
    (sessionEvents.length
      ? `<div class="turn"><div class="turn-label">session events</div>${sessionEvents
          .map((e) => entryHtml(e))
          .join("")}</div>`
      : "");
  return `<details class="session">
    <summary>
      <span class="chev">▸</span>
      <span class="sname">${esc(s.sessionId || "session")}</span>
      <span class="sstats">${turns.length} turns · ${turnEntries} entries</span>
    </summary>
    ${body || `<div class="turn dim">no turn entries</div>`}
  </details>`;
}

function turnHtml(t) {
  return `<div class="turn">
    <div class="turn-label">turn ${num(t.turn)}</div>
    ${(t.entries || []).map((e) => entryHtml(e)).join("")}
  </div>`;
}

function entryHtml(e) {
  const kind = e.kind || e.traceKind || e.eventKind || "event";
  const color = KIND_COLOR[kind] || "var(--hive-500)";
  const details = e.details;
  let detailHtml = "";
  if (details != null) {
    const json = JSON.stringify(details, null, 2);
    if (json && json !== "{}" && json !== "null") {
      detailHtml = `<details><summary>details</summary><pre>${esc(json)}</pre></details>`;
    }
  }
  return `<div class="entry" style="--kind-color:${color}">
    <div class="e-head">
      <span class="e-kind">${esc(kind)}</span>
      <span class="e-summary">${esc(e.summary || "")}</span>
      <span class="e-seq">#${num(e.seq)}</span>
    </div>
    ${detailHtml}
  </div>`;
}

// ---- Coverage -------------------------------------------------------------

function renderCoverage(cov, audit) {
  if (!cov) return `<div class="panel"><div class="dim">No coverage data.</div></div>`;
  const flags = Object.entries(cov).filter(([k, v]) => k.startsWith("has") && typeof v === "boolean");
  const counters = Object.entries(cov).filter(
    ([k, v]) => typeof v === "number" && !k.startsWith("has"),
  );
  let html = `<div class="panel"><h2>Event coverage</h2>
    <div class="cov-grid">${flags
      .map(
        ([k, v]) =>
          `<div class="cov ${v ? "on" : "off"}"><span class="led"></span>${esc(
            k.replace(/^has/, "").replace(/([A-Z])/g, " $1").trim(),
          )}</div>`,
      )
      .join("")}</div></div>`;

  html += `<div class="panel"><h2>Counters</h2><div class="metrics">${counters
    .map(([k, v]) => metric(k.replace(/([A-Z])/g, " $1").trim(), v))
    .join("")}</div></div>`;

  if (Array.isArray(cov.traceKinds) && cov.traceKinds.length) {
    html += `<div class="panel"><h2>Trace kinds</h2><div class="tags">${cov.traceKinds
      .map((k) => `<span class="tag">${esc(k)}</span>`)
      .join("")}</div></div>`;
  }

  if (audit) {
    html += `<div class="panel"><h2>Audit diagnostics</h2><pre style="white-space:pre-wrap;font-family:var(--font-mono);font-size:11px;max-height:360px;overflow:auto">${esc(
      JSON.stringify(audit, null, 2),
    )}</pre></div>`;
  }
  return html;
}

// ---- Raw events -----------------------------------------------------------

async function renderEventsTab(body) {
  const d = state.detail;
  const q = (state.events && state.events.q) || "";
  body.innerHTML = `<div class="panel">
    <div class="row spread" style="margin-bottom:12px">
      <input class="input" id="ev-search" style="max-width:320px" placeholder="filter events…" value="${esc(q)}" />
      <span class="dim mono" id="ev-count">loading…</span>
    </div>
    <div id="ev-rows"><div class="dim">loading events…</div></div>
    <div class="row" style="justify-content:center;margin-top:14px"><button class="btn ghost" id="ev-more">Load more</button></div>
  </div>`;

  const load = async (append) => {
    const offset = append && state.events ? state.events.offset + state.events.limit : 0;
    const params = new URLSearchParams({ trace: d.rel, offset, limit: 300, q });
    const data = await api(`/api/events?${params}`);
    if (append && state.events) data.events = state.events.events.concat(data.events);
    data.q = q;
    state.events = data;
    $("#ev-count").textContent = `${data.events.length} / ${data.total}`;
    $("#ev-rows").innerHTML = data.events.map(eventRowHtml).join("") || `<div class="dim">no events</div>`;
    $("#ev-more").style.display = data.events.length < data.total ? "" : "none";
  };

  $("#ev-more").onclick = () => load(true);
  let t;
  $("#ev-search").oninput = (ev) => {
    clearTimeout(t);
    const val = ev.target.value;
    t = setTimeout(() => {
      state.events = { q: val, offset: 0, limit: 300, events: [], total: 0 };
      load(false);
    }, 250);
  };
  await load(false);
}

function eventRowHtml(e) {
  const method = (e.method || "").replace("event.", "");
  return `<div class="event-row">
    <span class="ev-seq">#${num(e.seq)}</span>
    <span class="ev-method">${esc(method)}</span>
    <span class="ev-kind">${esc(e.eventKind || "")}</span>
    <span class="ev-sum" title="${esc(e.summary || e.traceKind || "")}">${esc(
    e.summary || e.traceKind || (e.sessionId ? `session ${e.sessionId}` : ""),
  )}</span>
  </div>`;
}

// ---- Launcher -------------------------------------------------------------

async function openLaunch() {
  if (!state.suites) {
    try {
      state.suites = await api("/api/suites");
    } catch (err) {
      state.suites = { presets: [], fixtures: {}, env: {} };
    }
  }
  const s = state.suites;
  const root = $("#modal-root");
  const presetOpts = s.presets
    .map((p) => `<option value="${esc(p.id)}">${esc(p.label)}</option>`)
    .join("");
  const codex = s.codex || { available: false, accounts: [] };
  const codexOpts = ['<option value="">Already-running proxy (:4141)</option>']
    .concat(
      (codex.accounts || []).map((a) => {
        const label = (a.email || a.id) + (a.workspaceLabel ? ` · ${a.workspaceLabel}` : "");
        return `<option value="${esc(a.id || a.email)}">${esc(label)}</option>`;
      }),
    )
    .join("");

  root.innerHTML = `<div class="overlay" id="overlay">
    <div class="dialog" id="dialog">
      <div class="kicker">launch eval</div>
      <h2>New run</h2>
      <div id="launch-form">
        <label class="field"><span class="label">Preset</span>
          <select class="select" id="f-preset">${presetOpts}</select>
        </label>
        <div id="f-preset-desc" class="hint"></div>
        <label class="field" id="f-fixture-wrap"><span class="label">Fixture</span>
          <select class="select" id="f-fixture"></select>
        </label>
        <div class="grid2" id="f-model-wrap">
          <label class="field" style="margin:0"><span class="label">Model</span>
            <input class="input" id="f-model" value="${esc(s.env.modelEnv || "gpt-5.4-mini")}" />
          </label>
          <label class="field" style="margin:0"><span class="label">Runner</span>
            <select class="select" id="f-runner">${(s.env.runners || [])
              .map((r) => `<option ${r === s.env.defaultRunner ? "selected" : ""}>${esc(r)}</option>`)
              .join("")}</select>
          </label>
        </div>
        <label class="check field" id="f-proxy-wrap">
          <input type="checkbox" id="f-proxy" ${s.env.hasOpenAiBaseUrl ? "" : "checked"} />
          <span>Route model calls through local Codex proxy</span>
        </label>
        <label class="field" id="f-codex-acct-wrap"><span class="label">Codex account</span>
          <select class="select" id="f-codex-acct">${codexOpts}</select>
          <span class="faint" id="f-codex-note" style="font-size:11px;margin-top:6px;display:block"></span>
        </label>
        <div id="f-warn"></div>
      </div>
      <div class="dialog-foot">
        <button class="btn ghost" id="f-cancel">Cancel</button>
        <button class="btn primary" id="f-launch">Launch ▸</button>
      </div>
    </div>
  </div>`;

  const presetSel = $("#f-preset");
  const fixtureSel = $("#f-fixture");
  const syncPreset = () => {
    const p = s.presets.find((x) => x.id === presetSel.value);
    $("#f-preset-desc").textContent = p ? p.description : "";
    const showFixture = !!p && !!p.fixtureSet;
    $("#f-fixture-wrap").style.display = showFixture ? "" : "none";
    $("#f-model-wrap").style.display = p && p.needsModel ? "" : "none";
    $("#f-proxy-wrap").style.display = p && p.needsModel ? "" : "none";
    const proxyOn = $("#f-proxy").checked;
    const showCodexAcct = p && p.needsModel && proxyOn;
    $("#f-codex-acct-wrap").style.display = showCodexAcct ? "" : "none";
    if (showCodexAcct) {
      const acctVal = $("#f-codex-acct").value;
      $("#f-codex-note").textContent = acctVal
        ? "muzen starts (and reuses) a managed proxy bound to this account."
        : codex.available
        ? "Uses a proxy you started yourself on :4141. Pick an account above to let muzen manage it."
        : "No CodexBar accounts found — uses a proxy you started yourself on :4141.";
    }
    if (showFixture) {
      const list = (s.fixtures && s.fixtures[p.fixtureSet]) || [];
      fixtureSel.innerHTML = list.map((f) => `<option>${esc(f)}</option>`).join("");
    }
    const warn = [];
    if (p && p.needsRunner && (!s.env.runners || !s.env.runners.length)) {
      warn.push(`Runner not built. Run <span class="mono">cargo build --release --bin muzen-runner</span> first.`);
    }
    if (p && p.needsModel && !s.env.hasOpenAiKey && !proxyOn) {
      warn.push(`No <span class="mono">OPENAI_API_KEY</span> in the server env — model calls may fail without the proxy.`);
    }
    $("#f-warn").innerHTML = warn.map((w) => `<div class="hint bad">${w}</div>`).join("");
    const blocked = p && p.needsRunner && (!s.env.runners || !s.env.runners.length);
    $("#f-launch").disabled = !!blocked;
  };
  presetSel.onchange = syncPreset;
  $("#f-proxy").onchange = syncPreset;
  $("#f-codex-acct").onchange = syncPreset;
  syncPreset();

  $("#f-cancel").onclick = closeModal;
  $("#overlay").onclick = (e) => {
    if (e.target.id === "overlay") closeModal();
  };
  $("#f-launch").onclick = doLaunch;
}

function closeModal() {
  $("#modal-root").innerHTML = "";
}

async function doLaunch() {
  const s = state.suites;
  const presetId = $("#f-preset").value;
  const p = s.presets.find((x) => x.id === presetId);
  const payload = { presetId };
  if (p.fixtureSet) payload.fixture = $("#f-fixture").value;
  if (p.needsModel) {
    payload.model = $("#f-model").value.trim();
    payload.useProxy = $("#f-proxy").checked;
    if (payload.useProxy) {
      const acct = $("#f-codex-acct") ? $("#f-codex-acct").value : "";
      if (acct) payload.codexAccount = acct;
    }
  }
  if (p.needsRunner) payload.runnerPath = $("#f-runner").value;

  const launchBtn = $("#f-launch");
  launchBtn.disabled = true;
  launchBtn.textContent = payload.codexAccount ? "Starting proxy…" : "Launching…";

  let resp;
  try {
    const r = await fetch("/api/launch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(payload),
    });
    resp = await r.json();
    if (!r.ok) throw new Error(resp.error || "launch failed");
  } catch (err) {
    $("#f-warn").innerHTML = `<div class="hint bad">${esc(err.message)}</div>`;
    launchBtn.disabled = false;
    launchBtn.textContent = "Launch ▸";
    return;
  }
  closeModal();
  streamRun(resp.jobId, resp.command, p.label);
}

function streamRun(jobId, command, label) {
  $("#run-list").querySelectorAll(".run-item.active").forEach((el) => el.classList.remove("active"));
  state.selectedId = null;
  $("#main").innerHTML = `
    <div class="page-head">
      <div style="flex:1">
        <div class="kicker">live run</div>
        <h1>${esc(label)}</h1>
        <div class="sub">${esc(command)}</div>
      </div>
      <span class="pill run" id="live-status"><span class="dot"></span>running</span>
    </div>
    <div class="panel"><div class="console" id="console"></div></div>
    <div id="live-actions"></div>`;

  const consoleEl = $("#console");
  const es = new EventSource(`/api/launch/stream?jobId=${encodeURIComponent(jobId)}`);
  es.onmessage = (msg) => {
    const data = JSON.parse(msg.data);
    if (data.type === "line") {
      const div = document.createElement("div");
      div.className = `log-line ${data.line.stream}`;
      div.textContent = data.line.text;
      consoleEl.appendChild(div);
      consoleEl.scrollTop = consoleEl.scrollHeight;
    } else if (data.type === "done") {
      es.close();
      const ok = data.status === "completed";
      $("#live-status").outerHTML = `<span class="pill ${ok ? "good" : "bad"}"><span class="dot"></span>${esc(
        data.status,
      )} (exit ${num(data.code)})</span>`;
      if (data.traceId) {
        $("#live-actions").innerHTML = `<div class="panel row spread"><span class="dim">Trace artifacts written.</span>
          <button class="btn primary" id="open-trace">Open trace ▸</button></div>`;
        $("#open-trace").onclick = () => {
          fetchRuns(true).then(() => selectRun(data.traceId));
        };
      } else {
        $("#live-actions").innerHTML = `<div class="panel dim">No trace artifacts (model-free gate or failed run).</div>`;
      }
      fetchRuns(true);
    }
  };
  es.onerror = () => {
    es.close();
    const st = $("#live-status");
    if (st) st.outerHTML = `<span class="pill bad"><span class="dot"></span>stream lost</span>`;
  };
}

// ---- Env flags ------------------------------------------------------------

async function renderEnvFlags() {
  try {
    const s = await api("/api/suites");
    state.suites = s;
    const flags = [];
    const ok = (s.env.runners || []).length > 0;
    flags.push(`<span class="pill ${ok ? "good" : "bad"}"><span class="dot"></span>runner</span>`);
    if (s.env.modelEnv) flags.push(`<span class="pill"><span class="mono">${esc(s.env.modelEnv)}</span></span>`);
    flags.push(
      `<span class="pill ${s.env.hasOpenAiKey || s.env.hasOpenAiBaseUrl ? "" : "warn"}">${
        s.env.hasOpenAiBaseUrl ? "proxy" : s.env.hasOpenAiKey ? "api key" : "no model auth"
      }</span>`,
    );
    $("#env-flags").innerHTML = flags.join("");
  } catch {
    /* ignore */
  }
}

// ---- Wiring ---------------------------------------------------------------

function init() {
  $("#run-list").addEventListener("click", (e) => {
    const item = e.target.closest(".run-item");
    if (item) selectRun(item.dataset.id);
  });
  $("#main").addEventListener("click", (e) => {
    const tab = e.target.closest(".tab");
    if (tab) {
      state.tab = tab.dataset.tab;
      renderDetail();
    }
  });
  $("#kind-seg").addEventListener("click", (e) => {
    const btn = e.target.closest("button[data-kind]");
    if (!btn) return;
    $$("#kind-seg button").forEach((b) => b.classList.toggle("on", b === btn));
    state.kind = btn.dataset.kind;
    fetchRuns(true);
  });
  let st;
  $("#search").addEventListener("input", (e) => {
    clearTimeout(st);
    st = setTimeout(() => {
      state.q = e.target.value.trim();
      fetchRuns(true);
    }, 250);
  });
  $("#refresh-btn").addEventListener("click", async () => {
    await api("/api/runs?refresh=1&limit=1");
    fetchRuns(true);
  });
  $("#launch-btn").addEventListener("click", openLaunch);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeModal();
  });

  renderEnvFlags();
  fetchRuns(true);
}

init();
