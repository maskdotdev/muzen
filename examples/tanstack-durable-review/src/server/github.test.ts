import assert from "node:assert/strict";
import test from "node:test";

import { parseGithubPullRequestInput } from "./github.js";

test("parses GitHub PR URLs", () => {
  assert.deepEqual(
    parseGithubPullRequestInput("https://github.com/maskdotdev/muzen/pull/1"),
    {
      owner: "maskdotdev",
      repo: "muzen",
      number: 1,
    },
  );
});

test("parses Muzen GitHub source keys", () => {
  assert.deepEqual(parseGithubPullRequestInput("github:maskdotdev/muzen#42"), {
    owner: "maskdotdev",
    repo: "muzen",
    number: 42,
  });
});

test("parses compact owner/repo shorthand", () => {
  assert.deepEqual(parseGithubPullRequestInput("maskdotdev/muzen#42"), {
    owner: "maskdotdev",
    repo: "muzen",
    number: 42,
  });
});

test("rejects non-GitHub URLs", () => {
  assert.throws(
    () => parseGithubPullRequestInput("https://example.com/maskdotdev/muzen/pull/1"),
    /Only github.com/,
  );
});

test("rejects malformed input", () => {
  assert.throws(
    () => parseGithubPullRequestInput("github.com/maskdotdev/muzen"),
    /GitHub PR must look like/,
  );
});
