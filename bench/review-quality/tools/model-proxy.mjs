#!/usr/bin/env node
// Local OpenAI-compatible logging proxy for benchmark debugging.
// Usage: node model-proxy.mjs --port 8787 --log /tmp/model-log.jsonl [--upstream https://api.openai.com]

import http from "node:http";
import fs from "node:fs";

const args = process.argv.slice(2);
function arg(name, fallback) {
  const index = args.indexOf(`--${name}`);
  return index >= 0 ? args[index + 1] : fallback;
}

const port = Number(arg("port", "8787"));
const logPath = arg("log", "/tmp/model-proxy-log.jsonl");
const upstream = (process.env.MODEL_PROXY_UPSTREAM || arg("upstream", "https://api.openai.com")).replace(/\/$/, "");

const server = http.createServer(async (req, res) => {
  const chunks = [];
  req.on("data", (chunk) => chunks.push(chunk));
  req.on("end", async () => {
    const body = Buffer.concat(chunks).toString("utf8");
    const url = `${upstream}${req.url}`;
    try {
      const response = await fetch(url, {
        method: req.method,
        headers: {
          "content-type": req.headers["content-type"] || "application/json",
          authorization: req.headers.authorization || "",
        },
        body: req.method === "GET" || req.method === "HEAD" ? undefined : body,
      });
      const responseText = await response.text();
      fs.appendFileSync(
        logPath,
        `${JSON.stringify({
          at: new Date().toISOString(),
          url: req.url,
          status: response.status,
          request: safeParse(body),
          response: safeParse(responseText),
        })}\n`,
      );
      res.writeHead(response.status, { "content-type": response.headers.get("content-type") || "application/json" });
      res.end(responseText);
    } catch (error) {
      fs.appendFileSync(
        logPath,
        `${JSON.stringify({ at: new Date().toISOString(), url: req.url, error: String(error) })}\n`,
      );
      res.writeHead(502, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: String(error) } }));
    }
  });
});

function safeParse(text) {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

server.listen(port, () => {
  process.stdout.write(`model proxy listening on :${port} -> ${upstream}, log ${logPath}\n`);
});
