import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  MuzenError,
  defineAgent,
  normalizeAgentInput,
  type RunSpec,
  type SessionSpec,
} from "./agent.js";

const fixtureUrl = new URL("../../../../../fixtures/agent-interface-v1.json", import.meta.url);

async function fixture(): Promise<{ sessionSpec: SessionSpec; runSpec: RunSpec }> {
  return JSON.parse(await readFile(fixtureUrl, "utf8")) as {
    sessionSpec: SessionSpec;
    runSpec: RunSpec;
  };
}

test("shared agent contract fixture matches the TypeScript surface", async () => {
  const value = await fixture();
  const agent = defineAgent(value.sessionSpec.agent);

  assert.equal(agent.name, "builder");
  assert.equal(value.sessionSpec.models[0]?.protocol, "responses");
  assert.equal(value.runSpec.limits.maxActiveAgents, 4);
  assert.equal(value.runSpec.limits.maxInputBytes, 1_048_576);
});

test("defineAgent rejects invalid budgets with a typed error", () => {
  assert.throws(
    () =>
      defineAgent({
        name: "builder",
        instructions: [{ type: "text", text: "Build." }],
        model: "primary",
        tools: [],
        budget: {
          maxTurns: 0,
          maxToolCalls: 0,
          maxPromptTokens: 1,
          maxOutputTokens: 1,
        },
      }),
    (error: unknown) => error instanceof MuzenError && error.code === "invalid_input",
  );
});

test("plain string inputs normalize to one text block", () => {
  assert.deepEqual(normalizeAgentInput("hello"), {
    content: [{ type: "text", text: "hello" }],
  });
});
