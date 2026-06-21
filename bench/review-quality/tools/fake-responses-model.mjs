#!/usr/bin/env node

import http from "node:http";
import fs from "node:fs";
import { pathToFileURL } from "node:url";

const DEFAULT_CONFIG = {
  port: 0,
  host: "127.0.0.1",
  latencyMs: 0,
  jitterMs: 0,
  maxConcurrent: 64,
  toolsBeforeFinal: Number.POSITIVE_INFINITY,
  invalidFinalAttempts: 0,
  httpErrorEvery: 0,
  toolName: "diff",
  logPath: null,
};

export async function startFakeResponsesServer(config = {}) {
  const state = {
    ...DEFAULT_CONFIG,
    ...config,
    active: 0,
    sequence: 0,
    invalidFinalsUsed: 0,
    queue: [],
  };
  const server = http.createServer((req, res) => {
    void handleQueuedRequest(state, req, res);
  });
  await new Promise((resolve) => server.listen(state.port, state.host, resolve));
  const address = server.address();
  return {
    baseUrl: `http://${address.address}:${address.port}/v1`,
    port: address.port,
    reset() {
      state.sequence = 0;
      state.invalidFinalsUsed = 0;
    },
    close() {
      return new Promise((resolve, reject) => {
        server.close((error) => (error ? reject(error) : resolve()));
      });
    },
  };
}

async function handleQueuedRequest(state, req, res) {
  if (state.active >= state.maxConcurrent) {
    await new Promise((resolve) => state.queue.push(resolve));
  }
  state.active += 1;
  try {
    await handleRequest(state, req, res);
  } finally {
    state.active -= 1;
    state.queue.shift()?.();
  }
}

async function handleRequest(state, req, res) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const rawBody = Buffer.concat(chunks).toString("utf8");
  const body = safeParse(rawBody) || {};
  const sequence = ++state.sequence;
  const startedAt = Date.now();
  const delay = state.latencyMs + deterministicJitter(sequence, state.jitterMs);
  if (delay > 0) await sleep(delay);

  if (req.method !== "POST" || !req.url.endsWith("/responses")) {
    writeJson(res, 404, { error: { message: "fake model only implements POST /responses" } });
    return;
  }
  if (state.httpErrorEvery > 0 && sequence % state.httpErrorEvery === 0) {
    writeLog(state, sequence, body, { status: 500, decision: "configured_http_error" }, startedAt);
    writeJson(res, 500, { error: { message: "configured fake model error" } });
    return;
  }

  const decision = decideResponse(state, body, sequence);
  writeLog(state, sequence, body, decision, startedAt);
  writeJson(res, 200, responseEnvelope(decision));
}

function decideResponse(state, body, sequence) {
  const toolsExposed = Array.isArray(body.tools) && body.tools.length > 0;
  const functionOutputs = Array.isArray(body.input)
    ? body.input.filter((item) => item?.type === "function_call_output").length
    : 0;
  const responseFormatName = body.text?.format?.name || "";
  const finalLike =
    !toolsExposed ||
    Boolean(responseFormatName) ||
    functionOutputs >= state.toolsBeforeFinal;
  if (toolsExposed && functionOutputs < state.toolsBeforeFinal) {
    return {
      status: 200,
      decision: "tool_call",
      output: [
        {
          type: "function_call",
          call_id: `fake_call_${sequence}`,
          name: state.toolName,
          arguments: toolArguments(state.toolName),
        },
      ],
    };
  }
  if (finalLike && state.invalidFinalsUsed < state.invalidFinalAttempts) {
    state.invalidFinalsUsed += 1;
    return {
      status: 200,
      decision: "invalid_final_text",
      output: [textMessage("this is intentionally not json")],
      output_text: "this is intentionally not json",
    };
  }
  const content = finalJson(responseFormatName);
  return {
    status: 200,
    decision: "valid_final_text",
    output: [textMessage(content)],
    output_text: content,
  };
}

function responseEnvelope(decision) {
  return {
    id: `resp_${Date.now()}`,
    object: "response",
    created_at: Math.floor(Date.now() / 1000),
    model: "fake-responses-model",
    output: decision.output,
    output_text: decision.output_text,
    usage: {
      input_tokens: 11,
      output_tokens: 7,
      total_tokens: 18,
    },
  };
}

function finalJson(responseFormatName) {
  if (String(responseFormatName).includes("packet")) {
    return JSON.stringify({
      status: "insufficient",
      summary: "Synthetic delegate packet.",
      checkedPaths: [],
      evidence: [],
      openQuestions: [],
      suggestedNextSearches: [],
      candidateFindings: [],
    });
  }
  return JSON.stringify({
    verdict: "clean",
    summary: "Synthetic deterministic review completed.",
    candidates: [],
    notes: ["fake model"],
    completeness: {
      reviewedChangedFiles: ["src/example.txt"],
      reviewedRiskEntries: [],
      unreviewedRiskEntries: [],
      unresolvedQuestions: [],
      incompleteReasons: [],
      ignoredChildCandidates: [],
    },
  });
}

function textMessage(text) {
  return {
    type: "message",
    role: "assistant",
    content: [{ type: "output_text", text }],
  };
}

function toolArguments(toolName) {
  switch (toolName) {
    case "diff":
    case "glob":
    case "list_changed_files":
      return "{}";
    case "grep":
      return JSON.stringify({ query: "example|value|TODO" });
    case "read":
    case "read_base_file":
    case "read_head_file":
    case "imports":
    case "tests":
    case "find_related_files":
      return JSON.stringify({ path: "src/example.txt" });
    case "read_range":
      return JSON.stringify({ path: "src/example.txt", start_line: 1, end_line: 20 });
    default:
      return "{}";
  }
}

function writeJson(res, status, value) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(`${JSON.stringify(value)}\n`);
}

function writeLog(state, sequence, body, decision, startedAt) {
  if (!state.logPath) return;
  fs.appendFileSync(
    state.logPath,
    `${JSON.stringify({
      atUtc: new Date().toISOString(),
      sequence,
      elapsedMs: Date.now() - startedAt,
      decision: decision.decision,
      status: decision.status,
      toolsExposed: Array.isArray(body.tools) ? body.tools.length : 0,
      functionOutputs: Array.isArray(body.input)
        ? body.input.filter((item) => item?.type === "function_call_output").length
        : 0,
      responseFormat: body.text?.format?.name || null,
    })}\n`,
  );
}

function deterministicJitter(sequence, max) {
  if (!max || max <= 0) return 0;
  return (sequence * 1103515245 + 12345) % (max + 1);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function safeParse(text) {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function parseArgs(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      args.help = true;
      continue;
    }
    if (!arg.startsWith("--")) throw new Error(`unexpected argument ${arg}`);
    const key = arg.slice(2).replace(/-([a-z])/g, (_, char) => char.toUpperCase());
    const value = argv[++index];
    if (value == null || value.startsWith("--")) throw new Error(`missing value for ${arg}`);
    args[key] = value;
  }
  return args;
}

function numberArg(value, fallback) {
  if (value == null) return fallback;
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) throw new Error(`invalid number: ${value}`);
  return parsed;
}

function configFromArgs(args) {
  return {
    port: numberArg(args.port, DEFAULT_CONFIG.port),
    latencyMs: numberArg(args.latencyMs, DEFAULT_CONFIG.latencyMs),
    jitterMs: numberArg(args.jitterMs, DEFAULT_CONFIG.jitterMs),
    maxConcurrent: numberArg(args.maxConcurrent, DEFAULT_CONFIG.maxConcurrent),
    toolsBeforeFinal:
      args.toolsBeforeFinal === "infinite"
        ? Number.POSITIVE_INFINITY
        : numberArg(args.toolsBeforeFinal, DEFAULT_CONFIG.toolsBeforeFinal),
    invalidFinalAttempts: numberArg(args.invalidFinalAttempts, DEFAULT_CONFIG.invalidFinalAttempts),
    httpErrorEvery: numberArg(args.httpErrorEvery, DEFAULT_CONFIG.httpErrorEvery),
    toolName: args.toolName || DEFAULT_CONFIG.toolName,
    logPath: args.log || null,
  };
}

function usage() {
  process.stderr.write(`Usage: fake-responses-model.mjs [--port 8787] [--latency-ms 25] [--max-concurrent 1] [--tools-before-final N|infinite] [--invalid-final-attempts N] [--tool-name diff|grep|read] [--log path]\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    usage();
    process.exit(0);
  }
  const server = await startFakeResponsesServer(configFromArgs(args));
  process.stdout.write(`${JSON.stringify({ baseUrl: server.baseUrl, port: server.port })}\n`);
}
