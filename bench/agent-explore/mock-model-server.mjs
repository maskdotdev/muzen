#!/usr/bin/env node
import { createServer } from "node:http";

function parseArgs(argv) {
  const options = { latencyMs: 300, readFiles: 5, port: 0 };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (flag === "--latency-ms") options.latencyMs = Number(value), index += 1;
    else if (flag === "--read-files") options.readFiles = Number(value), index += 1;
    else if (flag === "--port") options.port = Number(value), index += 1;
    else throw new Error(`unknown argument: ${flag}`);
  }
  for (const [name, value] of Object.entries(options)) {
    if (!Number.isInteger(value) || value < 0 || (name === "readFiles" && value < 1)) {
      throw new Error(`--${name.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)} must be a non-negative integer`);
    }
  }
  return options;
}

function decode(value) {
  let current = value;
  for (let depth = 0; depth < 4 && typeof current === "string"; depth += 1) {
    try { current = JSON.parse(current); } catch { break; }
  }
  return current;
}

function conversation(body) {
  const calls = new Map();
  const results = [];
  for (const message of Array.isArray(body.messages) ? body.messages : []) {
    for (const call of Array.isArray(message.tool_calls) ? message.tool_calls : []) {
      calls.set(call.id, {
        id: call.id,
        name: call.function?.name,
        arguments: decode(call.function?.arguments ?? "{}"),
      });
    }
    if (message.role === "tool") {
      const call = calls.get(message.tool_call_id);
      if (call !== undefined) results.push({ ...call, result: decode(message.content) });
    }
  }
  return results;
}

function filesFromListing(result) {
  const value = decode(result);
  if (Array.isArray(value?.files)) return value.files;
  if (value && typeof value === "object") {
    for (const child of Object.values(value)) {
      const found = filesFromListing(child);
      if (found.length > 0) return found;
    }
  }
  return [];
}

function toolCall(id, name, arguments_) {
  return {
    id,
    type: "function",
    function: { name, arguments: JSON.stringify(arguments_) },
  };
}

function responseFor(body, readFiles) {
  const results = conversation(body);
  const failed = results.find((item) => decode(item.result)?.error !== undefined);
  if (failed !== undefined) throw new Error(`${failed.name} failed: ${decode(failed.result).error}`);
  const listing = results.find((item) => item.name === "fs_list");
  if (listing === undefined) return toolCall("list-1", "fs_list", { path: "src" });

  const files = filesFromListing(listing.result)
    .filter((entry) => typeof entry?.path === "string")
    .slice(0, readFiles);
  if (files.length !== readFiles) {
    throw new Error(`fs_list returned ${files.length} usable files; expected ${readFiles}`);
  }
  const reads = results.filter((item) => item.name === "fs_read");
  if (reads.length < readFiles) {
    return toolCall(`read-${reads.length + 1}`, "fs_read", { path: files[reads.length].path });
  }
  const grep = results.find((item) => item.name === "fs_grep");
  if (grep === undefined) return toolCall("grep-1", "fs_grep", { pattern: "fn ", path: "src" });

  const bytes = reads.reduce((sum, item) => {
    const value = decode(item.result);
    return sum + (Number.isFinite(value?.bytes) ? Number(value.bytes) : 0);
  }, 0);
  const grepValue = decode(grep.result);
  const matches = Number.isFinite(grepValue?.totalMatches) ? Number(grepValue.totalMatches) : 0;
  return `Explored ${reads.length} files and ${bytes} bytes; grep found ${matches} matches.`;
}

const options = parseArgs(process.argv.slice(2));
const server = createServer(async (request, response) => {
  if (request.method !== "POST" || !["/v1/chat/completions", "/chat/completions"].includes(request.url)) {
    response.writeHead(404).end();
    return;
  }
  try {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    await new Promise((resolve) => setTimeout(resolve, options.latencyMs));
    const action = responseFor(body, options.readFiles);
    const message = typeof action === "string"
      ? { role: "assistant", content: action }
      : { role: "assistant", content: null, tool_calls: [action] };
    const payload = JSON.stringify({
      id: "chatcmpl-muzen-agent-explore",
      object: "chat.completion",
      choices: [{ index: 0, message, finish_reason: typeof action === "string" ? "stop" : "tool_calls" }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    });
    response.writeHead(200, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(payload) });
    response.end(payload);
  } catch (error) {
    const payload = JSON.stringify({ error: { message: error instanceof Error ? error.message : String(error) } });
    response.writeHead(500, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(payload) });
    response.end(payload);
  }
});

server.listen(options.port, "127.0.0.1", () => {
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("model server did not bind a TCP port");
  console.log(`LISTENING ${address.port}`);
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
