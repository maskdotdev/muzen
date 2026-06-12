import type {
  AnthropicReviewModelSpec,
  OpenAIReviewModelSpec,
  ReviewAgentSession,
  ReviewHostedModelSpec,
  ReviewModelCredential,
  ReviewModelSpec,
  ReviewOptions,
} from "./types.js";

export interface OpenAIModelOptions {
  model: string;
  credential?: ReviewModelCredential;
  baseUrl?: string;
  apiProtocol?: "responses" | "chat_completions";
  maxInputTokens?: number;
  maxOutputTokens?: number;
  temperature?: number;
  topP?: number;
}

export interface AnthropicModelOptions {
  model: string;
  credential?: ReviewModelCredential;
  baseUrl?: string;
  maxInputTokens?: number;
  maxOutputTokens?: number;
  temperature?: number;
  topP?: number;
}

export function openai(options: OpenAIModelOptions): OpenAIReviewModelSpec {
  validateOpenAIOptions(options);
  return {
    kind: "provider",
    provider: "openai",
    model: options.model.trim(),
    credential: options.credential ?? { env: "OPENAI_API_KEY" },
    baseUrl: trimmedOptional(options.baseUrl),
    apiProtocol: options.apiProtocol ?? "responses",
    maxInputTokens: options.maxInputTokens,
    maxOutputTokens: options.maxOutputTokens,
    temperature: options.temperature,
    topP: options.topP,
  };
}

export function anthropic(
  options: AnthropicModelOptions,
): AnthropicReviewModelSpec {
  validateHostedOptions("anthropic", options);
  return {
    kind: "provider",
    provider: "anthropic",
    model: options.model.trim(),
    credential: options.credential ?? { env: "ANTHROPIC_API_KEY" },
    baseUrl: trimmedOptional(options.baseUrl),
    apiProtocol: "messages",
    maxInputTokens: options.maxInputTokens,
    maxOutputTokens: options.maxOutputTokens,
    temperature: options.temperature,
    topP: options.topP,
  };
}

export function isCallbackReviewModelSpec(
  model: ReviewOptions["model"] | ReviewAgentSession["model"],
): model is Extract<ReviewModelSpec, { kind: "callback" }> {
  return typeof model === "object" && model !== null && model.kind === "callback";
}

export function isHostedReviewModelSpec(
  model: ReviewOptions["model"] | ReviewAgentSession["model"],
): model is ReviewHostedModelSpec {
  return typeof model === "object" && model !== null && model.kind === "provider";
}

export function reviewOptionsRequireSecretResolver(options: ReviewOptions): boolean {
  if (modelUsesSecretRef(options.model)) {
    return true;
  }
  return (options.sessions ?? []).some((session) => modelUsesSecretRef(session.model));
}

function modelUsesSecretRef(
  model: ReviewOptions["model"] | ReviewAgentSession["model"],
): boolean {
  return (
    isHostedReviewModelSpec(model) &&
    typeof model.credential === "object" &&
    model.credential !== null &&
    "secretRef" in model.credential
  );
}

function validateOpenAIOptions(options: OpenAIModelOptions): void {
  validateHostedOptions("openai", options);
}

function validateHostedOptions(
  factory: "openai" | "anthropic",
  options: OpenAIModelOptions | AnthropicModelOptions,
): void {
  const record = options as unknown as Record<string, unknown>;
  for (const field of ["apiKey", "token", "key"]) {
    if (field in record) {
      throw new Error(
        `${factory}(...) does not accept inline ${field}; use credential: { env } or credential: { secretRef }`,
      );
    }
  }
  if (typeof options.model !== "string" || options.model.trim().length === 0) {
    throw new Error(`${factory}(...) requires a non-empty model`);
  }
  validateCredential(options.credential);
  validatePositiveInteger(factory, options.maxInputTokens, "maxInputTokens");
  validatePositiveInteger(factory, options.maxOutputTokens, "maxOutputTokens");
  validateRange(factory, options.temperature, "temperature", 0, 2);
  validateRange(factory, options.topP, "topP", 0, 1);
}

function validateCredential(credential: ReviewModelCredential | undefined): void {
  if (!credential) {
    return;
  }
  const hasEnv = "env" in credential;
  const hasSecretRef = "secretRef" in credential;
  if (hasEnv === hasSecretRef) {
    throw new Error(
      "model credential must be exactly one of { env } or { secretRef }",
    );
  }
  const value = hasEnv ? credential.env : credential.secretRef;
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error("model credential reference must be a non-empty string");
  }
}

function validatePositiveInteger(
  factory: string,
  value: number | undefined,
  field: string,
): void {
  if (value === undefined) {
    return;
  }
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new Error(`${factory}(...) ${field} must be a positive integer`);
  }
}

function validateRange(
  factory: string,
  value: number | undefined,
  field: string,
  min: number,
  max: number,
): void {
  if (value === undefined) {
    return;
  }
  if (!Number.isFinite(value) || value < min || value > max) {
    throw new Error(`${factory}(...) ${field} must be between ${min} and ${max}`);
  }
}

function trimmedOptional(value: string | undefined): string | undefined {
  const trimmed = value?.trim();
  return trimmed ? trimmed : undefined;
}
