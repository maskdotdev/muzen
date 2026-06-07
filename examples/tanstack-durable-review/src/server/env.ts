import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

type EnvTarget = Record<string, string | undefined>;

export const demoEnvAuthorityKeys = [
  "OPENAI_API_KEY",
  "OPENAI_MODEL",
  "OPENAI_BASE_URL",
  "OPENAI_MAX_OUTPUT_TOKENS",
  "GITHUB_TOKEN",
  "GITLAB_TOKEN",
  "MUZEN_REAL_PROVIDER_MODEL",
  "MUZEN_RUN_REAL_PROVIDER_CANARY",
  "MUZEN_RUN_REAL_PROVIDER_SMOKE",
] as const;

export function loadDemoEnv(env: EnvTarget = process.env): void {
  clearDemoEnv(env);
  for (const path of [
    resolve(process.cwd(), "../../.env"),
    resolve(process.cwd(), ".env"),
  ]) {
    loadEnvFile(path, env);
  }
}

export function clearDemoEnv(env: EnvTarget): void {
  for (const key of demoEnvAuthorityKeys) {
    delete env[key];
  }
}

function loadEnvFile(path: string, env: EnvTarget): void {
  if (!existsSync(path)) {
    return;
  }
  applyEnvContent(readFileSync(path, "utf8"), env);
}

export function applyEnvContent(content: string, env: EnvTarget): void {
  for (const [key, value] of parseEnvContent(content)) {
    env[key] = value;
  }
}

export function parseEnvContent(content: string): Array<[string, string]> {
  const entries: Array<[string, string]> = [];
  for (const rawLine of content.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) {
      continue;
    }
    const normalized = line.startsWith("export ") ? line.slice(7).trim() : line;
    const separator = normalized.indexOf("=");
    if (separator <= 0) {
      continue;
    }
    const key = normalized.slice(0, separator).trim();
    const rawValue = normalized.slice(separator + 1).trim();
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
      continue;
    }
    entries.push([key, unquote(rawValue)]);
  }
  return entries;
}

function unquote(value: string): string {
  if (
    (value.startsWith('"') && value.endsWith('"')) ||
    (value.startsWith("'") && value.endsWith("'"))
  ) {
    return value.slice(1, -1);
  }
  return value;
}
