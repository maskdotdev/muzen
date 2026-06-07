#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const SCHEMA_VERSION = "muzen.review-quality-benchmark.v1";
const JOB_SCHEMA_VERSION = "heimdaal.review-run.v1";
const DEFAULT_MODEL = process.env.MODEL || "gpt-4o-mini";

function main() {
  const args = parseArgs(process.argv.slice(2));
  const repo = path.resolve(args.repo || ".");
  const runnerPath = path.resolve(args.runnerPath || "target/release/muzen");
  const outputPath = args.output ? path.resolve(args.output) : null;
  const baseRef = required(args.baseRef, "--base-ref is required");
  const goldenPath = args.golden ? path.resolve(args.golden) : null;
  const changedFiles = gitChangedFiles(repo, baseRef);
  const inlineDiff = git(repo, ["diff", "--find-renames", "--find-copies", `${baseRef}...HEAD`]);
  const runId = args.runId || `review-quality-${timestamp()}`;
  const job = buildReviewJob({
    runId,
    repo,
    baseRef,
    changedFiles,
    inlineDiff,
    model: args.model || DEFAULT_MODEL,
    sessions: numberArg(args.sessions, 1),
    maxActive: numberArg(args.maxActive, 1),
    maxTurns: numberArg(args.maxTurns, 7),
    maxToolCalls: numberArg(args.maxToolCalls, 14),
    maxOutputTokens: numberArg(args.maxOutputTokens, 8000),
  });

  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "muzen-review-quality-"));
  const jobPath = path.join(tempDir, "job.json");
  const eventLogPath = path.join(tempDir, "events.jsonl");
  fs.writeFileSync(jobPath, `${JSON.stringify(job, null, 2)}\n`);

  const startedAt = Date.now();
  const run = spawnSync(runnerPath, ["run", "--job", jobPath], {
    cwd: repo,
    encoding: "utf8",
    env: process.env,
    maxBuffer: 1024 * 1024 * 256,
  });
  const elapsedMs = Date.now() - startedAt;
  fs.writeFileSync(eventLogPath, run.stdout || "");

  const finalResult = parseFinalResult(run.stdout || "");
  const golden = goldenPath ? readJson(goldenPath) : { issues: [] };
  const scoring = scoreFindings(finalResult?.findings || [], golden.issues || []);
  const report = {
    schemaVersion: SCHEMA_VERSION,
    generatedAtUtc: new Date().toISOString(),
    mode: "production-review-run",
    reviewValid: run.status === 0 && Boolean(finalResult),
    exitCode: run.status,
    error: run.status === 0 ? null : trim(run.stderr || run.stdout || "review command failed"),
    inputs: {
      repo,
      baseRef,
      runnerPath,
      model: args.model || DEFAULT_MODEL,
      changedFileCount: changedFiles.length,
      goldenIssueCount: golden.issues?.length || 0,
    },
    artifacts: {
      job: jobPath,
      events: eventLogPath,
    },
    review: finalResult
      ? {
          outcome: finalResult.outcome,
          publishability: finalResult.publishability,
          sessions: finalResult.sessions,
          completedSessions: finalResult.completedSessions,
          fileReviews: finalResult.fileReviews?.length || 0,
          findings: finalResult.findings?.length || 0,
          modelCalls: finalResult.modelCalls,
          toolCounts: finalResult.toolCounts,
          tokens: finalResult.tokens,
          artifactStats: finalResult.artifactStats,
          elapsedMs: finalResult.elapsedMs,
        }
      : null,
    benchmark: {
      elapsedMs,
      hitRate: scoring.hitRate,
      hits: scoring.hits,
      misses: scoring.misses,
      falsePositiveCount: scoring.falsePositiveCount,
      candidateCount: terminalDiagnosticNumber(run.stdout || "", "candidateFindings"),
      rescuedCandidateCount: terminalDiagnosticNumber(run.stdout || "", "rescuedCandidates"),
      rejectedCandidateCount: terminalDiagnosticNumber(run.stdout || "", "rejectedCandidates"),
    },
    findings: finalResult?.findings || [],
  };

  if (outputPath) {
    fs.mkdirSync(path.dirname(outputPath), { recursive: true });
    fs.writeFileSync(outputPath, `${JSON.stringify(report, null, 2)}\n`);
  }
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  process.exitCode = report.reviewValid ? 0 : 1;
}

function buildReviewJob({
  runId,
  repo,
  baseRef,
  changedFiles,
  inlineDiff,
  model,
  sessions,
  maxActive,
  maxTurns,
  maxToolCalls,
  maxOutputTokens,
}) {
  const modelProfileId = "production-review-oai";
  return {
    schemaVersion: JOB_SCHEMA_VERSION,
    runId,
    projectId: "muzen-review-quality",
    attempt: 1,
    idempotencyKey: `${runId}-1`,
    deadlineUtc: null,
    repo: {
      provider: "local",
      repoId: path.basename(repo),
      repoRoot: repo,
      worktreeRoot: repo,
      defaultCwd: ".",
      materializationId: `${runId}-materialization`,
      materializedAtUtc: new Date().toISOString(),
      materializationDigest: null,
    },
    change: {
      kind: "pull_request",
      changeId: runId,
      sourceRef: "HEAD",
      targetRef: baseRef,
      baseRevisionId: git(repo, ["rev-parse", baseRef]).trim(),
      headRevisionId: git(repo, ["rev-parse", "HEAD"]).trim(),
      mergeBaseRevisionId: git(repo, ["merge-base", baseRef, "HEAD"]).trim(),
      changedFilesManifestRef: null,
      diffManifestRef: null,
      inlineDiff,
      snapshotMode: "worktree_head",
      renameDetection: "none",
      changedFiles,
    },
    modelProfiles: [
      {
        id: modelProfileId,
        providerKind: "openai_compatible",
        apiProtocol: "chat_completions",
        providerProfileId: "env-openai-compatible",
        credentialRef: "env:OPENAI_API_KEY",
        model,
        maxInputTokens: 32000,
        maxOutputTokens,
        toolCallingMode: "auto",
        temperature: 0,
        topP: null,
      },
    ],
    defaultModelProfileId: modelProfileId,
    personas: Array.from({ length: sessions }, (_, index) => ({
      id: `production-review-${index}`,
      role: roleForIndex(index),
      objective:
        "Review this production-materialized pull request for actionable correctness bugs introduced by the change. Gather concrete evidence with the read-only review tools and publish only evidence-backed findings.",
      cwd: ".",
      modelProfileId,
      allowedTools: reviewReadOnlyTools(),
      budget: {
        maxTurns,
        maxToolCalls,
        maxPromptTokens: 32000,
        maxOutputTokens,
      },
    })),
    pathPolicy: {
      allowedRoots: ["."],
      deniedGlobs: [".git", "node_modules", "target", ".venv", "dist", "build", ".next"],
      allowedGlobs: null,
      allowDotGit: false,
      followSymlinks: false,
      maxFileBytes: 200 * 1024,
      maxDiffBytes: 2 * 1024 * 1024,
      maxSearchResults: 120,
      maxDirectoryEntries: 5000,
    },
    scratchPolicy: {
      scratchRoot: null,
      outputRoot: null,
      maxScratchBytes: 0,
      cleanupOnFinish: true,
    },
    modelVisibility: {
      maxPromptArtifactBytes: 1200,
      allowFullFileContentInPrompts: false,
      denyGlobs: [".git"],
      redactSecretLikeContent: true,
    },
    outputRedaction: {
      policyId: "review-quality-redaction-v1",
      redactRepoSecrets: true,
      persistFullFileContents: false,
    },
    budgets: {
      maxActiveSessions: Math.max(1, Math.min(maxActive, sessions)),
      maxWallTimeMs: 600000,
      maxModelCalls: sessions * maxTurns + 2,
      maxToolCalls: sessions * maxToolCalls,
      maxPromptTokens: 2000000,
      maxOutputTokens: 2000000,
      maxArtifactBytes: 64 * 1024 * 1024,
      maxScratchBytes: 0,
      rssTargetMb: null,
      rssLimitMb: null,
    },
    telemetry: {
      emitDebugEvents: false,
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
    const match = findings.findIndex((finding, index) => {
      if (matchedFindingIndexes.has(index)) return false;
      return issueMatchesFinding(issue, finding);
    });
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

function issueMatchesFinding(issue, finding) {
  const findingPath = findingPathOf(finding);
  if (issue.path && findingPath !== issue.path) return false;
  if (issue.startLine && issue.endLine && finding.locationLineRange) {
    const overlaps =
      finding.locationLineRange.startLine <= issue.endLine &&
      finding.locationLineRange.endLine >= issue.startLine;
    if (!overlaps && issue.requireLineOverlap !== false) return false;
  }
  const text = `${finding.title || ""}\n${finding.claim || ""}`.toLowerCase();
  const keywords = issue.keywords || [];
  return keywords.every((keyword) => text.includes(String(keyword).toLowerCase()));
}

function findingPathOf(finding) {
  const firstRef = finding.fileRefs?.[0];
  if (firstRef?.locationKind === "single_path") return firstRef.path;
  if (firstRef?.locationKind === "rename") return firstRef.newPath;
  const firstEvidence = finding.evidence?.[0]?.location;
  if (firstEvidence?.locationKind === "single_path") return firstEvidence.path;
  if (firstEvidence?.locationKind === "rename") return firstEvidence.newPath;
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
    recordFileReview: true,
    recordFinding: true,
    challengeFinding: true,
    finish: true,
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

main();
