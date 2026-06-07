import { describe, it } from "node:test";
import assert from "node:assert/strict";

import {
  mapRunnerResult,
  mapSwarmResult,
  toRunnerStartParams,
  toSwarmStartParams,
} from "./runner-mapping.js";
import { anthropic, openai } from "./models.js";
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
        contextEngine: {
          mode: "snapshot_v0",
          maxIndexedFiles: 20_000,
          maxIndexedBytes: 67_108_864,
          maxEvidenceItems: 5_000,
          maxPackTokens: 12_000,
          maxQueryResults: 120,
          includeRepositoryGuidance: true,
          includeHostContext: false,
          strictEvidenceRequired: true,
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
    assert.deepEqual(params.contextEngine, {
      mode: "snapshot_v0",
      maxIndexedFiles: 20_000,
      maxIndexedBytes: 67_108_864,
      maxEvidenceItems: 5_000,
      maxPackTokens: 12_000,
      maxQueryResults: 120,
      includeRepositoryGuidance: true,
      includeHostContext: false,
      strictEvidenceRequired: true,
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

  it("does not approve failed runner results with no findings", () => {
    const result = mapRunnerResult("review-1", local("/repo"), {
      runId: "runner-1",
      status: "partial",
      summary: {
        sessions: 2,
        completedSessions: 0,
        modelCalls: 2,
        toolCalls: 0,
        totalTokens: 0,
      },
      findings: [],
      snapshots: [{ files: 2, capturedFiles: 2 }],
      metadata: {},
    });

    assert.equal(result.status, "failed");
    assert.equal(result.conclusion, "changes_requested");
  });

  it("maps OpenAI hosted models into runner model profiles", () => {
    const params = toRunnerStartParams(
      "review-1",
      local("/repo", { changedFiles: ["src/auth.ts"] }),
      {
        model: openai({
          model: "gpt-5.4-mini",
          credential: { env: "OPENAI_API_KEY" },
          maxOutputTokens: 4096,
        }),
        sessions: [
          {
            id: "generalist",
            role: "generalist",
            objective: "Review the change.",
          },
          {
            id: "security",
            role: "security",
            objective: "Review security risk.",
            model: openai({
              model: "gpt-5.4",
              credential: { secretRef: "tenant:acme/openai" },
            }),
          },
        ],
      },
    ) as {
      model: {
        defaultModelProfileId: string;
        modelProfiles: Array<Record<string, unknown>>;
      };
      sessions: Array<Record<string, unknown>>;
    };

    assert.equal(params.model.defaultModelProfileId, "default");
    assert.deepEqual(params.model.modelProfiles, [
      {
        id: "default",
        provider: "openai",
        model: "gpt-5.4-mini",
        credential: { env: "OPENAI_API_KEY" },
        baseUrl: undefined,
        apiProtocol: "responses",
        maxInputTokens: undefined,
        maxOutputTokens: 4096,
        temperature: undefined,
        topP: undefined,
      },
      {
        id: "session:security",
        provider: "openai",
        model: "gpt-5.4",
        credential: { secretRef: "tenant:acme/openai" },
        baseUrl: undefined,
        apiProtocol: "responses",
        maxInputTokens: undefined,
        maxOutputTokens: undefined,
        temperature: undefined,
        topP: undefined,
      },
    ]);
    assert.equal(params.sessions[0].modelProfileId, undefined);
    assert.equal(params.sessions[1].modelProfileId, "session:security");
  });

  it("maps mixed Anthropic and OpenAI models into one profile set", () => {
    const params = toRunnerStartParams(
      "review-1",
      local("/repo", { changedFiles: ["src/auth.ts"] }),
      {
        model: anthropic({ model: "claude-opus-4-8" }),
        sessions: [
          {
            id: "generalist",
            role: "generalist",
            objective: "Review the change.",
          },
          {
            id: "security",
            role: "security",
            objective: "Review security risk.",
            model: openai({
              model: "qwen3-coder",
              baseUrl: "http://127.0.0.1:8000/v1",
              credential: { env: "VLLM_API_KEY" },
            }),
          },
        ],
      },
    ) as {
      model: {
        defaultModelProfileId: string;
        modelProfiles: Array<Record<string, unknown>>;
      };
      sessions: Array<Record<string, unknown>>;
    };

    assert.equal(params.model.defaultModelProfileId, "default");
    assert.deepEqual(params.model.modelProfiles[0], {
      id: "default",
      provider: "anthropic",
      model: "claude-opus-4-8",
      credential: { env: "ANTHROPIC_API_KEY" },
      baseUrl: undefined,
      apiProtocol: "messages",
      maxInputTokens: undefined,
      maxOutputTokens: undefined,
      temperature: undefined,
      topP: undefined,
    });
    assert.equal(params.model.modelProfiles[1].provider, "openai");
    assert.equal(
      params.model.modelProfiles[1].baseUrl,
      "http://127.0.0.1:8000/v1",
    );
    assert.equal(params.sessions[1].modelProfileId, "session:security");
  });

  it("rejects hosted session overrides with callback run models", () => {
    assert.throws(
      () =>
        toRunnerStartParams("review-1", local("/repo"), {
          model: { kind: "callback", handler: () => ({ content: "done" }) },
          sessions: [
            {
              id: "security",
              role: "security",
              objective: "Review security risk.",
              model: openai({ model: "gpt-5.4-mini" }),
            },
          ],
        }),
      /hosted session model overrides cannot be mixed/,
    );
  });

  it("maps swarm options to direct-sessions runner params", () => {
    const params = toSwarmStartParams("swarm-1", {
      repo: "/repo",
      files: ["src/auth.ts"],
      model: openai({ model: "gpt-5.4-mini" }),
      metadata: { hostRunId: "swarm-host-1" },
      agents: [
        {
          id: "planner",
          objective: "Plan the migration.",
          instructions: [
            {
              kind: "session_objective",
              text: "Prefer small steps.",
              trusted: true,
            },
          ],
          toolGrants: ["read_file"],
        },
        {
          id: "implementer",
          objective: "Implement the migration.",
          model: openai({ model: "gpt-5.4" }),
        },
      ],
    }) as {
      mode: string;
      repo: string;
      changedFiles: string[];
      metadata: Record<string, unknown>;
      sessions: Array<Record<string, unknown>>;
      model: {
        defaultModelProfileId: string;
        modelProfiles: Array<Record<string, unknown>>;
      };
    };

    assert.equal(params.mode, "direct_sessions");
    assert.equal(params.repo, "/repo");
    assert.deepEqual(params.changedFiles, ["src/auth.ts"]);
    assert.deepEqual(params.metadata, { hostRunId: "swarm-host-1" });
    assert.deepEqual(params.sessions[0], {
      id: "planner",
      role: "generalist",
      objective: "Plan the migration.",
      cwd: undefined,
      modelProfileId: undefined,
      instructions: [
        {
          kind: "session_objective",
          text: "Prefer small steps.",
          trusted: true,
        },
      ],
      toolGrants: ["read_file"],
      budget: undefined,
    });
    assert.equal(params.sessions[1].modelProfileId, "session:implementer");
    assert.equal(params.model.defaultModelProfileId, "default");
    assert.equal(params.model.modelProfiles.length, 2);
  });

  it("rejects swarm runs with no agents", () => {
    assert.throws(
      () => toSwarmStartParams("swarm-1", { repo: "/repo", agents: [] }),
      /requires at least one agent/,
    );
  });

  it("maps direct-session runner results to swarm results", () => {
    const result = mapSwarmResult("swarm-1", {
      runId: "runner-1",
      status: "partial",
      summary: {
        sessions: 2,
        completedSessions: 1,
        modelCalls: 3,
        toolCalls: 4,
        inputTokens: 10,
        outputTokens: 8,
        totalTokens: 18,
      },
      findings: [],
      snapshots: [],
      sessionOutputs: [
        {
          sessionId: "planner",
          status: "done",
          completed: true,
          output: "Plan complete.",
        },
        {
          sessionId: "implementer",
          status: "failed",
          completed: false,
          output: null,
        },
      ],
      metadata: { hostRunId: "swarm-host-1" },
    });

    assert.equal(result.runId, "swarm-1");
    assert.equal(result.status, "partial");
    assert.deepEqual(result.usage, {
      agents: 2,
      completedAgents: 1,
      modelCalls: 3,
      toolCalls: 4,
      inputTokens: 10,
      outputTokens: 8,
      totalTokens: 18,
    });
    assert.deepEqual(result.outputs, [
      {
        agentId: "planner",
        status: "done",
        completed: true,
        output: "Plan complete.",
      },
      {
        agentId: "implementer",
        status: "failed",
        completed: false,
        output: undefined,
      },
    ]);
    assert.deepEqual(result.metadata, { hostRunId: "swarm-host-1" });
  });
});
