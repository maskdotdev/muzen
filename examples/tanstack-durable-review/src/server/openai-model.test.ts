import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import {
  createDemoReviewModel,
  createOpenAIModelPreflightRequest,
  modelPreflightErrorMessage,
} from "./openai-model.js";

const savedEnv = {
  OPENAI_API_KEY: process.env.OPENAI_API_KEY,
  OAI_API_KEY: process.env.OAI_API_KEY,
  OPENAI_BASE_URL: process.env.OPENAI_BASE_URL,
  OPENAI_MAX_OUTPUT_TOKENS: process.env.OPENAI_MAX_OUTPUT_TOKENS,
  OPENAI_MODEL: process.env.OPENAI_MODEL,
};

afterEach(() => {
  for (const key of Object.keys(savedEnv) as Array<keyof typeof savedEnv>) {
    const value = savedEnv[key];
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  }
});

test("creates an OpenAI model config for Muzen core", () => {
  process.env.OPENAI_API_KEY = "sk-test";
  process.env.OPENAI_MODEL = "gpt-5.4-mini";
  process.env.OPENAI_BASE_URL = "https://models.example.test/v1";
  process.env.OPENAI_MAX_OUTPUT_TOKENS = "4096";

  const model = createDemoReviewModel();

  assert.equal(model.label, "OpenAI gpt-5.4-mini");
  assert.deepEqual(model.model, {
    kind: "provider",
    provider: "openai",
    model: "gpt-5.4-mini",
    credential: { env: "OPENAI_API_KEY" },
    baseUrl: "https://models.example.test/v1",
    apiProtocol: "responses",
    maxInputTokens: undefined,
    maxOutputTokens: 4096,
    temperature: undefined,
    topP: undefined,
  });
  assert.deepEqual(model.metadata, {
    modelMode: "openai",
    modelProvider: "openai",
    model: "gpt-5.4-mini",
  });
});

test("requires an OpenAI key and model", () => {
  delete process.env.OPENAI_API_KEY;
  delete process.env.OAI_API_KEY;
  delete process.env.OPENAI_MODEL;

  assert.throws(() => createDemoReviewModel(), /OPENAI_MODEL is required/);

  process.env.OPENAI_MODEL = "gpt-5.4-mini";
  assert.throws(
    () => createDemoReviewModel(),
    /OPENAI_API_KEY is required/,
  );
});

test("does not accept alternate provider key env names", () => {
  delete process.env.OPENAI_API_KEY;
  process.env.OAI_API_KEY = "sk-test";
  process.env.OPENAI_MODEL = "gpt-5.4-mini";

  assert.throws(() => createDemoReviewModel(), /OPENAI_API_KEY is required/);
});

test("creates a lightweight OpenAI model preflight request", () => {
  process.env.OPENAI_API_KEY = "sk-test";
  process.env.OPENAI_MODEL = "gpt-5.4-mini";
  process.env.OPENAI_BASE_URL = "https://models.example.test/v1/";

  const request = createOpenAIModelPreflightRequest();

  assert.equal(request.url, "https://models.example.test/v1/responses");
  assert.equal(request.headers.Authorization, "Bearer sk-test");
  assert.deepEqual(request.body, {
    model: "gpt-5.4-mini",
    input: "Return exactly OK.",
    max_output_tokens: 16,
  });
});

test("formats model preflight failures with provider and model", () => {
  assert.equal(
    modelPreflightErrorMessage({
      ok: false,
      provider: "openai",
      model: "gpt-5.4-mini",
      status: 429,
      error: "insufficient_quota",
    }),
    "Model preflight failed for openai gpt-5.4-mini (status 429): insufficient_quota",
  );
});
