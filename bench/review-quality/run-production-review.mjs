#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { pathToFileURL } from "node:url";

const SCHEMA_VERSION = "muzen.review-quality-benchmark.v1";
const JOB_SCHEMA_VERSION = "heimdaal.review-run.v1";
const DEFAULT_MODEL = process.env.MODEL || "gpt-4o-mini";

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const job = args.job
    ? readJson(path.resolve(args.job))
    : buildProductionReviewJob({
        repo: path.resolve(args.repo || "."),
        runnerPath: path.resolve(args.runnerPath || "target/release/muzen-runner"),
        goldenPath: args.golden ? path.resolve(args.golden) : null,
        baseRef: required(args.baseRef, "--base-ref is required"),
        runId: args.runId || `review-quality-${timestamp()}`,
        mode: args.mode || "review",
        outputPath: args.output ? path.resolve(args.output) : null,
        traceOutputDir: args.traceOutputDir ? path.resolve(args.traceOutputDir) : null,
        tempDir: args.tempDir
          ? path.resolve(args.tempDir)
          : fs.mkdtempSync(path.join(os.tmpdir(), "muzen-review-quality-")),
        maxCapturedTextBytes: args.maxCapturedTextBytes
          ? numberArg(args.maxCapturedTextBytes, 0)
          : null,
        model: args.model || DEFAULT_MODEL,
        sessions: nonnegativeNumberArg(args.sessions, 0),
        maxActive: numberArg(args.maxActive, 8),
        maxTurns: numberArg(args.maxTurns, 10),
        maxToolCalls: numberArg(args.maxToolCalls, 32),
        maxPromptTokens: numberArg(args.maxPromptTokens, 64000),
        maxOutputTokens: numberArg(args.maxOutputTokens, 8000),
      });
  const runnerPath = path.resolve(args.runnerPath || job.runnerPath || "target/release/muzen-runner");
  if (args.output) job.outputPath = path.resolve(args.output);
  if (args.traceOutputDir) job.traceOutputDir = path.resolve(args.traceOutputDir);

  const startedAt = Date.now();
  const run = await runRunnerReview(runnerPath, job.runStart);
  const elapsedMs = Date.now() - startedAt;
  fs.writeFileSync(job.framesPath, `${run.frames.map((frame) => JSON.stringify(frame)).join("\n")}\n`);

  const report = buildProductionReviewReport(job, run, { elapsedMs });
  writeProductionReviewReport(report, job.outputPath);
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  process.exitCode = report.reviewValid ? 0 : 1;
}

export function buildProductionReviewJob({
  repo,
  runnerPath,
  goldenPath,
  baseRef,
  runId,
  mode = "review",
  sessions = 0,
  maxActive = 8,
  maxTurns = 10,
  maxToolCalls = 32,
  maxPromptTokens = 64000,
  maxOutputTokens = 8000,
  model = DEFAULT_MODEL,
  outputPath = null,
  traceOutputDir = null,
  maxCapturedTextBytes = null,
  tempDir = null,
}) {
  const startedAt = Date.now();
  const jobDir = path.resolve(tempDir || fs.mkdtempSync(path.join(os.tmpdir(), "muzen-review-quality-")));
  fs.mkdirSync(jobDir, { recursive: true });
  const jobDirReadyAt = Date.now();
  const changedFiles = gitChangedFiles(repo, baseRef);
  const changedFilesReadyAt = Date.now();
  const changedFilePaths = changedFiles
    .map((file) => file.newPath ?? file.oldPath)
    .filter(Boolean);
  const inlineDiff = git(repo, ["diff", "--find-renames", "--find-copies", `${baseRef}...HEAD`]);
  const inlineDiffReadyAt = Date.now();
  const runStart = buildRunnerStart({
    runId,
    repo,
    baseRef,
    changedFiles,
    changedFilePaths,
    inlineDiff,
    model,
    sessions,
    maxActive,
    maxTurns,
    maxToolCalls,
    maxPromptTokens,
    maxOutputTokens,
  });
  const runStartReadyAt = Date.now();
  const requestPath = path.join(jobDir, "run-start.json");
  const framesPath = path.join(jobDir, "frames.jsonl");
  fs.writeFileSync(requestPath, `${JSON.stringify(runStart, null, 2)}\n`);
  fs.writeFileSync(framesPath, "");
  const writtenAt = Date.now();
  return {
    schemaVersion: JOB_SCHEMA_VERSION,
    mode,
    repo,
    runnerPath,
    goldenPath,
    baseRef,
    runId,
    model,
    outputPath,
    traceOutputDir,
    maxCapturedTextBytes,
    tempDir: jobDir,
    requestPath,
    framesPath,
    changedFiles,
    changedFilePaths,
    inlineDiff,
    runStart,
    timing: {
      elapsedMs: writtenAt - startedAt,
      jobDirMs: jobDirReadyAt - startedAt,
      changedFilesMs: changedFilesReadyAt - jobDirReadyAt,
      inlineDiffMs: inlineDiffReadyAt - changedFilesReadyAt,
      runStartBuildMs: runStartReadyAt - inlineDiffReadyAt,
      writeArtifactsMs: writtenAt - runStartReadyAt,
    },
  };
}

export function buildProductionReviewReport(job, run, { elapsedMs }) {
  const finalResult = run.result;
  const verdictCounts = fileReviewVerdictCounts(finalResult?.fileReviews || []);
  const frames = Array.isArray(run.frames) ? run.frames : readFramesJsonl(job.framesPath);
  const diagnostics = eventDiagnostics(frames);
  const qualityDiagnostics = finalResult?.summary?.qualityDiagnostics || {};
  const golden = job.goldenPath ? readJson(job.goldenPath) : { issues: [] };
  const scoring = scoreFindings(finalResult?.findings || [], golden.issues || []);
  const runtimeDiagnostics = buildRuntimeDiagnostics(frames);
  return {
    schemaVersion: SCHEMA_VERSION,
    generatedAtUtc: new Date().toISOString(),
    mode: "production-review-run",
    reviewValid: run.ok && Boolean(finalResult),
    exitCode: run.exitCode,
    error: run.ok ? null : trim(run.error || run.stderr || "review command failed"),
    inputs: {
      repo: job.repo,
      baseRef: job.baseRef,
      runnerPath: job.runnerPath,
      model: job.model,
      changedFileCount: job.changedFilePaths.length,
      goldenIssueCount: golden.issues?.length || 0,
    },
    artifacts: {
      request: job.requestPath,
      frames: job.framesPath,
      traceOutputDir: job.traceOutputDir,
    },
    audit: {
      diagnostics: runtimeDiagnostics,
    },
    review: finalResult
      ? {
          status: finalResult.status,
          sessions: finalResult.summary?.sessions,
          completedSessions: finalResult.summary?.completedSessions,
          reviewUnits: finalResult.summary?.reviewUnits,
          completedReviewUnits: finalResult.summary?.completedReviewUnits,
          fileReviews: finalResult.fileReviews?.length || 0,
          fileReviewVerdicts: verdictCounts,
          findings: finalResult.findings?.length || 0,
          modelCalls: finalResult.summary?.modelCalls,
          toolCalls: finalResult.summary?.toolCalls,
          modelMetrics: finalResult.summary?.modelMetrics ?? null,
          tokens: {
            inputTokens: finalResult.summary?.inputTokens,
            outputTokens: finalResult.summary?.outputTokens,
            totalTokens: finalResult.summary?.totalTokens,
          },
          artifactStats: {
            artifacts: finalResult.summary?.artifacts,
            artifactBytes: finalResult.summary?.artifactBytes,
          },
          completionDiagnostics: finalResult.summary?.completionDiagnostics ?? [],
          elapsedMs: finalResult.summary?.elapsedMs,
        }
      : null,
    benchmark: {
      elapsedMs,
      jobBuild: job.timing ?? null,
      runnerInvocation: run.timing ?? null,
      hitRate: scoring.hitRate,
      hits: scoring.hits,
      misses: scoring.misses,
      falsePositiveCount: scoring.falsePositiveCount,
      candidateCount: qualityDiagnostics.candidateFindings ?? diagnostics.candidateFindings,
      rescuedCandidateCount: qualityDiagnostics.rescuedCandidates ?? diagnostics.rescuedCandidates,
      rejectedCandidateCount: qualityDiagnostics.rejectedCandidates ?? diagnostics.rejectedCandidates,
      contractRiskUnits: qualityDiagnostics.contractRiskUnits ?? diagnostics.contractRiskUnits,
      contractSeedCount: qualityDiagnostics.contractSeedCount ?? diagnostics.contractSeedCount,
      contractPackCount: qualityDiagnostics.contractPackCount ?? 0,
      contractEvidenceFailures:
        qualityDiagnostics.contractEvidenceFailures ?? diagnostics.requiredEvidenceFailures,
      requiredEvidenceFailures:
        qualityDiagnostics.contractEvidenceFailures ?? diagnostics.requiredEvidenceFailures,
      coverageCounts: qualityDiagnostics.coverageCounts ?? {},
      coverageCountsByLens: qualityDiagnostics.coverageCountsByLens ?? {},
      highRiskFilesBelowTarget: qualityDiagnostics.highRiskFilesBelowTarget ?? [],
      challengeStatusCounts: qualityDiagnostics.challengeStatusCounts ?? {},
      selectedContractPacks: qualityDiagnostics.selectedContractPacks ?? [],
      omittedContractPackCandidates: qualityDiagnostics.omittedContractPackCandidates ?? [],
      explicitCallerCapSessions: qualityDiagnostics.explicitCallerCapSessions ?? 0,
      rejectionReasons: qualityDiagnostics.rejectionReasons ?? {},
      contractRiskCompletionCount: diagnostics.contractRiskCompletionCount,
      searchCount: diagnostics.searchCount,
      importCount: diagnostics.importCount,
      needsReviewCount: verdictCounts.needs_review ?? 0,
      cleanCount: verdictCounts.clean ?? 0,
      skippedCount: verdictCounts.skipped ?? 0,
    },
    findings: finalResult?.findings || [],
  };
}

export function writeProductionReviewReport(report, outputPath) {
  if (outputPath) {
    fs.mkdirSync(path.dirname(outputPath), { recursive: true });
    fs.writeFileSync(outputPath, `${JSON.stringify(report, null, 2)}\n`);
  }
  writeTraceArtifacts(report);
}

function fileReviewVerdictCounts(fileReviews) {
  const counts = {};
  for (const review of fileReviews) {
    const verdict = review.verdict || "unknown";
    counts[verdict] = (counts[verdict] || 0) + 1;
  }
  return counts;
}

export async function runRunnerReview(runnerPath, runStart, { onSpawn = null } = {}) {
  const startedAt = Date.now();
  const child = spawn(runnerPath, ["stdio"], {
    cwd: process.cwd(),
    env: process.env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  if (onSpawn) onSpawn(child);
  const spawnReturnedAt = Date.now();
  const stderr = [];
  child.stderr.on("data", (chunk) => stderr.push(chunk.toString("utf8")));
  const frames = [];
  const pending = new Map();
  let nextId = 1;
  let firstFrameAt = null;
  const readline = createInterface({ input: child.stdout });
  readline.on("line", (line) => {
    if (!line.trim()) return;
    if (firstFrameAt === null) firstFrameAt = Date.now();
    const frame = JSON.parse(line);
    frames.push(frame);
    if (frame.id !== undefined && pending.has(String(frame.id))) {
      const { resolve, reject } = pending.get(String(frame.id));
      pending.delete(String(frame.id));
      if (frame.error) {
        reject(new Error(`${frame.error.message}: ${JSON.stringify(frame.error.data ?? {})}`));
      } else {
        resolve(frame.result);
      }
    }
  });
  const request = (method, params) => {
    const id = nextId++;
    const promise = new Promise((resolve, reject) => {
      pending.set(String(id), { resolve, reject });
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return promise;
  };
  try {
    const handshakeStartedAt = Date.now();
    await request("runner.handshake", {
      protocolVersion: "muzen.runner.v1",
      clientName: "review-quality-benchmark",
    });
    const handshakeCompletedAt = Date.now();
    const runStartStartedAt = Date.now();
    const result = await request("run.start", runStart);
    const runStartCompletedAt = Date.now();
    child.stdin.end();
    return {
      ok: true,
      exitCode: 0,
      result,
      frames,
      stderr: stderr.join(""),
      timing: runnerInvocationTiming({
        startedAt,
        spawnReturnedAt,
        firstFrameAt,
        handshakeStartedAt,
        handshakeCompletedAt,
        runStartStartedAt,
        runStartCompletedAt,
      }),
    };
  } catch (error) {
    const failedAt = Date.now();
    child.stdin.end();
    return {
      ok: false,
      exitCode: 1,
      result: null,
      frames,
      stderr: stderr.join(""),
      error: error instanceof Error ? error.message : String(error),
      timing: runnerInvocationTiming({
        startedAt,
        spawnReturnedAt,
        firstFrameAt,
        failedAt,
      }),
    };
  } finally {
    setTimeout(() => child.kill(), 500).unref();
  }
}

function runnerInvocationTiming({
  startedAt,
  spawnReturnedAt,
  firstFrameAt,
  handshakeStartedAt = null,
  handshakeCompletedAt = null,
  runStartStartedAt = null,
  runStartCompletedAt = null,
  failedAt = null,
}) {
  const completedAt = runStartCompletedAt ?? failedAt ?? Date.now();
  return {
    elapsedMs: completedAt - startedAt,
    spawnReturnMs: spawnReturnedAt - startedAt,
    firstFrameMs: nullableMs(firstFrameAt, startedAt),
    handshakeMs: nullableMs(handshakeCompletedAt, handshakeStartedAt),
    runStartMs: nullableMs(runStartCompletedAt, runStartStartedAt),
    runStartOffsetMs: nullableMs(runStartStartedAt, startedAt),
  };
}

function nullableMs(end, start) {
  return Number.isFinite(end) && Number.isFinite(start) ? end - start : null;
}

function eventDiagnostics(frames) {
  let requiredEvidenceFailures = 0;
  let contractRiskCompletionCount = 0;
  let candidateFindings = 0;
  let rescuedCandidates = 0;
  let rejectedCandidates = 0;
  let contractRiskUnits = 0;
  let contractSeedCount = 0;
  let searchCount = 0;
  let importCount = 0;
  for (const frame of frames) {
    const event = frame.params?.event;
    if (!event) continue;
    if (event.artifactCreated?.toolName === "search_text" || event.artifactCreated?.toolId === "search_text") {
      searchCount += 1;
    }
    if (event.artifactCreated?.toolName === "list_imports" || event.artifactCreated?.toolId === "list_imports") {
      importCount += 1;
    }
    const summary = String(
      event.sessionFinished?.completionSummary ??
        event.modelFailed?.message ??
        frame.params?.completionSummary ??
        "",
    );
    if (summary.includes("contractRisk=true")) contractRiskCompletionCount += 1;
    const match = summary.match(/missingEvidence=(\d+)/);
    if (match) requiredEvidenceFailures += Number(match[1]);
    const candidateMatch = summary.match(/candidateFindings=(\d+)/);
    if (candidateMatch) candidateFindings += Number(candidateMatch[1]);
    const rescuedMatch = summary.match(/rescuedCandidates=(\d+)/);
    if (rescuedMatch) rescuedCandidates += Number(rescuedMatch[1]);
    const rejectedMatch = summary.match(/rejectedCandidates=(\d+)/);
    if (rejectedMatch) rejectedCandidates += Number(rejectedMatch[1]);
  }
  return {
    requiredEvidenceFailures,
    contractRiskCompletionCount,
    candidateFindings,
    rescuedCandidates,
    rejectedCandidates,
    contractRiskUnits,
    contractSeedCount,
    searchCount,
    importCount,
  };
}

function buildRuntimeDiagnostics(frames) {
  const sessions = new Map();
  for (const frame of frames) {
    if (frame.method !== "event.runtime") continue;
    const record = frame.params || {};
    const event = record.event || {};
    const eventName = Object.keys(event)[0];
    const payload = eventName ? event[eventName] || {} : {};
    const sessionId =
      payload.sessionId ??
      record.context?.sessionId ??
      record.context?.session_id ??
      null;
    if (!sessionId) continue;
    const session = sessionDiagnostics(sessions, sessionId);
    if (eventName === "modelStarted") {
      const turn = Number(payload.turnId ?? record.context?.turnId ?? 0);
      session.turns = Math.max(session.turns, turn + 1);
    } else if (eventName === "modelCompleted") {
      session.modelTurnsCompleted += 1;
      const turn = Number(payload.turnId ?? record.context?.turnId ?? 0);
      session.turns = Math.max(session.turns, turn + 1);
    } else if (eventName === "agentTrace") {
      const traceKind = payload.traceKind || "";
      const details = payload.details || {};
      const turn = Number(payload.turnId ?? record.context?.turnId ?? 0);
      if (Number.isFinite(turn)) session.turns = Math.max(session.turns, turn + 1);
      if (traceKind === "model_turn_prepared") {
        session.modelTurnsPrepared += 1;
      } else if (traceKind === "transcript_compacted") {
        session.transcriptCompactions += 1;
        session.evictedToolResults += Number(details.evictedToolResults || 0);
        for (const [kind, count] of Object.entries(details.evictedItemCounts || {})) {
          session.evictedItemCounts[kind] =
            (session.evictedItemCounts[kind] || 0) + Number(count || 0);
        }
      } else if (traceKind === "tool_calls_requested") {
        session.toolCallsRequested += Array.isArray(details.calls) ? details.calls.length : 0;
      } else if (traceKind === "tool_batch_planned") {
        session.toolCallsCompleted += Number(details.scheduledCount || 0);
        session.toolCallsDenied += Number(details.deniedCount || 0);
      } else if (traceKind === "candidate_finding_emitted") {
        session.candidatesEmitted += 1;
      } else if (traceKind === "candidate_validation_started") {
        session.candidateValidationsStarted += 1;
      } else if (traceKind === "candidate_validation_completed") {
        session.candidateValidationsCompleted += 1;
      } else if (traceKind === "candidate_finding_decision") {
        session.candidateDecisions += 1;
        if (details.decision === "rejected") {
          session.rejectedCandidates += 1;
        }
        if (details.decision === "accepted") session.acceptedCandidates += 1;
        if (details.publicationSkippedBudgetExhausted) {
          session.publicationSkippedBudgetExhausted += 1;
        }
      } else if (traceKind === "candidate_publication_skipped") {
        session.publicationSkipped += 1;
        if (details.publicationSkippedBudgetExhausted) {
          session.publicationSkippedBudgetExhausted += 1;
        }
      }
    } else if (eventName === "sessionFinished") {
      session.status = payload.status || session.status;
    }
  }
  return {
    sessions: [...sessions.values()].sort((left, right) => left.sessionId.localeCompare(right.sessionId)),
  };
}

function sessionDiagnostics(sessions, sessionId) {
  if (!sessions.has(sessionId)) {
    sessions.set(sessionId, {
      sessionId,
      status: null,
      turns: 0,
      modelTurnsPrepared: 0,
      modelTurnsCompleted: 0,
      toolCallsRequested: 0,
      toolCallsCompleted: 0,
      toolCallsDenied: 0,
      transcriptCompactions: 0,
      evictedToolResults: 0,
      evictedItemCounts: {},
      candidatesEmitted: 0,
      candidateValidationsStarted: 0,
      candidateValidationsCompleted: 0,
      candidateDecisions: 0,
      acceptedCandidates: 0,
      rejectedCandidates: 0,
      publicationSkipped: 0,
      publicationSkippedBudgetExhausted: 0,
    });
  }
  return sessions.get(sessionId);
}

function writeTraceArtifacts(report) {
  const traceOutputDir = report.artifacts?.traceOutputDir;
  const framesPath = report.artifacts?.frames;
  if (!traceOutputDir || !framesPath || !fs.existsSync(framesPath)) return;
  fs.mkdirSync(traceOutputDir, { recursive: true });
  const runtime = [];
  const review = [];
  for (const line of fs.readFileSync(framesPath, "utf8").split("\n")) {
    if (!line.trim()) continue;
    const frame = JSON.parse(line);
    if (frame.method === "event.runtime") runtime.push(JSON.stringify(frame.params));
    if (frame.method === "event.review") review.push(JSON.stringify(frame.params));
  }
  fs.writeFileSync(path.join(traceOutputDir, "runtime-events.jsonl"), runtime.join("\n") + (runtime.length ? "\n" : ""));
  fs.writeFileSync(path.join(traceOutputDir, "review-events.jsonl"), review.join("\n") + (review.length ? "\n" : ""));
  fs.writeFileSync(path.join(traceOutputDir, "audit-diagnostics.json"), `${JSON.stringify(report.audit, null, 2)}\n`);
}

function readFramesJsonl(framesPath) {
  if (!framesPath || !fs.existsSync(framesPath)) return [];
  return fs
    .readFileSync(framesPath, "utf8")
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line));
}

function buildRunnerStart({
  runId,
  repo,
  baseRef,
  changedFiles,
  changedFilePaths,
  inlineDiff,
  model,
  sessions,
  maxActive,
  maxTurns,
  maxToolCalls,
  maxPromptTokens,
  maxOutputTokens,
}) {
  const modelProfileId = "production-review-oai";
  return {
    protocolVersion: "muzen.runner.v1",
    runId,
    repo,
    changedFiles: changedFilePaths,
    change: {
      kind: "pull_request",
      baseRevision: git(repo, ["rev-parse", baseRef]).trim(),
      headRevision: git(repo, ["rev-parse", "HEAD"]).trim(),
      changedFiles: changedFiles.map((file) => ({
        path: file.newPath ?? file.oldPath,
        status: file.status,
      })),
      diff: inlineDiff,
      reviewTarget: `github-pr:${path.basename(repo)}`,
    },
    model: {
      callback: false,
      defaultModelProfileId: modelProfileId,
      modelProfiles: [
        {
          id: modelProfileId,
          provider: "openai_compatible",
          apiProtocol: "responses",
          baseUrl: process.env.OPENAI_BASE_URL || undefined,
          credential: { env: "OPENAI_API_KEY" },
          model,
          maxInputTokens: maxPromptTokens,
          maxOutputTokens,
          temperature: 0,
        },
      ],
    },
    sessions: Array.from({ length: sessions }, (_, index) => ({
      id: `production-review-${index}`,
      role: roleForIndex(index),
      objective:
        "Review this pull request for actionable correctness bugs introduced by the change. Gather concrete evidence with the read-only review tools and publish only evidence-backed findings.",
      modelProfileId,
      allowedTools: reviewReadOnlyTools(),
      budget: {
        maxTurns,
        maxToolCalls,
        maxPromptTokens,
        maxOutputTokens,
      },
    })),
    limits: {
      maxActiveSessions: sessions === 0 ? Math.max(1, maxActive) : Math.max(1, Math.min(maxActive, sessions)),
      maxFileBytes: 200 * 1024,
      maxSearchMatches: 120,
    },
  };
}

function gitChangedFiles(repo, baseRef) {
  return git(repo, ["diff", "--name-status", "--find-renames", "--find-copies", `${baseRef}...HEAD`])
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const parts = line.split("\t");
      const status = parts[0];
      if (status.startsWith("R") || status.startsWith("C")) {
        return changedFile("renamed", parts[1], parts[2]);
      }
      if (status === "A") return changedFile("added", null, parts[1]);
      if (status === "D") return changedFile("deleted", parts[1], null);
      if (status === "T") return changedFile("type_changed", parts[1], parts[1]);
      return changedFile("modified", parts[1], parts[1]);
    });
}

function changedFile(status, oldPath, newPath) {
  return {
    status,
    oldPath,
    newPath,
    oldContentHash: null,
    newContentHash: null,
    isBinary: false,
    isGenerated: false,
  };
}

function parseFinalResult(stdout) {
  let finalResult = null;
  for (const line of stdout.split("\n")) {
    if (!line.trim()) continue;
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      continue;
    }
    if (event.eventType === "run_finished" && event.payload?.findings) {
      finalResult = event.payload;
    }
  }
  return finalResult;
}

function terminalDiagnosticNumber(stdout, key) {
  let total = 0;
  for (const line of stdout.split("\n")) {
    if (!line.trim()) continue;
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      continue;
    }
    const value = event.payload?.diagnostic?.[key] ?? event.payload?.[key];
    if (Number.isFinite(value)) total += value;
  }
  return total;
}

function scoreFindings(findings, issues) {
  const matchedFindingIndexes = new Set();
  const hits = [];
  const misses = [];
  for (const issue of issues) {
    const match = findMatchingFindingIndex(findings, issue, matchedFindingIndexes);
    if (match >= 0) {
      matchedFindingIndexes.add(match);
      hits.push({ issueId: issue.id, findingId: findings[match].id, title: findings[match].title });
    } else {
      misses.push({ issueId: issue.id, path: issue.path, title: issue.title });
    }
  }
  return {
    hitRate: issues.length === 0 ? null : hits.length / issues.length,
    hits,
    misses,
    falsePositiveCount: Math.max(0, findings.length - matchedFindingIndexes.size),
  };
}

function findMatchingFindingIndex(findings, issue, matchedFindingIndexes) {
  for (let index = 0; index < findings.length; index += 1) {
    const finding = findings[index];
    const paths = findingPathsOf(finding);
    if (matchedFindingIndexes.has(index) && paths.length <= 1) continue;
    if (issueMatchesFinding(issue, finding)) return index;
  }
  return -1;
}

function issueMatchesFinding(issue, finding) {
  const findingPaths = findingPathsOf(finding);
  if (issue.path && !findingPaths.includes(issue.path)) return false;
  const range = findingLineRangeOf(finding);
  if (issue.startLine && issue.endLine && range) {
    const overlaps =
      range.startLine <= issue.endLine &&
      range.endLine >= issue.startLine;
    if (!overlaps && issue.requireLineOverlap !== false) return false;
  }
  const text = `${finding.title || ""}\n${finding.claim || ""}`.toLowerCase();
  const keywords = issue.keywords || [];
  // Each entry is either a single required keyword or an array of accepted
  // alternatives, so goldens can pin semantics without pinning phrasing.
  return keywords.every((keyword) => {
    const alternatives = Array.isArray(keyword) ? keyword : [keyword];
    return alternatives.some((alternative) => text.includes(String(alternative).toLowerCase()));
  });
}

function findingPathsOf(finding) {
  const paths = [];
  const primary = findingPathOf(finding);
  if (primary) paths.push(primary);
  for (const relatedPath of finding.relatedPaths || []) {
    if (relatedPath && !paths.includes(relatedPath)) paths.push(relatedPath);
  }
  return paths;
}

function findingPathOf(finding) {
  if (finding.location?.path) return finding.location.path;
  const firstRef = finding.fileRefs?.[0];
  if (firstRef?.locationKind === "single_path") return firstRef.path;
  if (firstRef?.locationKind === "rename") return firstRef.newPath;
  const firstEvidence = finding.evidence?.[0]?.location;
  if (firstEvidence?.locationKind === "single_path") return firstEvidence.path;
  if (firstEvidence?.locationKind === "rename") return firstEvidence.newPath;
  return null;
}

function findingLineRangeOf(finding) {
  if (finding.location?.startLine || finding.location?.endLine) {
    return {
      startLine: finding.location.startLine ?? finding.location.endLine,
      endLine: finding.location.endLine ?? finding.location.startLine,
    };
  }
  if (finding.locationLineRange) return finding.locationLineRange;
  return null;
}

function reviewReadOnlyTools() {
  return {
    listChangedFiles: true,
    readDiff: true,
    listFiles: true,
    readFile: true,
    readFileRange: true,
    readBaseFile: true,
    readHeadFile: true,
    searchText: true,
    findRelatedFiles: true,
    findTestsForFile: true,
    listImports: true,
  };
}

function roleForIndex(index) {
  return ["correctness", "security", "performance", "maintainability", "architecture", "validator"][
    index % 6
  ];
}

function git(repo, args) {
  const result = spawnSync("git", args, { cwd: repo, encoding: "utf8", maxBuffer: 1024 * 1024 * 64 });
  if (result.status !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr || result.stdout}`);
  }
  return result.stdout;
}

function parseArgs(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith("--")) throw new Error(`unexpected argument: ${arg}`);
    const key = arg.slice(2).replace(/-([a-z])/g, (_, char) => char.toUpperCase());
    const value = argv[index + 1];
    if (!value || value.startsWith("--")) throw new Error(`missing value for ${arg}`);
    args[key] = value;
    index += 1;
  }
  return args;
}

function numberArg(value, fallback) {
  if (value == null) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 1) throw new Error(`invalid positive integer: ${value}`);
  return parsed;
}

function nonnegativeNumberArg(value, fallback) {
  if (value == null) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) throw new Error(`invalid non-negative integer: ${value}`);
  return parsed;
}

function required(value, message) {
  if (!value) throw new Error(message);
  return value;
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function trim(text) {
  return text.length > 4000 ? `${text.slice(0, 4000)}...` : text;
}

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, "").replace(/\..+/, "Z");
}

if (isMainModule()) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.stack : String(error));
    process.exitCode = 1;
  });
}

function isMainModule() {
  return process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
}
