import assert from "node:assert/strict";
import test from "node:test";

import { applyEnvContent, clearDemoEnv, parseEnvContent } from "./env.js";

test("parses simple dotenv content", () => {
  assert.deepEqual(
    parseEnvContent(`
      # ignored
      OPENAI_API_KEY=secret
      export OPENAI_MODEL="gpt-5.4-mini"
      OPENAI_MAX_OUTPUT_TOKENS='4096'
    `),
    [
      ["OPENAI_API_KEY", "secret"],
      ["OPENAI_MODEL", "gpt-5.4-mini"],
      ["OPENAI_MAX_OUTPUT_TOKENS", "4096"],
    ],
  );
});

test("ignores malformed dotenv lines", () => {
  assert.deepEqual(
    parseEnvContent(`
      =missing
      1BAD=value
      GOOD=value
    `),
    [["GOOD", "value"]],
  );
});

test("clears shell provider env before applying dotenv content", () => {
  const env: Record<string, string | undefined> = {
    OPENAI_API_KEY: "sk-shell",
    OPENAI_MODEL: "shell-model",
    GITHUB_TOKEN: "github-shell",
    MUZEN_RUNNER_PATH: "../../target/debug/muzen-runner",
  };

  clearDemoEnv(env);
  applyEnvContent(
    `
      OPENAI_API_KEY=sk-dotenv
      OPENAI_MODEL=gpt-5.4-mini
    `,
    env,
  );

  assert.equal(env.OPENAI_API_KEY, "sk-dotenv");
  assert.equal(env.OPENAI_MODEL, "gpt-5.4-mini");
  assert.equal(env.GITHUB_TOKEN, undefined);
  assert.equal(env.MUZEN_RUNNER_PATH, "../../target/debug/muzen-runner");
});
