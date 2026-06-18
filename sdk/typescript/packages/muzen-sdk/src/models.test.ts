import { describe, it } from "node:test";
import assert from "node:assert/strict";

import {
  anthropic,
  openai,
  reviewOptionsRequireSecretResolver,
  swarmOptionsRequireSecretResolver,
} from "./models.js";

describe("hosted model helpers", () => {
  it("creates an OpenAI provider model with a default env credential", () => {
    assert.deepEqual(openai({ model: " gpt-5.4-mini " }), {
      kind: "provider",
      provider: "openai",
      model: "gpt-5.4-mini",
      credential: { env: "OPENAI_API_KEY" },
      baseUrl: undefined,
      apiProtocol: "responses",
      maxInputTokens: undefined,
      maxOutputTokens: undefined,
      temperature: undefined,
      topP: undefined,
    });
  });

  it("creates an Anthropic provider model on the messages protocol", () => {
    assert.deepEqual(anthropic({ model: " claude-opus-4-8 " }), {
      kind: "provider",
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
    assert.throws(
      () => anthropic({ model: "claude-opus-4-8", apiKey: "sk-ant" } as never),
      /anthropic\(\.\.\.\) does not accept inline apiKey/,
    );
  });

  it("rejects inline credential material", () => {
    assert.throws(
      () => openai({ model: "gpt-5.4-mini", apiKey: "sk-test" } as never),
      /does not accept inline apiKey/,
    );
  });

  it("detects whether local runs need a host secret resolver", () => {
    assert.equal(
      reviewOptionsRequireSecretResolver({
        model: openai({
          model: "gpt-5.4-mini",
          credential: { secretRef: "tenant:acme/openai" },
        }),
      }),
      true,
    );
    assert.equal(
      reviewOptionsRequireSecretResolver({
        model: openai({
          model: "gpt-5.4-mini",
          credential: { env: "OPENAI_API_KEY" },
        }),
      }),
      false,
    );
    assert.equal(
      swarmOptionsRequireSecretResolver({
        repo: "/repo",
        agents: [
          {
            id: "security",
            objective: "Review security risk.",
            model: openai({
              model: "gpt-5.4-mini",
              credential: { secretRef: "tenant:acme/openai" },
            }),
          },
        ],
      }),
      true,
    );
  });
});
