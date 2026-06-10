#!/usr/bin/env node
// Re-score an existing benchmark result JSON against a golden file.
// Usage: node score-result.mjs --result path.json --golden golden.json

import fs from "node:fs";

const args = process.argv.slice(2);
function arg(name) {
  const index = args.indexOf(`--${name}`);
  return index >= 0 ? args[index + 1] : null;
}

const result = JSON.parse(fs.readFileSync(arg("result"), "utf8"));
const golden = JSON.parse(fs.readFileSync(arg("golden"), "utf8"));
const findings = result.findings || [];
const issues = golden.issues || [];

const matchedFindingIndexes = new Set();
const hits = [];
const misses = [];
for (const issue of issues) {
  const match = findMatchingFindingIndex(findings, issue, matchedFindingIndexes);
  if (match >= 0) {
    matchedFindingIndexes.add(match);
    hits.push({ issueId: issue.id, title: findings[match].title });
  } else {
    misses.push(issue.id);
  }
}
process.stdout.write(
  `${JSON.stringify(
    {
      hits,
      misses,
      falsePositiveCount: Math.max(0, findings.length - matchedFindingIndexes.size),
    },
    null,
    2,
  )}\n`,
);

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
    const overlaps = range.startLine <= issue.endLine && range.endLine >= issue.startLine;
    if (!overlaps && issue.requireLineOverlap !== false) return false;
  }
  const text = `${finding.title || ""}\n${finding.claim || ""}`.toLowerCase();
  const keywords = issue.keywords || [];
  return keywords.every((keyword) => {
    const alternatives = Array.isArray(keyword) ? keyword : [keyword];
    return alternatives.some((alternative) => text.includes(String(alternative).toLowerCase()));
  });
}

function findingPathsOf(finding) {
  const paths = [];
  if (finding.location?.path) paths.push(finding.location.path);
  for (const relatedPath of finding.relatedPaths || []) {
    if (relatedPath && !paths.includes(relatedPath)) paths.push(relatedPath);
  }
  return paths;
}

function findingLineRangeOf(finding) {
  if (finding.location?.startLine || finding.location?.endLine) {
    return {
      startLine: finding.location.startLine ?? finding.location.endLine,
      endLine: finding.location.endLine ?? finding.location.startLine,
    };
  }
  return null;
}
