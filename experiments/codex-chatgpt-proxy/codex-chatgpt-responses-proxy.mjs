#!/usr/bin/env node

import { createHash, randomBytes } from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { Readable } from "node:stream";

const CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_ISSUER = "https://auth.openai.com";
const DEFAULT_CODEX_RESPONSES_URL = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_AUTH_FILE = path.resolve("experiments/codex-chatgpt-proxy/.auth.json");
const DEFAULT_CODEXBAR_ACCOUNTS_FILE = path.join(
  os.homedir(),
  "Library/Application Support/CodexBar/managed-codex-accounts.json",
);
const DEFAULT_PORT = 4141;
const USER_AGENT = `muzen-codex-chatgpt-proxy/0.1 (${os.platform()} ${os.release()}; ${os.arch()})`;
const REFRESH_SKEW_MS = 60_000;

const args = parseArgs(process.argv.slice(2));
const command = args._[0] || "help";

try {
  if (command === "login") await login(args);
  else if (command === "serve") await serve(args);
  else if (command === "preflight") await preflight(args);
  else if (command === "accounts") await listAccounts(args);
  else {
    printHelp();
    process.exitCode = command === "help" || args.help ? 0 : 1;
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}

async function login(args) {
  const authFile = path.resolve(args.authFile || DEFAULT_AUTH_FILE);
  const issuer = trimUrl(args.issuer || DEFAULT_ISSUER);
  const mode = args.mode || "device";
  if (mode === "browser") {
    await browserLogin({ authFile, issuer, openBrowser: args.open !== "false" });
    return;
  }
  if (mode !== "device") throw new Error("--mode must be device or browser");
  await deviceLogin({ authFile, issuer });
}

async function deviceLogin({ authFile, issuer }) {
  const response = await fetch(`${issuer}/api/accounts/deviceauth/usercode`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "User-Agent": USER_AGENT,
    },
    body: JSON.stringify({ client_id: CLIENT_ID }),
  });
  if (!response.ok) throw new Error(`device auth failed: ${response.status} ${await response.text()}`);
  const device = await response.json();
  const intervalMs = Math.max(Number.parseInt(device.interval || "5", 10) || 5, 1) * 1000;

  console.log("Open this URL and enter the code:");
  console.log(`${issuer}/codex/device`);
  console.log(String(device.user_code || ""));
  console.log("");
  console.log("Waiting for authorization...");

  while (true) {
    const tokenResponse = await fetch(`${issuer}/api/accounts/deviceauth/token`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "User-Agent": USER_AGENT,
      },
      body: JSON.stringify({
        device_auth_id: device.device_auth_id,
        user_code: device.user_code,
      }),
    });

    if (tokenResponse.ok) {
      const data = await tokenResponse.json();
      const tokens = await exchangeAuthorizationCode({
        issuer,
        code: data.authorization_code,
        redirectUri: `${issuer}/deviceauth/callback`,
        codeVerifier: data.code_verifier,
      });
      writeAuth(authFile, authFromTokenResponse(tokens));
      console.log(`Stored OAuth credentials in ${authFile}`);
      return;
    }

    if (tokenResponse.status !== 403 && tokenResponse.status !== 404) {
      throw new Error(`device token polling failed: ${tokenResponse.status} ${await tokenResponse.text()}`);
    }
    await sleep(intervalMs + 3_000);
  }
}

async function browserLogin({ authFile, issuer, openBrowser }) {
  const port = Number(process.env.MUZEN_CODEX_PROXY_OAUTH_PORT || 1456);
  const redirectUri = `http://localhost:${port}/auth/callback`;
  const pkce = await generatePkce();
  const state = base64Url(randomBytes(32));
  const url = buildAuthorizeUrl({ issuer, redirectUri, pkce, state });
  const server = http.createServer();

  const tokensPromise = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("OAuth callback timed out")), 5 * 60 * 1000);
    server.on("request", async (request, response) => {
      const requestUrl = new URL(request.url || "/", redirectUri);
      if (requestUrl.pathname !== "/auth/callback") {
        response.writeHead(404);
        response.end("Not found");
        return;
      }
      try {
        const error = requestUrl.searchParams.get("error");
        if (error) throw new Error(requestUrl.searchParams.get("error_description") || error);
        const code = requestUrl.searchParams.get("code");
        if (!code) throw new Error("Missing authorization code");
        if (requestUrl.searchParams.get("state") !== state) throw new Error("Invalid OAuth state");
        const tokens = await exchangeAuthorizationCode({
          issuer,
          code,
          redirectUri,
          codeVerifier: pkce.verifier,
        });
        response.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
        response.end("<!doctype html><title>Muzen Codex proxy</title><p>Authorization complete. You can close this window.</p>");
        clearTimeout(timer);
        resolve(tokens);
      } catch (error) {
        response.writeHead(400, { "Content-Type": "text/plain; charset=utf-8" });
        response.end(error instanceof Error ? error.message : String(error));
        clearTimeout(timer);
        reject(error);
      }
    });
  });

  await new Promise((resolve, reject) => {
    server.listen(port, "127.0.0.1", resolve);
    server.once("error", reject);
  });
  console.log("Open this URL to authorize:");
  console.log(url);
  if (openBrowser) openUrl(url);
  const tokens = await tokensPromise.finally(() => server.close());
  writeAuth(authFile, authFromTokenResponse(tokens));
  console.log(`Stored OAuth credentials in ${authFile}`);
}

async function serve(args) {
  const authFile = resolveAuthFile(args);
  const issuer = trimUrl(args.issuer || DEFAULT_ISSUER);
  const upstream = args.upstream || DEFAULT_CODEX_RESPONSES_URL;
  const host = args.host || "127.0.0.1";
  const port = Number(args.port || DEFAULT_PORT);
  const reasoningEffort = optionalReasoningEffort(args.reasoningEffort);

  const server = http.createServer(async (request, response) => {
    try {
      const requestUrl = new URL(request.url || "/", `http://${host}:${port}`);
      if (request.method === "GET" && requestUrl.pathname === "/health") {
        const auth = readAuthRecord(authFile)?.auth;
        responseJson(response, 200, {
          ok: true,
          hasAuth: Boolean(auth?.refresh),
          expiresAt: auth?.expires ? new Date(auth.expires).toISOString() : null,
        });
        return;
      }

      if (request.method !== "POST" || !["/v1/responses", "/responses"].includes(requestUrl.pathname)) {
        responseJson(response, 404, {
          error: "expected POST /v1/responses",
        });
        return;
      }

      const auth = await freshAuth({ authFile, issuer });
      const raw = await readRequestBody(request);
      const normalized = normalizeResponsesBody(raw, { reasoningEffort });
      const upstreamResponse = await fetch(upstream, {
        method: "POST",
        headers: upstreamHeaders(request.headers, auth),
        body: normalized.body,
      });

      if (!upstreamResponse.ok) {
        const upstreamText = await upstreamResponse.text();
        response.writeHead(upstreamResponse.status, responseHeaders(upstreamResponse.headers));
        response.end(upstreamText);
        return;
      }

      if (normalized.requestedStream && upstreamResponse.body) {
        response.writeHead(upstreamResponse.status, responseHeaders(upstreamResponse.headers));
        await pipeWebStream(upstreamResponse.body, response);
        return;
      }

      const upstreamText = await upstreamResponse.text();

      const collected = collectResponsesSse(upstreamText);
      if (collected) {
        responseJson(response, upstreamResponse.status, collected);
        return;
      }

      response.writeHead(upstreamResponse.status, responseHeaders(upstreamResponse.headers));
      response.end(upstreamText);
    } catch (error) {
      responseJson(response, 500, {
        error: error instanceof Error ? error.message : String(error),
      });
    }
  });

  await new Promise((resolve, reject) => {
    server.listen(port, host, resolve);
    server.once("error", reject);
  });
  console.log(`Codex ChatGPT Responses proxy listening on http://${host}:${port}/v1`);
  console.log(`Use: OPENAI_BASE_URL=http://${host}:${port}/v1 OPENAI_API_KEY=muzen-codex-proxy MODEL=gpt-5.4-mini ...`);
}

async function listAccounts(args) {
  const accounts = readCodexBarAccounts(args.codexbarAccountsFile);
  if (accounts.length === 0) {
    console.log("No CodexBar managed accounts found.");
    return;
  }
  for (const account of accounts) {
    console.log(
      `${account.email || "(no email)"}\t${account.id}\t${account.workspaceAccountID || ""}\t${account.authFile}`,
    );
  }
}

async function preflight(args) {
  const baseUrl = trimUrl(args.baseUrl || `http://127.0.0.1:${DEFAULT_PORT}/v1`);
  const model = args.model || process.env.MODEL || "gpt-5.4-mini";
  const reasoningEffort = optionalReasoningEffort(args.reasoningEffort);
  const body = {
    model,
    store: false,
    instructions: "You are a concise preflight responder.",
    input: [{
      type: "message",
      role: "user",
      content: [{ type: "input_text", text: "Return exactly OK." }],
    }],
  };
  if (reasoningEffort) body.reasoning = { effort: reasoningEffort };
  const response = await fetch(`${baseUrl}/responses`, {
    method: "POST",
    headers: {
      Authorization: "Bearer muzen-codex-proxy",
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  if (!response.ok) throw new Error(`preflight failed: ${response.status} ${text}`);
  console.log(text);
}

async function freshAuth({ authFile, issuer }) {
  const authRecord = readAuthRecord(authFile);
  if (!authRecord) throw new Error(`missing auth file ${authFile}; run login first`);
  const auth = authRecord.auth;
  if (auth.access && auth.expires && auth.expires > Date.now() + REFRESH_SKEW_MS) return auth;
  if (!auth.refresh) throw new Error("auth file is missing a refresh token");
  const refreshed = await refreshAccessToken({ issuer, refreshToken: auth.refresh });
  const next = {
    ...authFromTokenResponse(refreshed),
    refresh: refreshed.refresh_token || auth.refresh,
    accountId: extractAccountId(refreshed) || auth.accountId,
  };
  writeAuthRecord(authFile, authRecord, next, refreshed);
  return next;
}

async function exchangeAuthorizationCode({ issuer, code, redirectUri, codeVerifier }) {
  const response = await fetch(`${issuer}/oauth/token`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      code,
      redirect_uri: redirectUri,
      client_id: CLIENT_ID,
      code_verifier: codeVerifier,
    }).toString(),
  });
  if (!response.ok) throw new Error(`token exchange failed: ${response.status} ${await response.text()}`);
  return response.json();
}

async function refreshAccessToken({ issuer, refreshToken }) {
  const response = await fetch(`${issuer}/oauth/token`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: refreshToken,
      client_id: CLIENT_ID,
    }).toString(),
  });
  if (!response.ok) throw new Error(`token refresh failed: ${response.status} ${await response.text()}`);
  return response.json();
}

function authFromTokenResponse(tokens) {
  return {
    type: "oauth",
    refresh: tokens.refresh_token,
    access: tokens.access_token,
    expires: Date.now() + Number(tokens.expires_in || 3600) * 1000,
    accountId: extractAccountId(tokens),
  };
}

function upstreamHeaders(incoming, auth) {
  const headers = new Headers();
  const contentType = incoming["content-type"];
  headers.set("Content-Type", Array.isArray(contentType) ? contentType[0] : contentType || "application/json");
  headers.set("authorization", `Bearer ${auth.access}`);
  headers.set("originator", "muzen");
  headers.set("User-Agent", USER_AGENT);
  if (auth.accountId) headers.set("ChatGPT-Account-Id", auth.accountId);
  return headers;
}

function responseHeaders(headers) {
  const output = {};
  for (const [key, value] of headers.entries()) {
    if (["connection", "content-encoding", "content-length", "transfer-encoding"].includes(key.toLowerCase())) continue;
    output[key] = value;
  }
  return output;
}

function normalizeResponsesBody(raw, { reasoningEffort } = {}) {
  try {
    const value = JSON.parse(raw.toString("utf8"));
    const requestedStream = value.stream === true;
    if (value.store === undefined) value.store = false;
    value.stream = true;
    if (reasoningEffort) {
      value.reasoning = {
        ...(value.reasoning && typeof value.reasoning === "object" ? value.reasoning : {}),
        effort: reasoningEffort,
      };
    }
    delete value.max_output_tokens;
    return { body: JSON.stringify(value), requestedStream };
  } catch {
    return { body: raw, requestedStream: false };
  }
}

function pipeWebStream(body, response) {
  return new Promise((resolve, reject) => {
    const readable = Readable.fromWeb(body);
    readable.on("error", reject);
    response.on("error", reject);
    response.on("finish", resolve);
    readable.pipe(response);
  });
}

function optionalReasoningEffort(value) {
  if (value === undefined) return undefined;
  const normalized = String(value).trim();
  if (!normalized) return undefined;
  if (!["minimal", "low", "medium", "high"].includes(normalized)) {
    throw new Error("--reasoning-effort must be one of minimal, low, medium, high");
  }
  return normalized;
}

function collectResponsesSse(text) {
  if (!text.includes("event:") || !text.includes("data:")) return null;
  let latestResponse = null;
  let outputText = "";
  const outputByIndex = new Map();

  for (const block of text.split(/\n\n+/)) {
    const dataLines = block
      .split(/\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice("data:".length).trimStart());
    if (dataLines.length === 0) continue;
    const data = dataLines.join("\n");
    if (data === "[DONE]") continue;
    let event;
    try {
      event = JSON.parse(data);
    } catch {
      continue;
    }
    if (event.response) latestResponse = event.response;
    if (event.type === "response.output_text.delta" && typeof event.delta === "string") {
      outputText += event.delta;
    }
    if (
      (event.type === "response.output_item.added" || event.type === "response.output_item.done") &&
      Number.isInteger(event.output_index) &&
      event.item
    ) {
      outputByIndex.set(event.output_index, event.item);
    }
  }

  if (!latestResponse && outputByIndex.size === 0 && !outputText) return null;
  const output =
    latestResponse?.output && latestResponse.output.length > 0
      ? latestResponse.output
      : [...outputByIndex.entries()].sort(([a], [b]) => a - b).map(([, item]) => item);
  return {
    ...(latestResponse || {}),
    output,
    output_text: latestResponse?.output_text ?? (outputText || undefined),
    usage: latestResponse?.usage ?? null,
  };
}

function readRequestBody(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => resolve(Buffer.concat(chunks)));
    request.on("error", reject);
  });
}

function resolveAuthFile(args) {
  if (args.codexbarAccount) return resolveCodexBarAuthFile(args.codexbarAccount, args.codexbarAccountsFile);
  return path.resolve(args.authFile || DEFAULT_AUTH_FILE);
}

function readAuthRecord(authFile) {
  try {
    const raw = JSON.parse(fs.readFileSync(authFile, "utf8"));
    const auth = normalizeAuth(raw);
    return auth ? { raw, auth, format: raw?.auth_mode === "chatgpt" && raw?.tokens ? "codex-chatgpt" : "proxy" } : null;
  } catch {
    return null;
  }
}

function normalizeAuth(raw) {
  if (!raw || typeof raw !== "object") return null;
  if (raw.refresh || raw.access) return raw;
  const tokens = parseTokens(raw.tokens);
  if (!tokens) return null;
  return {
    type: "oauth",
    refresh: tokens.refresh_token,
    access: tokens.access_token,
    expires: tokenExpiryMs(tokens),
    accountId:
      tokens.account_id ||
      tokens.accountId ||
      extractAccountId({ id_token: tokens.id_token, access_token: tokens.access_token }),
  };
}

function parseTokens(value) {
  if (!value) return null;
  if (typeof value === "object") return value;
  if (typeof value !== "string") return null;
  try {
    return JSON.parse(value);
  } catch {
    return null;
  }
}

function tokenExpiryMs(tokens) {
  return (
    Number(tokens.expires_at || tokens.expiresAt || tokens.expiry || 0) ||
    extractExpiryMsFromJwt(tokens.access_token)
  );
}

function writeAuthRecord(authFile, record, auth, tokens) {
  if (record.format !== "codex-chatgpt") {
    writeJson0600(authFile, auth);
    return;
  }
  const existingTokens = parseTokens(record.raw.tokens) || {};
  const raw = {
    ...record.raw,
    last_refresh: new Date().toISOString(),
    tokens: {
      ...existingTokens,
      access_token: tokens.access_token,
      refresh_token: auth.refresh,
      id_token: tokens.id_token || existingTokens.id_token,
      account_id: auth.accountId,
    },
  };
  writeJson0600(authFile, raw);
}

function writeAuth(authFile, auth) {
  writeJson0600(authFile, auth);
}

function writeJson0600(authFile, value) {
  fs.mkdirSync(path.dirname(authFile), { recursive: true });
  const temp = `${authFile}.${process.pid}.tmp`;
  fs.writeFileSync(temp, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  fs.renameSync(temp, authFile);
  fs.chmodSync(authFile, 0o600);
}

function resolveCodexBarAuthFile(selector, accountsFile = DEFAULT_CODEXBAR_ACCOUNTS_FILE) {
  const normalized = String(selector).trim().toLowerCase();
  const matches = readCodexBarAccounts(accountsFile).filter((account) => {
    return [account.id, account.email, account.providerAccountID, account.workspaceAccountID]
      .filter(Boolean)
      .some((value) => String(value).toLowerCase() === normalized);
  });
  if (matches.length === 0) throw new Error(`no CodexBar account matched ${selector}; run the accounts command`);
  if (matches.length > 1) throw new Error(`multiple CodexBar accounts matched ${selector}; use the account id`);
  return matches[0].authFile;
}

function readCodexBarAccounts(accountsFile = DEFAULT_CODEXBAR_ACCOUNTS_FILE) {
  const file = path.resolve(accountsFile);
  try {
    const data = JSON.parse(fs.readFileSync(file, "utf8"));
    return (Array.isArray(data.accounts) ? data.accounts : []).map((account) => ({
      ...account,
      authFile: path.join(account.managedHomePath, "auth.json"),
    }));
  } catch {
    return [];
  }
}

async function generatePkce() {
  const verifier = base64Url(randomBytes(32));
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  return { verifier, challenge };
}

function buildAuthorizeUrl({ issuer, redirectUri, pkce, state }) {
  const params = new URLSearchParams({
    response_type: "code",
    client_id: CLIENT_ID,
    redirect_uri: redirectUri,
    scope: "openid profile email offline_access",
    code_challenge: pkce.challenge,
    code_challenge_method: "S256",
    id_token_add_organizations: "true",
    codex_cli_simplified_flow: "true",
    state,
    originator: "muzen",
  });
  return `${issuer}/oauth/authorize?${params.toString()}`;
}

function extractAccountId(tokens) {
  return extractAccountIdFromJwt(tokens.id_token) || extractAccountIdFromJwt(tokens.access_token);
}

function extractAccountIdFromJwt(token) {
  if (!token) return undefined;
  const parts = String(token).split(".");
  if (parts.length !== 3) return undefined;
  try {
    const claims = JSON.parse(Buffer.from(parts[1], "base64url").toString("utf8"));
    return (
      claims.chatgpt_account_id ||
      claims["https://api.openai.com/auth"]?.chatgpt_account_id ||
      claims.organizations?.[0]?.id
    );
  } catch {
    return undefined;
  }
}

function extractExpiryMsFromJwt(token) {
  if (!token) return undefined;
  const parts = String(token).split(".");
  if (parts.length !== 3) return undefined;
  try {
    const claims = JSON.parse(Buffer.from(parts[1], "base64url").toString("utf8"));
    return Number.isFinite(claims.exp) ? claims.exp * 1000 : undefined;
  } catch {
    return undefined;
  }
}

function responseJson(response, status, value) {
  response.writeHead(status, { "Content-Type": "application/json; charset=utf-8" });
  response.end(`${JSON.stringify(value, null, 2)}\n`);
}

function openUrl(url) {
  if (process.platform === "darwin") spawn("open", [url], { detached: true, stdio: "ignore" }).unref();
}

function trimUrl(value) {
  return String(value).replace(/\/+$/, "");
}

function base64Url(buffer) {
  return Buffer.from(buffer).toString("base64url");
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function parseArgs(raw) {
  const parsed = { _: [] };
  for (let index = 0; index < raw.length; index += 1) {
    const arg = raw[index];
    if (!arg.startsWith("--")) {
      parsed._.push(arg);
      continue;
    }
    const key = arg.slice(2).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
    if (key === "help") {
      parsed[key] = true;
      continue;
    }
    if (index + 1 >= raw.length) throw new Error(`${arg} requires a value`);
    parsed[key] = raw[++index];
  }
  return parsed;
}

function printHelp() {
  console.log(`Usage:
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs login [--mode device|browser]
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs serve [--port 4141]
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs preflight [--base-url http://127.0.0.1:4141/v1]
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs accounts

Options:
  --auth-file PATH              Token cache. Default: ${DEFAULT_AUTH_FILE}
  --codexbar-account VALUE      Serve with a CodexBar managed account email, id, or workspace account id.
  --codexbar-accounts-file PATH CodexBar account index. Default: ${DEFAULT_CODEXBAR_ACCOUNTS_FILE}
  --issuer URL                  OAuth issuer. Default: ${DEFAULT_ISSUER}
  --upstream URL                Codex Responses endpoint. Default: ${DEFAULT_CODEX_RESPONSES_URL}
  --reasoning-effort VALUE      Inject Responses reasoning effort: minimal, low, medium, or high.
`);
}
