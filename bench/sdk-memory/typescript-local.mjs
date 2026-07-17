#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const SCHEMA_VERSION = "muzen.sdk-memory-benchmark.v1";
const ROLES = [
  "correctness",
  "security",
  "performance",
  "maintainability",
  "architecture",
  "validator",
];

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "../..");

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    printHelp();
    return 0;
  }

  const packagePath = resolve(
    repoRoot,
    "sdk/typescript/packages/muzen-sdk/package.json",
  );
  const sdkEntry = resolve(
    repoRoot,
    "sdk/typescript/packages/muzen-sdk/dist/index.js",
  );
  if (!existsSync(sdkEntry)) {
    throw new Error(
      `TypeScript SDK build not found at ${sdkEntry}; run npm --prefix sdk/typescript/packages/muzen-sdk run build`,
    );
  }

  const sdkPackage = JSON.parse(readFileSync(packagePath, "utf8"));
  const { createMuzen, openai } = await import(pathToFileURL(sdkEntry).href);

  const runnerPath = args.runnerPath ?? defaultRunnerPath();
  const repo = resolve(process.cwd(), args.repo);
  const maxActiveSessions = args.maxActive ?? args.sessions;
  const startedAtUtc = new Date().toISOString();
  const started = performance.now();
  const sampler = new MemorySampler(args.sampleMs);
  let muzen;
  let result;
  let errorMessage;

  sampler.start();
  try {
    muzen = await createMuzen({
      runnerPath,
      clientName: "@muzen/sdk-memory-bench",
    });
    sampler.sample();
    result = await muzen.runSwarm({
      repo,
      files: args.changedFiles,
      agents: buildAgents(args.sessions, args),
      model: benchmarkModel(args, openai),
      limits: {
        maxActiveSessions,
        maxFileBytes: args.maxFileKb * 1024,
        maxSearchMatches: args.maxSearchMatches,
      },
    });
    if (args.holdMs > 0) {
      await delay(args.holdMs);
    }
    sampler.sample();
  } catch (error) {
    errorMessage = error instanceof Error ? error.message : String(error);
  } finally {
    sampler.stop();
    if (muzen) {
      await muzen.close().catch(() => {});
    }
  }

  const elapsedMs = Math.max(1, Math.ceil(performance.now() - started));
  const finishedAtUtc = new Date().toISOString();
  const benchmarkFailures = benchmarkFailuresFor(result, errorMessage, sampler);
  const report = {
    schemaVersion: SCHEMA_VERSION,
    sdk: {
      language: "typescript",
      package: sdkPackage.name,
      version: sdkPackage.version,
      runtime: `node ${process.version}`,
    },
    mode: "local-runner-stdio",
    runner: {
      path: runnerPath,
    },
    workload: {
      repo,
      sessions: args.sessions,
      maxActiveSessions,
      maxTurns: args.maxTurns,
      maxToolCalls: args.maxToolCalls,
      maxFileKb: args.maxFileKb,
      maxSearchMatches: args.maxSearchMatches,
      changedFiles: args.changedFiles,
    },
    timing: {
      startedAtUtc,
      finishedAtUtc,
      elapsedMs,
      holdMs: args.holdMs,
      sampleMs: args.sampleMs,
    },
    memory: sampler.report(),
    result: result
      ? {
          status: result.status,
          agents: result.usage.agents,
          completedAgents: result.usage.completedAgents,
          outputs: result.outputs.length,
          modelCalls: result.usage.modelCalls,
          toolCalls: result.usage.toolCalls,
          totalTokens: result.usage.totalTokens,
          metadata: result.metadata ?? {},
        }
      : undefined,
    benchmarkValid: benchmarkFailures.length === 0,
    benchmarkFailures,
  };

  const output = JSON.stringify(report, null, 2);
  if (args.output) {
    mkdirSync(dirname(resolve(process.cwd(), args.output)), { recursive: true });
    writeFileSync(args.output, `${output}\n`);
  } else {
    console.log(output);
  }
  return benchmarkFailures.length === 0 ? 0 : 1;
}

function benchmarkModel(options, openai) {
  if (options.openaiModel) {
    return openai({
      model: options.openaiModel,
      maxOutputTokens: options.maxOutputTokens,
      temperature: 0,
    });
  }
  return {
    kind: "callback",
    handler(request) {
      const toolResults = request.transcript.filter(
        (item) => item && typeof item === "object" && item.kind === "tool_result",
      );
      if (toolResults.length === 0) {
        return {
          toolCalls: [
            { toolId: "read_diff", arguments: {} },
            { toolId: "list_changed_files", arguments: {} },
            { toolId: "read_file", arguments: { path: "Cargo.toml" } },
            { toolId: "search_text", arguments: { query: "fn|class|export|pub|TODO" } },
          ],
          usage: { inputTokens: 8, outputTokens: 4, totalTokens: 12 },
        };
      }
      return {
        content: JSON.stringify({
          summary: "SDK memory benchmark completed.",
          fileVerdicts: [
            {
              path: "Cargo.toml",
              verdict: "clean",
              summary: "No benchmark issues found.",
            },
          ],
          findings: [],
        }),
        usage: { inputTokens: 8, outputTokens: 4, totalTokens: 12 },
      };
    },
  };
}

function buildAgents(count, options) {
  return Array.from({ length: count }, (_, index) => {
    const role = ROLES[index % ROLES.length];
    if (options.openaiModel) {
      return {
        id: `typescript-sdk-bench-session-${index}`,
        objective:
          `SDK memory benchmark as ${role}: perform a deeper bounded repository scan with several read and search tools, then write a concise final assessment.`,
        instructions: [
          {
            kind: "system",
            text:
              "This is a real-model memory benchmark. Use exactly five repository tool calls before finalizing: read_diff, list_changed_files, read_file for Cargo.toml, read_file for src/workspace/snapshot.rs, and search_text for SnapshotContentRef. After the fifth tool result, do not call more tools; return a concise final answer.",
            trusted: true,
          },
        ],
        budget: {
          maxTurns: options.maxTurns,
          maxToolCalls: options.maxToolCalls,
          maxPromptTokens: 32_000,
          maxOutputTokens: options.maxOutputTokens,
        },
      };
    }
    return {
      id: `typescript-sdk-bench-session-${index}`,
      role,
      objective:
        `SDK memory benchmark as ${role}: gather diff, file, and search evidence, then record one concise benchmark finding.`,
      budget: {
        maxTurns: options.maxTurns,
        maxToolCalls: options.maxToolCalls,
        maxPromptTokens: 32_000,
        maxOutputTokens: options.maxOutputTokens,
      },
    };
  });
}

function benchmarkFailuresFor(reviewResult, error, sampler) {
  const failures = [];
  if (error) {
    failures.push(`benchmark errored: ${error}`);
  }
  if (!reviewResult) {
    failures.push("no review result returned");
  } else {
    if (reviewResult.status !== "completed") {
      failures.push(`swarm status was ${reviewResult.status}`);
    }
    if (reviewResult.outputs.length === 0) {
      failures.push("no agent outputs returned");
    }
    if (reviewResult.usage.completedAgents !== reviewResult.usage.agents) {
      failures.push(
        `completed ${reviewResult.usage.completedAgents} of ${reviewResult.usage.agents} agents`,
      );
    }
    if (reviewResult.usage.toolCalls === 0) {
      failures.push("no repository tools were called");
    }
  }
  if (sampler.peakCombinedRssBytes === 0) {
    failures.push("memory sampler recorded no RSS samples");
  }
  return failures;
}

class MemorySampler {
  constructor(sampleMs) {
    this.sampleMs = sampleMs;
    this.started = performance.now();
    this.samples = [];
    this.runnerPids = new Set();
    this.peakClientRssBytes = 0;
    this.peakRunnerRssBytes = 0;
    this.peakCombinedRssBytes = 0;
  }

  start() {
    this.sample();
    this.timer = setInterval(() => this.sample(), this.sampleMs);
  }

  stop() {
    if (this.timer) {
      clearInterval(this.timer);
    }
    this.sample();
  }

  sample() {
    const table = processTable();
    const clientRssBytes = rssBytesForPid(table, process.pid);
    const runners = runnerDescendants(table, process.pid);
    const runnerRssBytes = runners.reduce((sum, proc) => sum + proc.rssBytes, 0);
    const combinedRssBytes = clientRssBytes + runnerRssBytes;
    for (const proc of runners) {
      this.runnerPids.add(proc.pid);
    }
    this.peakClientRssBytes = Math.max(this.peakClientRssBytes, clientRssBytes);
    this.peakRunnerRssBytes = Math.max(this.peakRunnerRssBytes, runnerRssBytes);
    this.peakCombinedRssBytes = Math.max(this.peakCombinedRssBytes, combinedRssBytes);
    this.samples.push({
      atMs: Math.max(0, Math.round(performance.now() - this.started)),
      clientRssBytes,
      runnerRssBytes,
      combinedRssBytes,
      runnerPids: runners.map((proc) => proc.pid),
    });
  }

  report() {
    const last = this.samples[this.samples.length - 1];
    return {
      peakClientRssBytes: this.peakClientRssBytes,
      peakRunnerRssBytes: this.peakRunnerRssBytes,
      peakCombinedRssBytes: this.peakCombinedRssBytes,
      finalClientRssBytes: last?.clientRssBytes ?? 0,
      finalRunnerRssBytes: last?.runnerRssBytes ?? 0,
      finalCombinedRssBytes: last?.combinedRssBytes ?? 0,
      sampleCount: this.samples.length,
      runnerPids: Array.from(this.runnerPids).sort((left, right) => left - right),
      samples: this.samples,
    };
  }
}

function processTable() {
  const output = execFileSync("ps", ["-axo", "pid=,ppid=,rss=,comm="], {
    encoding: "utf8",
  });
  return output
    .split("\n")
    .map((line) => {
      const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.+)$/);
      if (!match) {
        return undefined;
      }
      return {
        pid: Number(match[1]),
        ppid: Number(match[2]),
        rssBytes: Number(match[3]) * 1024,
        command: match[4],
      };
    })
    .filter(Boolean);
}

function rssBytesForPid(table, pid) {
  return table.find((proc) => proc.pid === pid)?.rssBytes ?? process.memoryUsage().rss;
}

function runnerDescendants(table, parentPid) {
  const byParent = new Map();
  for (const proc of table) {
    const children = byParent.get(proc.ppid) ?? [];
    children.push(proc);
    byParent.set(proc.ppid, children);
  }
  const descendants = [];
  const stack = [...(byParent.get(parentPid) ?? [])];
  while (stack.length > 0) {
    const proc = stack.pop();
    descendants.push(proc);
    stack.push(...(byParent.get(proc.pid) ?? []));
  }
  return descendants.filter((proc) => basename(proc.command).includes("muzen-runner"));
}

function defaultRunnerPath() {
  if (process.env.MUZEN_RUNNER_PATH) {
    return process.env.MUZEN_RUNNER_PATH;
  }
  for (const candidate of [
    resolve(repoRoot, "target/release/muzen-runner"),
    resolve(repoRoot, "target/debug/muzen-runner"),
  ]) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  return "muzen-runner";
}

function parseArgs(argv) {
  const parsed = {
    repo: ".",
    sessions: 50,
    maxActive: undefined,
    maxTurns: 4,
    maxToolCalls: 8,
    maxOutputTokens: 512,
    maxFileKb: 200,
    maxSearchMatches: 120,
    holdMs: 1000,
    sampleMs: 25,
    openaiModel: undefined,
    runnerPath: undefined,
    output: undefined,
    changedFiles: [],
    help: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
    } else if (arg === "--repo") {
      parsed.repo = requireValue(argv, ++index, arg);
    } else if (arg === "--sessions") {
      parsed.sessions = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--max-active") {
      parsed.maxActive = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--max-turns") {
      parsed.maxTurns = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--max-tool-calls") {
      parsed.maxToolCalls = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--max-output-tokens") {
      parsed.maxOutputTokens = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--max-file-kb") {
      parsed.maxFileKb = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--max-search-matches") {
      parsed.maxSearchMatches = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--openai-model") {
      parsed.openaiModel = requireValue(argv, ++index, arg);
    } else if (arg === "--hold-ms") {
      parsed.holdMs = nonNegativeInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--sample-ms") {
      parsed.sampleMs = positiveInt(requireValue(argv, ++index, arg), arg);
    } else if (arg === "--runner-path") {
      parsed.runnerPath = requireValue(argv, ++index, arg);
    } else if (arg === "--output") {
      parsed.output = requireValue(argv, ++index, arg);
    } else if (arg === "--changed-file") {
      parsed.changedFiles.push(requireValue(argv, ++index, arg));
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return parsed;
}

function requireValue(argv, index, name) {
  const value = argv[index];
  if (!value || value.startsWith("--")) {
    throw new Error(`${name} requires a value`);
  }
  return value;
}

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error(`${name} must be a positive integer`);
  }
  return parsed;
}

function nonNegativeInt(value, name) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`${name} must be a non-negative integer`);
  }
  return parsed;
}

function delay(ms) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, ms));
}

function printHelp() {
  console.log(`Usage: node bench/sdk-memory/typescript-local.mjs [options]

Options:
  --repo PATH                 Repo to review. Default: .
  --sessions N                Number of SDK agent sessions. Default: 50
  --max-active N              Max active sessions. Default: sessions
  --max-turns N               Per-agent max turns. Default: 4
  --max-tool-calls N          Per-agent max tool calls. Default: 8
  --max-output-tokens N       Per-agent max output tokens. Default: 512
  --max-file-kb N             Snapshot file limit. Default: 200
  --max-search-matches N      Search result limit. Default: 120
  --openai-model MODEL        Use a real OpenAI hosted model instead of callback
  --changed-file PATH         Changed file to pass to the SDK. Repeatable.
  --runner-path PATH          muzen-runner path. Default: MUZEN_RUNNER_PATH, release, debug, PATH
  --hold-ms N                 Keep processes alive after review for sampling. Default: 1000
  --sample-ms N               RSS sampling interval. Default: 25
  --output PATH               Write JSON report to path instead of stdout
`);
}

const exitCode = await main();
process.exitCode = exitCode;
