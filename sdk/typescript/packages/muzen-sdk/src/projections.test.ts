import { describe, it } from "node:test";
import assert from "node:assert/strict";

import {
  projectReviewComments,
  projectSarif,
  type ReviewFinding,
} from "./index.js";

const finding: ReviewFinding = {
  id: "finding-1",
  severity: "error",
  category: "bug",
  title: "Unsafe unwrap",
  message: "The changed code can panic when the input is missing.",
  confidence: 0.92,
  location: {
    path: "src/lib.rs",
    revision: "head",
    startLine: 12,
    endLine: 14,
    side: "additions",
    providerAnchor: {
      gitlabLineCode: "abc",
    },
  },
  suggestedFix: {
    description: "Return an error instead of unwrapping.",
  },
};

describe("review projections", () => {
  it("projects provider-neutral findings into host review comments", () => {
    const comments = projectReviewComments([finding]);

    assert.deepEqual(comments, [
      {
        sourceFindingId: "finding-1",
        path: "src/lib.rs",
        line: 14,
        startLine: 12,
        side: "additions",
        severity: "error",
        title: "Unsafe unwrap",
        providerAnchor: {
          gitlabLineCode: "abc",
        },
        body: [
          "### Unsafe unwrap",
          "",
          "The changed code can panic when the input is missing.",
          "",
          "Severity: error",
          "Category: bug",
          "Confidence: 92%",
          "",
          "Suggested fix:",
          "Return an error instead of unwrapping.",
        ].join("\n"),
      },
    ]);
  });

  it("skips unanchored comments unless requested", () => {
    const unanchored = {
      ...finding,
      id: "finding-2",
      location: undefined,
    };

    assert.deepEqual(projectReviewComments([unanchored]), []);
    assert.equal(
      projectReviewComments([unanchored], { includeUnanchored: true })[0]
        .sourceFindingId,
      "finding-2",
    );
  });

  it("projects findings into SARIF", () => {
    const sarif = projectSarif([finding], { toolName: "Muzen Test" });

    assert.deepEqual(sarif, {
      version: "2.1.0",
      runs: [
        {
          tool: {
            driver: {
              name: "Muzen Test",
              rules: [
                {
                  id: "bug",
                  name: "bug",
                  shortDescription: { text: "bug finding" },
                },
              ],
            },
          },
          results: [
            {
              ruleId: "bug",
              level: "error",
              message: {
                text:
                  "Unsafe unwrap\n\nThe changed code can panic when the input is missing.",
              },
              locations: [
                {
                  physicalLocation: {
                    artifactLocation: { uri: "src/lib.rs" },
                    region: {
                      startLine: 12,
                      endLine: 14,
                      startColumn: undefined,
                      endColumn: undefined,
                    },
                  },
                },
              ],
              properties: {
                sourceFindingId: "finding-1",
                category: "bug",
                confidence: 0.92,
              },
            },
          ],
        },
      ],
    });
  });
});
