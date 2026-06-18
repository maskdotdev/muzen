import { describe, it } from "node:test";
import assert from "node:assert/strict";

import {
  github,
  gitlab,
  customSource,
  local,
  parseReviewSource,
  perforce,
  rawSnapshot,
  sourceKey,
} from "./index.js";

describe("review sources", () => {
  it("parses GitHub pull request shorthand", () => {
    const source = parseReviewSource("github:maskdotdev/heimdaal#123");

    assert.deepEqual(source, {
      type: "github_pull_request",
      owner: "maskdotdev",
      repo: "heimdaal",
      number: 123,
    });
    assert.equal(sourceKey(source), "github:maskdotdev/heimdaal#123");
  });

  it("parses GitLab merge request shorthand with nested owners", () => {
    const source = parseReviewSource("gitlab:platform/reviews/heimdaal!42");

    assert.deepEqual(source, {
      type: "gitlab_merge_request",
      owner: "platform/reviews",
      repo: "heimdaal",
      number: 42,
    });
    assert.equal(sourceKey(source), "gitlab:platform/reviews/heimdaal!42");
  });

  it("parses raw snapshot shorthand", () => {
    const source = parseReviewSource("raw_snapshot:/tmp/muzen-snapshot");

    assert.deepEqual(source, {
      type: "raw_snapshot",
      root: "/tmp/muzen-snapshot",
    });
    assert.equal(sourceKey(source), "raw_snapshot:/tmp/muzen-snapshot");
  });

  it("builds typed sources", () => {
    assert.equal(
      sourceKey(github.pullRequest({ owner: "maskdotdev", repo: "heimdaal", number: 1 })),
      "github:maskdotdev/heimdaal#1",
    );
    assert.equal(
      sourceKey(gitlab.mergeRequest({ owner: "maskdotdev", repo: "heimdaal", number: 2 })),
      "gitlab:maskdotdev/heimdaal!2",
    );
    assert.deepEqual(local("."), {
      type: "local",
      repo: ".",
    });
    assert.deepEqual(rawSnapshot("/bundle"), {
      type: "raw_snapshot",
      root: "/bundle",
    });
    assert.equal(
      sourceKey(
        perforce.changelist({
          server: "perforce.example:1666",
          changelist: 12345,
          client: "build-client",
          depotPaths: ["//depot/main/..."],
        }),
      ),
      "perforce:perforce.example:1666@12345",
    );
    assert.equal(
      sourceKey(customSource({ provider: "acme", id: "review-123" })),
      "custom:acme:review-123",
    );
  });

  it("rejects invalid review source shorthand", () => {
    assert.throws(
      () => parseReviewSource("github:maskdotdev/heimdaal"),
      /missing # review number delimiter/,
    );
  });
});
