import type { ReviewFinding, ReviewResult } from "./types.js";

export interface ProjectReviewCommentsOptions {
  includeUnanchored?: boolean;
}

export interface ProjectedReviewComment {
  sourceFindingId: string;
  path?: string;
  line?: number;
  startLine?: number;
  side?: string;
  body: string;
  severity: ReviewFinding["severity"];
  title: string;
  providerAnchor?: Record<string, unknown>;
}

export interface SarifProjectionOptions {
  toolName?: string;
}

export interface SarifLog {
  version: "2.1.0";
  runs: SarifRun[];
}

export interface SarifRun {
  tool: {
    driver: {
      name: string;
      rules: SarifRule[];
    };
  };
  results: SarifResult[];
}

export interface SarifRule {
  id: string;
  name: string;
  shortDescription: {
    text: string;
  };
}

export interface SarifResult {
  ruleId: string;
  level: "error" | "warning" | "note";
  message: {
    text: string;
  };
  locations?: SarifLocation[];
  properties: {
    sourceFindingId: string;
    category: ReviewFinding["category"];
    confidence?: number;
  };
}

export interface SarifLocation {
  physicalLocation: {
    artifactLocation: {
      uri: string;
    };
    region?: {
      startLine?: number;
      endLine?: number;
      startColumn?: number;
      endColumn?: number;
    };
  };
}

export function projectReviewComments(
  input: ReviewResult | readonly ReviewFinding[],
  options: ProjectReviewCommentsOptions = {},
): ProjectedReviewComment[] {
  return findingsFromInput(input)
    .flatMap((finding) => {
      const location = finding.location;
      if (!location?.path && !options.includeUnanchored) {
        return [];
      }
      return [
        {
          sourceFindingId: finding.id,
          path: location?.path,
          line: location?.endLine ?? location?.startLine,
          startLine: location?.startLine,
          side: location?.side,
          body: formatFindingMarkdown(finding),
          severity: finding.severity,
          title: finding.title,
          providerAnchor: location?.providerAnchor,
        },
      ];
    });
}

export function projectSarif(
  input: ReviewResult | readonly ReviewFinding[],
  options: SarifProjectionOptions = {},
): SarifLog {
  const findings = findingsFromInput(input);
  const rules = new Map<string, SarifRule>();
  const results = findings.map((finding) => {
    const ruleId = finding.category;
    if (!rules.has(ruleId)) {
      rules.set(ruleId, {
        id: ruleId,
        name: ruleId,
        shortDescription: { text: `${ruleId} finding` },
      });
    }
    return {
      ruleId,
      level: sarifLevel(finding.severity),
      message: { text: `${finding.title}\n\n${finding.message}` },
      locations: sarifLocations(finding),
      properties: {
        sourceFindingId: finding.id,
        category: finding.category,
        confidence: finding.confidence,
      },
    };
  });
  return {
    version: "2.1.0",
    runs: [
      {
        tool: {
          driver: {
            name: options.toolName ?? "Muzen",
            rules: [...rules.values()],
          },
        },
        results,
      },
    ],
  };
}

function findingsFromInput(
  input: ReviewResult | readonly ReviewFinding[],
): readonly ReviewFinding[] {
  return isReviewResult(input) ? input.findings : input;
}

function isReviewResult(
  input: ReviewResult | readonly ReviewFinding[],
): input is ReviewResult {
  return !Array.isArray(input);
}

function formatFindingMarkdown(finding: ReviewFinding): string {
  const lines = [
    `### ${finding.title}`,
    "",
    finding.message,
    "",
    `Severity: ${finding.severity}`,
    `Category: ${finding.category}`,
  ];
  if (typeof finding.confidence === "number") {
    lines.push(`Confidence: ${Math.round(finding.confidence * 100)}%`);
  }
  if (finding.suggestedFix?.description) {
    lines.push("", "Suggested fix:", finding.suggestedFix.description);
  }
  return lines.join("\n");
}

function sarifLevel(severity: ReviewFinding["severity"]): SarifResult["level"] {
  switch (severity) {
    case "error":
      return "error";
    case "warning":
      return "warning";
    case "info":
      return "note";
  }
}

function sarifLocations(finding: ReviewFinding): SarifLocation[] | undefined {
  const location = finding.location;
  if (!location?.path) {
    return undefined;
  }
  const region =
    location.startLine ||
    location.endLine ||
    location.startColumn ||
    location.endColumn
      ? {
          startLine: location.startLine,
          endLine: location.endLine,
          startColumn: location.startColumn,
          endColumn: location.endColumn,
        }
      : undefined;
  return [
    {
      physicalLocation: {
        artifactLocation: { uri: location.path },
        region,
      },
    },
  ];
}
