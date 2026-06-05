import { describe, it } from "node:test";
import assert from "node:assert/strict";

import {
  github,
  gitlab,
  local,
  parseReviewSource,
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

  it("builds typed sources", () => {
    assert.equal(
      sourceKey(github.pullRequest({ owner: "maskdotdev", repo: "heimdaal", number: 1 })),
      "github:maskdotdev/heimdaal#1",
    );
    assert.equal(
      sourceKey(gitlab.mergeRequest({ owner: "maskdotdev", repo: "heimdaal", number: 2 })),
      "gitlab:maskdotdev/heimdaal!2",
    );
    assert.deepEqual(local(".", { changedFiles: ["Cargo.toml"] }), {
      type: "local",
      repo: ".",
      changedFiles: ["Cargo.toml"],
    });
  });

  it("rejects invalid review source shorthand", () => {
    assert.throws(
      () => parseReviewSource("github:maskdotdev/heimdaal"),
      /missing # review number delimiter/,
    );
  });
});
