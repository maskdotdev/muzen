#!/usr/bin/env node
import { readFile, readdir, realpath, stat } from "node:fs/promises";
import { relative, resolve, sep } from "node:path";

import {
  Agent,
  tool,
} from "../../sdk/typescript/packages/muzen-sdk/dist/agent.js";

function parseArgs(argv) {
  const options = { readFiles: 5 };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (flag === "--transport") (options.transport = value), (index += 1);
    else if (flag === "--root") (options.root = value), (index += 1);
    else if (flag === "--base-url") (options.baseUrl = value), (index += 1);
    else if (flag === "--model-base-url")
      (options.modelBaseUrl = value), (index += 1);
    else if (flag === "--read-files")
      (options.readFiles = Number(value)), (index += 1);
    else throw new Error(`unknown argument: ${flag}`);
  }
  if (!["local_runner", "http"].includes(options.transport))
    throw new Error("--transport must be local_runner or http");
  if (!options.root || !options.modelBaseUrl)
    throw new Error("--root and --model-base-url are required");
  if (options.transport === "http" && !options.baseUrl)
    throw new Error("--base-url is required for http transport");
  if (!Number.isInteger(options.readFiles) || options.readFiles < 1)
    throw new Error("--read-files must be positive");
  return options;
}

function inside(root, candidate) {
  return candidate === root || candidate.startsWith(`${root}${sep}`);
}

async function jailed(root, requested) {
  const candidate = await realpath(resolve(root, requested));
  if (!inside(root, candidate))
    throw new Error(`path escapes --root: ${requested}`);
  return candidate;
}

async function filesUnder(root, requested) {
  const base = await jailed(root, requested);
  const found = [];
  async function visit(directory) {
    const entries = await readdir(directory, { withFileTypes: true });
    entries.sort((left, right) => left.name.localeCompare(right.name));
    for (const entry of entries) {
      const lexical = resolve(directory, entry.name);
      if (entry.isDirectory()) await visit(lexical);
      else if (entry.isFile()) {
        const candidate = await realpath(lexical);
        if (inside(root, candidate))
          found.push({
            path: relative(root, candidate).split(sep).join("/"),
            bytes: (await stat(candidate)).size,
          });
      }
    }
  }
  await visit(base);
  return found.sort((left, right) => left.path.localeCompare(right.path));
}

const options = parseArgs(process.argv.slice(2));
const root = await realpath(options.root);
const counts = { fs_list: 0, fs_read: 0, fs_grep: 0 };
const objectInput = (properties, required) => ({
  type: "object",
  properties,
  required,
  additionalProperties: false,
});

const fsList = tool({
  name: "fs_list",
  description: "Recursively list regular files below a repository path.",
  input: objectInput({ path: { type: "string" } }, ["path"]),
  execute: async ({ path }) => {
    counts.fs_list += 1;
    const files = await filesUnder(root, path);
    return { path, files, totalFiles: files.length };
  },
});
const fsRead = tool({
  name: "fs_read",
  description: "Read one UTF-8 repository file.",
  input: objectInput({ path: { type: "string" } }, ["path"]),
  execute: async ({ path }) => {
    counts.fs_read += 1;
    const data = await readFile(await jailed(root, path));
    return { path, bytes: data.length, content: data.toString("utf8") };
  },
});
const fsGrep = tool({
  name: "fs_grep",
  description: "Search repository files for a fixed text pattern.",
  input: objectInput(
    { pattern: { type: "string" }, path: { type: "string" } },
    ["pattern", "path"],
  ),
  execute: async ({ pattern, path }) => {
    counts.fs_grep += 1;
    const matches = [];
    let totalMatches = 0;
    for (const entry of await filesUnder(root, path)) {
      const lines = (
        await readFile(await jailed(root, entry.path), "utf8")
      ).split(/\r?\n/);
      lines.forEach((line, index) => {
        if (line.includes(pattern)) {
          totalMatches += 1;
          if (matches.length < 100)
            matches.push({
              path: entry.path,
              line: index + 1,
              text: line.slice(0, 500),
            });
        }
      });
    }
    return {
      pattern,
      path,
      matches,
      totalMatches,
      truncated: totalMatches > matches.length,
    };
  },
});

const agentOptions = {
  instructions:
    "Explore the repository with the provided filesystem tools, then summarize what you saw.",
  model: "openai:muzen-agent-explore",
  tools: [fsList, fsRead, fsGrep],
  transport: options.transport,
  apiKey: "bench-test-key",
  ...(options.transport === "local_runner"
    ? { baseUrl: options.modelBaseUrl }
    : { baseUrl: options.baseUrl, modelBaseUrl: options.modelBaseUrl }),
};

const started = performance.now();
const agent = new Agent(agentOptions);
let result;
try {
  result = (
    await agent.run("Explore src and report a concise summary.")
  ).raiseForStatus();
} finally {
  await agent.close();
}
const expected = { fs_list: 1, fs_read: options.readFiles, fs_grep: 1 };
if (JSON.stringify(counts) !== JSON.stringify(expected))
  throw new Error(
    `tool count mismatch: expected ${JSON.stringify(expected)}, got ${JSON.stringify(counts)}`,
  );
if (!result.text.trim()) throw new Error("agent returned an empty summary");
console.log(
  JSON.stringify({
    turns: Object.values(counts).reduce((sum, value) => sum + value, 1),
    toolCalls: Object.values(counts).reduce((sum, value) => sum + value, 0),
    durationMs: Number((performance.now() - started).toFixed(3)),
    summaryText: result.text,
  }),
);
