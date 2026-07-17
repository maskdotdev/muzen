#!/usr/bin/env node
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const defaultRepoRoot = resolve(scriptDirectory, "../..");
const nodeMajor = Number(process.versions.node.split(".")[0]);
if (nodeMajor < 22) throw new Error(`Node >= 22 is required; found ${process.version}`);

function parseArgs(argv) {
  const options = {
    languages: ["python", "ts", "rust"],
    transports: ["local", "http"],
    concurrency: 5,
    waves: 3,
    repoRoot: defaultRepoRoot,
    latencyMs: 300,
    readFiles: 5,
    store: "sqlite",
    allowDebug: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (flag === "--languages") options.languages = value.split(",").filter(Boolean), index += 1;
    else if (flag === "--transports") options.transports = value.split(",").filter(Boolean), index += 1;
    else if (flag === "--concurrency") options.concurrency = Number(value), index += 1;
    else if (flag === "--waves") options.waves = Number(value), index += 1;
    else if (flag === "--repo-root") options.repoRoot = resolve(value), index += 1;
    else if (flag === "--latency-ms") options.latencyMs = Number(value), index += 1;
    else if (flag === "--read-files") options.readFiles = Number(value), index += 1;
    else if (flag === "--store") options.store = value, index += 1;
    else if (flag === "--output") options.output = value, index += 1;
    else if (flag === "--runstamp") options.runstamp = value, index += 1;
    else if (flag === "--allow-debug") options.allowDebug = true;
    else throw new Error(`unknown argument: ${flag}`);
  }
  const validLanguages = new Set(["python", "ts", "rust"]);
  const validTransports = new Set(["local", "http"]);
  if (options.languages.length === 0 || options.languages.some((item) => !validLanguages.has(item))) throw new Error("--languages must be a comma-separated subset of python,ts,rust");
  if (options.transports.length === 0 || options.transports.some((item) => !validTransports.has(item))) throw new Error("--transports must be a comma-separated subset of local,http");
  for (const field of ["concurrency", "waves", "readFiles"]) {
    if (!Number.isInteger(options[field]) || options[field] < 1) throw new Error(`--${field.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)} must be positive`);
  }
  if (!Number.isInteger(options.latencyMs) || options.latencyMs < 0) throw new Error("--latency-ms must be non-negative");
  if (!["sqlite", "memory"].includes(options.store)) throw new Error("--store must be sqlite or memory");
  options.runstamp ??= new Date().toISOString().replaceAll(":", "-").replace(".", "-");
  options.output ??= join(options.repoRoot, "bench", "results-agent-explore", `${options.runstamp}.json`);
  options.output = isAbsolute(options.output) ? options.output : resolve(process.cwd(), options.output);
  return options;
}

async function exists(path) {
  try { await stat(path); return true; } catch { return false; }
}

async function binary(options, envName, name) {
  const configured = process.env[envName];
  const release = join(options.repoRoot, "target", "release", name);
  const debug = join(options.repoRoot, "target", "debug", name);
  let selected = configured ? resolve(configured) : release;
  if (!(await exists(selected)) && options.allowDebug && !configured && await exists(debug)) selected = debug;
  if (!(await exists(selected))) throw new Error(`${name} is missing; build target/release/${name} or set ${envName}`);
  const debugMarker = `${sep}target${sep}debug${sep}`;
  if (!options.allowDebug && selected.includes(debugMarker)) throw new Error(`${name} points to a debug build (${selected}); build release or pass --allow-debug explicitly`);
  return selected;
}

function delay(milliseconds) {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

function start(command, args, options = {}) {
  const child = spawn(command, args, {
    cwd: options.cwd,
    env: options.env,
    stdio: ["ignore", "pipe", "pipe"],
    detached: options.detached ?? process.platform !== "win32",
  });
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdoutText = "";
  child.stderrText = "";
  child.stdout.on("data", (chunk) => { child.stdoutText += chunk; });
  child.stderr.on("data", (chunk) => { child.stderrText += chunk; });
  return child;
}

async function stop(child) {
  if (child === undefined || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolvePromise) => child.once("exit", resolvePromise));
  const signal = (name) => {
    try {
      if (process.platform !== "win32") process.kill(-child.pid, name);
    } catch {}
    try { child.kill(name); } catch {}
  };
  signal("SIGTERM");
  await Promise.race([exited, delay(1_000)]);
  if (child.exitCode === null && child.signalCode === null) {
    signal("SIGKILL");
    await Promise.race([exited, delay(1_000)]);
  }
  child.stdout?.destroy();
  child.stderr?.destroy();
}

async function waitListening(child) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const match = child.stdoutText.match(/(?:^|\n)LISTENING (\d+)(?:\n|$)/);
    if (match) return Number(match[1]);
    if (child.exitCode !== null) throw new Error(`mock model exited ${child.exitCode}: ${child.stderrText.trim()}`);
    await delay(20);
  }
  throw new Error(`mock model did not announce a port: ${child.stderrText.trim()}`);
}

async function waitService(port, child) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`muzen-agent-service exited ${child.exitCode}: ${child.stderrText.trim()}`);
    try {
      const response = await fetch(`http://127.0.0.1:${port}/v1/capabilities`);
      if (response.ok) return;
    } catch {}
    await delay(25);
  }
  throw new Error(`muzen-agent-service did not listen on port ${port}: ${child.stderrText.trim()}`);
}

async function unusedPort() {
  const { createServer } = await import("node:net");
  const server = createServer();
  await new Promise((resolvePromise, reject) => server.once("error", reject).listen(0, "127.0.0.1", resolvePromise));
  const address = server.address();
  const port = typeof address === "object" && address !== null ? address.port : 0;
  await new Promise((resolvePromise) => server.close(resolvePromise));
  return port;
}

async function processRows(samplerBinary) {
  const child = start(samplerBinary, ["--sample-processes"], { detached: false });
  const code = await new Promise((resolvePromise) => child.once("exit", resolvePromise));
  if (code !== 0) throw new Error(`native RSS sampler failed: ${child.stderrText.trim()}`);
  const rows = JSON.parse(child.stdoutText);
  if (!Array.isArray(rows)) throw new Error("native RSS sampler returned a non-array");
  return rows;
}

function descendantSet(rows, roots) {
  const result = new Set(roots);
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of rows) {
      if (result.has(row.ppid) && !result.has(row.pid)) result.add(row.pid), changed = true;
    }
  }
  return result;
}

class Sampler {
  constructor(transport, language, service, samplerBinary) {
    this.transport = transport;
    this.language = language;
    this.service = service;
    this.samplerBinary = samplerBinary;
    this.driverPids = new Set();
    this.samples = [];
  }
  async sample() {
    const rows = await processRows(this.samplerBinary);
    const driverTree = descendantSet(rows, this.driverPids);
    const serviceTree = this.service ? descendantSet(rows, new Set([this.service.pid])) : new Set();
    const driverRssBytes = rows.filter((row) => this.driverPids.has(row.pid)).reduce((sum, row) => sum + row.rssBytes, 0);
    let serviceRssBytes;
    if (this.transport === "http") {
      serviceRssBytes = rows.filter((row) => serviceTree.has(row.pid)).reduce((sum, row) => sum + row.rssBytes, 0);
    } else if (this.language === "rust") {
      serviceRssBytes = driverRssBytes;
    } else {
      serviceRssBytes = rows
        .filter((row) => driverTree.has(row.pid) && basename(row.command.split(/\s+/)[0]).includes("muzen-agent-runner"))
        .reduce((sum, row) => sum + row.rssBytes, 0);
    }
    const treeRssBytes = rows.filter((row) => driverTree.has(row.pid)).reduce((sum, row) => sum + row.rssBytes, 0);
    const sample = { atMs: Date.now(), serviceRssBytes, driverRssBytes, treeRssBytes };
    this.samples.push(sample);
    return sample;
  }
  start() {
    this.timer = setInterval(() => void this.sample(), 100);
  }
  stop() {
    clearInterval(this.timer);
  }
}

function driverCommand(language, transport, options, binaries, modelUrl, serviceUrl) {
  const common = [
    "--transport", language === "rust" ? transport : transport === "local" ? "local_runner" : "http",
    "--root", options.repoRoot,
    "--model-base-url", modelUrl,
    "--read-files", String(options.readFiles),
  ];
  if (transport === "http") common.push("--base-url", serviceUrl);
  if (language === "python") return {
    command: process.env.PYTHON ?? "python3",
    args: [join(options.repoRoot, "bench/agent-explore/driver.py"), ...common],
    env: { ...process.env, PYTHONPATH: [join(options.repoRoot, "sdk/python"), process.env.PYTHONPATH].filter(Boolean).join(":") },
  };
  if (language === "ts") return {
    command: process.execPath,
    args: [join(options.repoRoot, "bench/agent-explore/driver.mjs"), ...common],
    env: process.env,
  };
  return { command: binaries.rustDriver, args: common, env: process.env };
}

async function runCell(language, transport, options, binaries, temporaryRoot) {
  const children = [];
  let sampler;
  try {
    const mock = start(process.execPath, [
      join(options.repoRoot, "bench/agent-explore/mock-model-server.mjs"),
      "--port", "0",
      "--latency-ms", String(options.latencyMs),
      "--read-files", String(options.readFiles),
    ], { cwd: options.repoRoot, env: process.env });
    children.push(mock);
    const modelPort = await waitListening(mock);
    const modelUrl = `http://127.0.0.1:${modelPort}/v1`;
    let service;
    let serviceUrl;
    if (transport === "http") {
      const port = await unusedPort();
      const serviceArgs = ["--listen", `127.0.0.1:${port}`, "--store", options.store, "--allow-loopback-http"];
      if (options.store === "sqlite") serviceArgs.push("--db", join(temporaryRoot, `${language}-${transport}.sqlite`));
      service = start(binaries.service, serviceArgs, { cwd: options.repoRoot, env: process.env });
      children.push(service);
      await waitService(port, service);
      serviceUrl = `http://127.0.0.1:${port}`;
    }
    sampler = new Sampler(transport, language, service, binaries.rustDriver);
    await sampler.sample();
    const initialServiceRssBytes = sampler.samples.at(-1).serviceRssBytes;
    sampler.start();
    const waves = [];
    const modelTurnsPerRun = options.readFiles + 3;
    const sequentialEstimateTurnsPerRun = options.readFiles + 2;
    const theoreticalSequentialMs = sequentialEstimateTurnsPerRun * options.latencyMs * options.concurrency;
    const timeoutMs = Math.max(60_000, modelTurnsPerRun * options.latencyMs * options.concurrency * 4 + 20_000);
    for (let wave = 1; wave <= options.waves; wave += 1) {
      const sampleStart = sampler.samples.length;
      const started = performance.now();
      const specifications = Array.from({ length: options.concurrency }, () => driverCommand(language, transport, options, binaries, modelUrl, serviceUrl));
      const running = specifications.map((specification) => {
        const child = start(specification.command, specification.args, { cwd: options.repoRoot, env: {
          ...specification.env,
          MUZEN_AGENT_RUNNER_BIN: binaries.runner,
        } });
        sampler.driverPids.add(child.pid);
        children.push(child);
        return { child, specification };
      });
      const results = await Promise.all(running.map(async ({ child, specification }) => {
        const completion = new Promise((resolvePromise) => child.once("exit", (code, signal) => resolvePromise({ code, signal })));
        const outcome = await Promise.race([completion, delay(timeoutMs).then(() => ({ timeout: true }))]);
        if (outcome.timeout) {
          await stop(child);
          throw new Error(`HANG FINDING: ${language}/${transport} driver exceeded ${timeoutMs}ms at concurrency ${options.concurrency}; driver stderr: ${child.stderrText.trim()}; service stderr: ${service?.stderrText.trim() ?? "n/a"}; model stderr: ${mock.stderrText.trim()}`);
        }
        if (outcome.code !== 0) throw new Error(`${language}/${transport} driver exited ${outcome.code ?? outcome.signal}\nstdout: ${child.stdoutText.trim()}\nstderr: ${child.stderrText.trim()}`);
        const lines = child.stdoutText.trim().split("\n").filter(Boolean);
        if (lines.length !== 1) throw new Error(`${language}/${transport} driver printed ${lines.length} JSON lines, expected 1: ${child.stdoutText.trim()}`);
        let record;
        try { record = JSON.parse(lines[0]); } catch { throw new Error(`${language}/${transport} driver printed invalid JSON: ${lines[0]}`); }
        const expectedTurns = options.readFiles + 3;
        const expectedTools = options.readFiles + 2;
        if (record.turns !== expectedTurns || record.toolCalls !== expectedTools || !Number.isFinite(record.durationMs) || typeof record.summaryText !== "string" || !record.summaryText.trim()) {
          throw new Error(`${language}/${transport} driver assertion mismatch: ${JSON.stringify(record)}`);
        }
        return record;
      }));
      const wallMs = performance.now() - started;
      for (const { child } of running) sampler.driverPids.delete(child.pid);
      await delay(350);
      const settled = await sampler.sample();
      const samples = sampler.samples.slice(sampleStart);
      waves.push({
        wave,
        runs: results,
        wallMs,
        theoreticalSequentialMs,
        speedup: theoreticalSequentialMs === 0 ? null : theoreticalSequentialMs / wallMs,
        servicePeakRssBytes: Math.max(0, ...samples.map((sample) => sample.serviceRssBytes)),
        serviceSettledRssBytes: settled.serviceRssBytes,
        driverPeakRssBytes: Math.max(0, ...samples.map((sample) => sample.driverRssBytes)),
        processTreePeakRssBytes: Math.max(0, ...samples.map((sample) => sample.treeRssBytes)),
      });
    }
    sampler.stop();
    const finalServiceRssBytes = waves.at(-1).serviceSettledRssBytes;
    const totalRuns = options.waves * options.concurrency;
    return {
      language,
      transport,
      store: transport === "http" ? options.store : null,
      modelTurnsPerRun,
      sequentialEstimateTurnsPerRun,
      toolCallsPerRun: options.readFiles + 2,
      initialServiceRssBytes,
      finalServiceRssBytes,
      retainedPerRunBytes: transport === "http" ? (finalServiceRssBytes - initialServiceRssBytes) / totalRuns : 0,
      waves,
    };
  } finally {
    sampler?.stop();
    await Promise.allSettled(children.reverse().map(stop));
  }
}

function average(values) {
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

function mib(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}

function table(cells) {
  const rows = [
    "| language | transport | wave time | sequential estimate | speedup | service peak / settled RSS | retained / run |",
    "|---|---:|---:|---:|---:|---:|---:|",
  ];
  for (const cell of cells) {
    const wall = average(cell.waves.map((wave) => wave.wallMs));
    const sequential = average(cell.waves.map((wave) => wave.theoreticalSequentialMs));
    const peak = Math.max(...cell.waves.map((wave) => wave.servicePeakRssBytes));
    const settled = cell.waves.at(-1).serviceSettledRssBytes;
    rows.push(`| ${cell.language} | ${cell.transport} | ${(wall / 1000).toFixed(2)}s | ${(sequential / 1000).toFixed(2)}s | ${(sequential / wall).toFixed(2)}x | ${mib(peak)} / ${mib(settled)} | ${mib(cell.retainedPerRunBytes)} |`);
  }
  return rows.join("\n");
}

const options = parseArgs(process.argv.slice(2));
const binaries = {
  service: await binary(options, "MUZEN_AGENT_SERVICE_BIN", "muzen-agent-service"),
  runner: await binary(options, "MUZEN_AGENT_RUNNER_BIN", "muzen-agent-runner"),
  rustDriver: await binary(options, "MUZEN_AGENT_EXPLORE_BENCH_BIN", "muzen-agent-explore-bench"),
};
const temporaryRoot = await mkdtemp(join(tmpdir(), "muzen-agent-explore-"));
const cells = [];
let failure = null;
let currentCell = null;
let exitCode = 0;
const reportValue = () => ({
  schema: "muzen.agent-explore-benchmark.v2",
  runstamp: options.runstamp,
  generatedAt: new Date().toISOString(),
  complete: failure === null,
  config: {
    languages: options.languages,
    transports: options.transports,
    concurrency: options.concurrency,
    waves: options.waves,
    repoRoot: options.repoRoot,
    latencyMs: options.latencyMs,
    readFiles: options.readFiles,
    store: options.store,
    modelTurnsPerRun: options.readFiles + 3,
    sequentialEstimateTurnsPerRun: options.readFiles + 2,
  },
  binaries,
  cells,
  ...(failure === null ? {} : { failure }),
});
try {
  for (const language of options.languages) {
    for (const transport of options.transports) {
      currentCell = { language, transport };
      process.stderr.write(`[agent-explore] ${language}/${transport}: ${options.waves} waves x ${options.concurrency}\n`);
      cells.push(await runCell(language, transport, options, binaries, temporaryRoot));
    }
  }
} catch (error) {
  exitCode = 1;
  failure = {
    ...currentCell,
    message: error instanceof Error ? error.message : String(error),
  };
} finally {
  await mkdir(dirname(options.output), { recursive: true });
  await writeFile(options.output, `${JSON.stringify(reportValue(), null, 2)}\n`);
  console.log(table(cells));
  console.log(`\nReport: ${options.output}`);
  if (failure !== null) console.error(`\nFAILED ${failure.language}/${failure.transport}: ${failure.message}`);
  await rm(temporaryRoot, { recursive: true, force: true });
}
process.exit(exitCode);
