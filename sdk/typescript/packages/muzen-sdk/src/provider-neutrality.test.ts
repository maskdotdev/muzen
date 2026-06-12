import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

import { toRunnerStartParams } from "./runner-mapping.js";
import {
  customSource,
  github,
  gitlab,
  local,
  perforce,
  rawSnapshot,
} from "./sources.js";
import type { ReviewSource } from "./types.js";

const repoRoot = join(process.cwd(), "../../../..");

describe("provider-neutral review contracts", () => {
  it("maps provider examples through the same runner start envelope", () => {
    const sources: ReviewSource[] = [
      local("/repo", { changedFiles: ["src/lib.rs"] }),
      rawSnapshot("/bundle", { changedFiles: ["src/lib.rs"] }),
      github.pullRequest({ owner: "maskdotdev", repo: "heimdaal", number: 123 }),
      gitlab.mergeRequest({ owner: "maskdotdev", repo: "heimdaal", number: 123 }),
      perforce.changelist({
        server: "perforce.example:1666",
        changelist: "12345",
        client: "review-client",
        depotPaths: ["//depot/main/..."],
      }),
      customSource({ provider: "acme", id: "review-123" }),
    ];

    for (const source of sources) {
      const params = toRunnerStartParams("review-1", source, {
        metadata: { host: "contract-test" },
        change: {
          kind: "provider_review",
          reviewTarget: "review-123",
          changedFiles: [{ path: "src/lib.rs", status: "modified" }],
        },
      }) as Record<string, unknown>;

      assert.equal(params.runId, "review-1");
      assert.deepEqual(params.metadata, { host: "contract-test" });
      assert.deepEqual(params.changedFiles, ["src/lib.rs"]);
      assert.equal((params.source as ReviewSource).type, source.type);
      assert.equal("mergeRequestIid" in params, false);
      assert.equal("sourceBaseSha" in params, false);
      assert.equal("flowRunId" in params, false);
    }
  });

  it("keeps production contracts free of Argus-owned field names", async () => {
    const violations: string[] = [];
    for (const file of providerNeutralContractFiles) {
      const text = await readFile(join(repoRoot, file), "utf8");
      for (const term of forbiddenCoreTerms) {
        if (text.includes(term)) {
          violations.push(`${file}: ${term}`);
        }
      }
      if (/\bargus\b/i.test(text)) {
        violations.push(`${file}: argus`);
      }
    }

    assert.deepEqual(violations, []);
  });
});

const providerNeutralContractFiles = [
  "fixtures/runner-schema-v1.json",
  "sdk/typescript/packages/muzen-sdk/src/types.ts",
  "sdk/typescript/packages/muzen-sdk/src/sources.ts",
  "sdk/typescript/packages/muzen-sdk/src/runner-mapping.ts",
  "sdk/typescript/packages/muzen-sdk/src/progress.ts",
  "sdk/typescript/packages/muzen-sdk/src/projections.ts",
  "src/review_session/options.rs",
  "src/review_session/source.rs",
  "src/review_session/outcome.rs",
  "src/review_session/session.rs",
  "src/runner/types.rs",
  "src/runner/schema.rs",
  "src/runner/execution.rs",
  "src/runtime/contracts.rs",
  "src/runtime/policy/mod.rs",
];

const forbiddenCoreTerms = [
  "flowRunId",
  "traceId",
  "gitlabActorUserId",
  "mergeRequestIid",
  "sourceBaseSha",
  "sourceStartSha",
  "sourceHeadSha",
  "ArgusProgressStage",
  "ArgusFinding",
  "issueContext",
  "personaTemplates",
];
