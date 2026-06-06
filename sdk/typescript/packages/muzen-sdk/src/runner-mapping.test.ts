import { describe, it } from "node:test";
import assert from "node:assert/strict";

import { mapRunnerResult, toRunnerStartParams } from "./runner-mapping.js";
import { local } from "./sources.js";

describe("runner mapping", () => {
  it("maps provider-neutral review options into runner start params", () => {
    const params = toRunnerStartParams(
      "review-1",
      local("/repo", { changedFiles: ["fallback.ts"] }),
      {
        model: {
          kind: "callback",
          handler: () => ({ content: "done" }),
        },
        sourceProvider: {
          baseUrl: "https://gitlab.example.test",
          handler: () => ({ root: "/bundle", changedFiles: ["src/auth.ts"] }),
        },
        hooks: {
          onEvent: () => {},
          onHeartbeat: () => true,
        },
        heartbeat: {
          intervalMs: 250,
          leaseSeconds: 30,
        },
        metadata: {
          hostRunId: "flow-1",
        },
        change: {
          kind: "revision_range",
          baseRevision: "base",
          headRevision: "head",
          changedFiles: [{ path: "src/auth.ts", status: "modified" }],
        },
        instructions: [
          {
            kind: "host_policy",
            text: "Prefer concrete regressions.",
            trusted: true,
          },
        ],
        tools: [
          {
            id: "argus.issue_context",
            description: "Fetch linked issue context.",
            parameters: { type: "object", properties: {} },
            effects: ["read_host", "write_artifact"],
            cacheable: true,
            providerResources: ["issue:123"],
            handler: () => ({ data: { ok: true } }),
          },
        ],
        sessions: [
          {
            id: "security",
            role: "security",
            objective: "Find auth regressions.",
            instructions: [
              {
                kind: "session_objective",
                text: "Focus on token boundaries.",
                trusted: true,
              },
            ],
            toolGrants: ["argus.issue_context"],
          },
        ],
      },
    ) as Record<string, unknown>;

    assert.deepEqual(params.changedFiles, ["src/auth.ts"]);
    assert.deepEqual(params.metadata, { hostRunId: "flow-1" });
    assert.deepEqual(params.model, { callback: true });
    assert.deepEqual(params.sourceProvider, {
      baseUrl: "https://gitlab.example.test",
      callback: true,
    });
    assert.deepEqual(params.heartbeat, {
      callback: true,
      intervalMs: 250,
      leaseSeconds: 30,
    });
    assert.deepEqual(params.instructions, [
      {
        kind: "host_policy",
        text: "Prefer concrete regressions.",
        trusted: true,
      },
    ]);
    assert.deepEqual(params.tools, [
      {
        id: "argus.issue_context",
        description: "Fetch linked issue context.",
        parameters: { type: "object", properties: {} },
        effects: ["read_host", "write_artifact"],
        cacheable: true,
        providerResources: ["issue:123"],
      },
    ]);
    assert.deepEqual((params.sessions as unknown[])[0], {
      id: "security",
      role: "security",
      objective: "Find auth regressions.",
      cwd: undefined,
      modelProfileId: undefined,
      instructions: [
        {
          kind: "session_objective",
          text: "Focus on token boundaries.",
          trusted: true,
        },
      ],
      toolGrants: ["argus.issue_context"],
      budget: undefined,
    });
    assert.equal("hooks" in params, false);
  });

  it("preserves provider-neutral finding locations from runner results", () => {
    const result = mapRunnerResult(
      "review-1",
      local("/repo"),
      {
        runId: "review-1",
        status: "completed",
        summary: {
          sessions: 1,
          completedSessions: 1,
          modelCalls: 1,
          toolCalls: 2,
          totalTokens: 12,
        },
        findings: [
          {
            id: "finding-1",
            title: "Unsafe unwrap",
            claim: "The code can panic.",
            publishable: true,
            severity: "high",
            confidence: 0.81,
            validationStatus: "validated",
            evidence: [
              {
                evidenceId: "ev-1",
                artifactId: "art-1",
                kind: "file_slice",
                contentHash: "hash-1",
                producingToolCallId: "call-1",
              },
            ],
            discoveredBy: ["security"],
            validatedBy: ["call-1"],
            location: {
              path: "src/lib.rs",
              revision: "head",
              startLine: 12,
              side: "additions",
              providerAnchor: { lineCode: "abc" },
            },
          },
        ],
        snapshots: [{ files: 2, capturedFiles: 2 }],
        metadata: { hostRunId: "flow-1" },
      },
    );

    assert.equal(result.metadata?.hostRunId, "flow-1");
    assert.equal(result.findings[0].severity, "error");
    assert.equal(result.findings[0].confidence, 0.81);
    assert.equal(result.findings[0].validationStatus, "validated");
    assert.deepEqual(result.findings[0].evidence, [
      {
        evidenceId: "ev-1",
        artifactId: "art-1",
        kind: "file_slice",
        contentHash: "hash-1",
        producingToolCallId: "call-1",
      },
    ]);
    assert.deepEqual(result.findings[0].discoveredBy, ["security"]);
    assert.deepEqual(result.findings[0].validatedBy, ["call-1"]);
    assert.deepEqual(result.findings[0].location, {
      path: "src/lib.rs",
      revision: "head",
      startLine: 12,
      side: "additions",
      providerAnchor: { lineCode: "abc" },
    });
  });
});
