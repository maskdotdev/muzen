import { describe, it } from "node:test";
import assert from "node:assert/strict";

import { openai, reviewOptionsRequireSecretResolver } from "./models.js";

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
        sessions: [
          {
            id: "security",
            role: "security",
            objective: "Review security risk.",
            model: openai({
              model: "gpt-5.4-mini",
              credential: { env: "OPENAI_API_KEY" },
            }),
          },
        ],
      }),
      false,
    );
  });
});
