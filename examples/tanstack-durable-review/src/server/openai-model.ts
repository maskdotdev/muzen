import {
  openai,
  type ReviewOptions,
} from "@muzen/sdk";

export interface DemoReviewModel {
  label: string;
  model: ReviewOptions["model"];
  metadata: Record<string, unknown>;
}

export interface OpenAIModelPreflightResult {
  error?: string;
  model?: string;
  ok: boolean;
  provider?: string;
  status?: number;
}

export function createDemoReviewModel(): DemoReviewModel {
  const model = requiredEnv("OPENAI_MODEL");
  requiredOpenAIKey();
  const maxOutputTokens = numberFromEnv("OPENAI_MAX_OUTPUT_TOKENS");
  return {
    label: `OpenAI ${model}`,
    model: openai({
      model,
      credential: { env: "OPENAI_API_KEY" },
      baseUrl: optionalEnv("OPENAI_BASE_URL"),
      maxOutputTokens,
    }),
    metadata: {
      modelMode: "openai",
      modelProvider: "openai",
      model,
    },
  };
}

export function createOpenAIModelPreflightRequest(): {
  body: unknown;
  headers: Record<string, string>;
  url: string;
} {
  const apiKey = requiredOpenAIKey();
  const baseUrl = optionalEnv("OPENAI_BASE_URL") ?? "https://api.openai.com/v1";
  return {
    url: `${baseUrl.replace(/\/+$/, "")}/responses`,
    headers: {
      Authorization: `Bearer ${apiKey}`,
      "Content-Type": "application/json",
    },
    body: {
      model: requiredEnv("OPENAI_MODEL"),
      input: "Return exactly OK.",
      max_output_tokens: 16,
    },
  };
}

export async function runOpenAIModelPreflight(): Promise<OpenAIModelPreflightResult> {
  const request = createOpenAIModelPreflightRequest();
  try {
    const response = await fetch(request.url, {
      method: "POST",
      headers: request.headers,
      body: JSON.stringify(request.body),
    });
    if (!response.ok) {
      return {
        ok: false,
        model: requiredEnv("OPENAI_MODEL"),
        provider: "openai",
        status: response.status,
        error: truncate(await response.text(), 1_000),
      };
    }
    return {
      ok: true,
      model: requiredEnv("OPENAI_MODEL"),
      provider: "openai",
      status: response.status,
    };
  } catch (error) {
    return {
      ok: false,
      model: optionalEnv("OPENAI_MODEL"),
      provider: "openai",
      error: error instanceof Error ? error.message : String(error),
    };
  }
}

export function modelPreflightErrorMessage(
  preflight: OpenAIModelPreflightResult,
): string {
  const label = [preflight.provider, preflight.model].filter(Boolean).join(" ");
  const status = preflight.status ? `status ${preflight.status}` : "provider unavailable";
  return `Model preflight failed${label ? ` for ${label}` : ""} (${status}): ${
    preflight.error ?? "provider unavailable"
  }`;
}

function requiredOpenAIKey(): string {
  const key = optionalEnv("OPENAI_API_KEY");
  if (key) {
    return key;
  }
  throw new Error("OPENAI_API_KEY is required");
}

function requiredEnv(name: string): string {
  const value = optionalEnv(name);
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function optionalEnv(name: string): string | undefined {
  const value = process.env[name]?.trim();
  return value ? value : undefined;
}

function numberFromEnv(name: string): number | undefined {
  const value = optionalEnv(name);
  if (!value) {
    return undefined;
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`${name} must be a positive integer`);
  }
  return parsed;
}

function truncate(value: string, maxLength: number): string {
  return value.length <= maxLength ? value : `${value.slice(0, maxLength)}[truncated]`;
}
