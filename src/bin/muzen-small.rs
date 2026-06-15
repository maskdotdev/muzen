//! muzen-small — lean autonomous code reviewer demo with depth-2 orchestration.
//!
//! A lead reviewer agent runs an agentic tool-call loop (read / grep /
//! spawn_subagent) and can spawn `explore` / `verify` sub-agents.
//! Concurrency-capped, digest-only returns, full per-agent tracking.
//!
//! Materialization uses the GitHub API (anonymous; public PRs) — no giant clone.
//!
//! Model backends: real OpenAI (gpt-5.4-mini) when OPENAI_API_KEY is set, else a
//! mock that still exercises the spawn + tool-call paths (tokens = 0).
//!
//! Usage:
//!   cargo run --release --bin muzen-small -- \
//!     --pr https://github.com/calcom/cal.com/pull/14943 [--out r.json] [--mock]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

const MAX_AGENT_TURNS: usize = 8;
const SUBAGENT_CONCURRENCY: usize = 4;
const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_GREP_MATCHES: usize = 60;

const LEAD_PROMPT: &str = r#"You are the lead code reviewer. Review the PR diff below and report real, actionable bugs.

Focus on BUGS first: logic errors, off-by-one, bad conditionals, missing guards, edge cases (null/empty), race conditions, security, broken error handling. Then obvious performance and behavior changes.

Be certain. Only flag something if you are confident it is a real bug. Review only the changed code.

Tools available to you:
- read(path): read a full file from the repo for context. Diffs alone are not enough.
- grep(pattern): search the changed files for a regex.
- spawn_subagent(agent_type, task): delegate. agent_type is "explore" (investigate how existing code works / conventions) or "verify" (adversarially try to REFUTE a candidate finding before you report it). Use verify on anything you intend to flag.

Investigate first, verify before flagging, then produce your final answer as a single JSON object (and nothing else):
{"summary": string, "findings": [{"title": string, "path": string, "startLine": number, "endLine": number, "severity":"low"|"medium"|"high", "confidence":"low"|"medium"|"high", "body": string}]}"#;

const EXPLORE_PROMPT: &str = r#"You are an investigation sub-agent. Answer the lead reviewer's question about this codebase using the read/grep tools. Be concise. Return a short digest (a few sentences) of what you found — patterns, conventions, prior art, relevant control flow. No JSON, just the digest."#;

const VERIFY_PROMPT: &str = r#"You are an adversarial verification sub-agent. The lead reviewer has a candidate finding. Your job is to try to REFUTE it using read/grep. Default to skepticism: if the bug does not clearly reproduce, say so. Return a short digest: REFUTED or CONFIRMED, plus one or two sentences of reasoning and evidence."#;

// ============================================================================
// CLI
// ============================================================================

struct Args {
    owner: String,
    repo: String,
    pr: u64,
    model: String,
    out: Option<String>,
    force_mock: bool,
}

fn parse_args() -> Result<Args> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut pr_input = "https://github.com/calcom/cal.com/pull/14943".to_string();
    let mut model = "gpt-5.4-mini".to_string();
    let mut out = None;
    let mut force_mock = false;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--pr" => {
                i += 1;
                pr_input = argv
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow!("--pr needs a value"))?;
            }
            "--model" => {
                i += 1;
                model = argv
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow!("--model needs a value"))?;
            }
            "--out" => {
                i += 1;
                out = Some(
                    argv.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--out needs a value"))?,
                );
            }
            "--mock" => force_mock = true,
            other => return Err(anyhow!("unknown argument: {other}")),
        }
        i += 1;
    }
    let (owner, repo, pr) = parse_pr_source(&pr_input)?;
    Ok(Args {
        owner,
        repo,
        pr,
        model,
        out,
        force_mock,
    })
}

fn parse_pr_source(input: &str) -> Result<(String, String, u64)> {
    if let Some(rest) = input
        .strip_prefix("https://github.com/")
        .or_else(|| input.strip_prefix("http://github.com/"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            let num = parts[3]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("");
            return Ok((
                parts[0].to_string(),
                parts[1].to_string(),
                num.parse().context("PR number")?,
            ));
        }
    }
    if let Some((repo_part, num)) = input.split_once('#') {
        if let Some((owner, repo)) = repo_part.split_once('/') {
            return Ok((
                owner.to_string(),
                repo.to_string(),
                num.parse().context("PR number")?,
            ));
        }
    }
    Err(anyhow!(
        "could not parse PR source: {input} (expected a github.com PR URL or owner/repo#number)"
    ))
}

// ============================================================================
// Materialization via GitHub API
// ============================================================================

struct Material {
    repo_root: tempfile::TempDir,
    files: Vec<String>,
    unified_diff: String,
}

async fn materialize_pr(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<Material> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/pulls/{pr}/files?per_page=100");
    let files: Value = gh_json(client, &url).await.context("fetch PR files")?;
    let files = files
        .as_array()
        .ok_or_else(|| anyhow!("PR files response not an array"))?;
    let dir = tempfile::Builder::new().prefix("muzen-small-").tempdir()?;
    let mut names = Vec::new();
    let mut diff = String::new();
    for f in files {
        let name = f
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let status = f
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("modified");
        if name.is_empty() || status == "removed" {
            continue;
        }
        if let Some(patch) = f.get("patch").and_then(Value::as_str) {
            diff.push_str(&format!(
                "diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n{patch}\n\n"
            ));
        }
        if let Some(raw) = f.get("raw_url").and_then(Value::as_str) {
            if let Ok(content) = gh_text(client, raw).await {
                let p = dir.path().join(&name);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&p, content).ok();
            }
        }
        names.push(name);
    }
    if names.is_empty() {
        return Err(anyhow!(
            "no reviewable changed files for {owner}/{repo}#{pr}"
        ));
    }
    Ok(Material {
        repo_root: dir,
        files: names,
        unified_diff: diff,
    })
}

async fn gh_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    let r = client
        .get(url)
        .header("User-Agent", "muzen-small")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if !r.status().is_success() {
        return Err(anyhow!("GitHub API {url} -> {}", r.status()));
    }
    Ok(r.json().await?)
}
async fn gh_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let r = client
        .get(url)
        .header("User-Agent", "muzen-small")
        .send()
        .await?;
    if !r.status().is_success() {
        return Err(anyhow!("GitHub raw {url} -> {}", r.status()));
    }
    Ok(r.text().await?)
}

// ============================================================================
// Brain: one model step (real OpenAI or mock). Returns tool calls or final text.
// ============================================================================

#[derive(Clone)]
struct ToolCall {
    id: String,
    name: String,
    args: Value,
}
struct Step {
    tool_calls: Vec<ToolCall>,
    final_text: Option<String>,
    in_tokens: u64,
    out_tokens: u64,
}

#[async_trait]
trait Brain: Send + Sync {
    async fn step(&self, role: &str, messages: &[Value], tools: &[Value]) -> Result<Step>;
}

/// Real OpenAI Chat Completions brain.
struct OpenAiBrain {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

#[async_trait]
impl Brain for OpenAiBrain {
    async fn step(&self, _role: &str, messages: &[Value], tools: &[Value]) -> Result<Step> {
        let body = json!({ "model": self.model, "messages": messages, "tools": tools, "tool_choice": "auto", "temperature": 0 });
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .context("openai request")?;
        let status = resp.status();
        let payload: Value = resp.json().await.context("openai decode")?;
        if !status.is_success() {
            return Err(anyhow!("openai {status}: {payload}"));
        }
        let msg = &payload["choices"][0]["message"];
        let in_tokens = payload["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let out_tokens = payload["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        let mut tool_calls = Vec::new();
        if let Some(tcs) = msg["tool_calls"].as_array() {
            for tc in tcs {
                let id = tc["id"].as_str().unwrap_or("call").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let args = tc["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                tool_calls.push(ToolCall { id, name, args });
            }
        }
        let final_text = if tool_calls.is_empty() {
            Some(msg["content"].as_str().unwrap_or("").to_string())
        } else {
            None
        };
        Ok(Step {
            tool_calls,
            final_text,
            in_tokens,
            out_tokens,
        })
    }
}

/// Mock brain: scripts a lead→explore→verify→findings trajectory so the spawn
/// and tool-call instrumentation is exercised with zero API cost.
struct MockBrain {
    first_file: String,
}

#[async_trait]
impl Brain for MockBrain {
    async fn step(&self, role: &str, messages: &[Value], _tools: &[Value]) -> Result<Step> {
        let history = serde_json::to_string(messages).unwrap_or_default();
        let mk = |tcs: Vec<ToolCall>| Step {
            tool_calls: tcs,
            final_text: None,
            in_tokens: 0,
            out_tokens: 0,
        };
        let fin = |t: String| Step {
            tool_calls: vec![],
            final_text: Some(t),
            in_tokens: 0,
            out_tokens: 0,
        };
        if role == "explore" {
            return Ok(fin("[explore digest] The changed flow schedules workflow reminders; the new retry_count column is added by the migration and defaults via the schema. Existing reminders are created without an explicit retry_count.".into()));
        }
        if role == "verify" {
            return Ok(fin("[verify digest] CONFIRMED (low confidence): the migration adds retry_count without backfilling a guard on max retries in the scheduling path; a runaway-retry scenario is plausible but depends on the worker honoring the cap.".into()));
        }
        // lead
        let read_done = history.contains("\"tool_call_id\"") && history.contains("retry_count");
        let explored = history.contains("[explore digest]");
        let verified = history.contains("[verify digest]");
        if !read_done {
            return Ok(mk(vec![ToolCall {
                id: "c_read".into(),
                name: "read".into(),
                args: json!({"path": self.first_file}),
            }]));
        }
        if !explored {
            return Ok(mk(vec![ToolCall {
                id: "c_exp".into(),
                name: "spawn_subagent".into(),
                args: json!({"agent_type":"explore","task":"How are workflow reminders scheduled and is retry_count handled?"}),
            }]));
        }
        if !verified {
            return Ok(mk(vec![ToolCall {
                id: "c_ver".into(),
                name: "spawn_subagent".into(),
                args: json!({"agent_type":"verify","task":"Verify candidate finding: retry_count has no max-retry guard in the scheduling path."}),
            }]));
        }
        Ok(fin(json!({
            "summary": "[MOCK] Demonstrates depth-2 orchestration: lead read a file, spawned explore + verify, then synthesized. Set OPENAI_API_KEY for a real review.",
            "findings": [{
                "title": "retry_count added without an enforced max-retry guard",
                "path": self.first_file, "startLine": 1, "endLine": 1,
                "severity": "medium", "confidence": "low",
                "body": "Mock finding (verify sub-agent marked CONFIRMED low-confidence). Real findings require a real model."
            }]
        }).to_string()))
    }
}

// ============================================================================
// Orchestrator: depth-2 agent tree with per-agent tracking
// ============================================================================

#[derive(Default)]
struct AgentRecord {
    role: String,
    turns: u64,
    tool_calls: u64,
    in_tokens: u64,
    out_tokens: u64,
}

struct Orchestrator {
    brain: Arc<dyn Brain>,
    repo_root: PathBuf,
    files: Vec<String>,
    sem: Semaphore,
    sub_agents: AtomicU64,
    records: Mutex<Vec<AgentRecord>>,
}

impl Orchestrator {
    fn tool_schemas(allow_spawn: bool) -> Vec<Value> {
        let mut t = vec![
            json!({"type":"function","function":{"name":"read","description":"Read a full file from the repo.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
            json!({"type":"function","function":{"name":"grep","description":"Regex search across the changed files.",
                "parameters":{"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}}}),
        ];
        if allow_spawn {
            t.push(
                json!({"type":"function","function":{"name":"spawn_subagent",
                "description":"Delegate to an explore or verify sub-agent. Returns a short digest.",
                "parameters":{"type":"object","properties":{
                    "agent_type":{"type":"string","enum":["explore","verify"]},
                    "task":{"type":"string"}},"required":["agent_type","task"]}}}),
            );
        }
        t
    }

    fn read_file(&self, rel: &str) -> String {
        let p = self.repo_root.join(rel);
        if !p.starts_with(&self.repo_root) {
            return "error: path escapes repo".into();
        }
        match std::fs::read(&p) {
            Ok(b) => {
                let n = b.len().min(MAX_READ_BYTES);
                String::from_utf8_lossy(&b[..n]).to_string()
            }
            Err(_) => format!("error: cannot read {rel}"),
        }
    }

    fn grep(&self, pattern: &str) -> String {
        let re = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return format!("error: bad regex: {e}"),
        };
        let mut out = Vec::new();
        for rel in &self.files {
            if let Ok(b) = std::fs::read(self.repo_root.join(rel)) {
                for (i, line) in String::from_utf8_lossy(&b).lines().enumerate() {
                    if re.is_match(line) {
                        out.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                        if out.len() >= MAX_GREP_MATCHES {
                            return out.join("\n");
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            "no matches".into()
        } else {
            out.join("\n")
        }
    }

    /// Run one agent's tool-call loop. Returns its final text.
    async fn run_agent(
        self: &Arc<Self>,
        role: &str,
        system_prompt: &str,
        objective: &str,
        allow_spawn: bool,
    ) -> String {
        let tools = Self::tool_schemas(allow_spawn);
        let mut messages = vec![
            json!({"role":"system","content": system_prompt}),
            json!({"role":"user","content": objective}),
        ];
        let mut rec = AgentRecord {
            role: role.to_string(),
            ..Default::default()
        };
        let mut final_text = String::new();
        for _ in 0..MAX_AGENT_TURNS {
            rec.turns += 1;
            let step = match self.brain.step(role, &messages, &tools).await {
                Ok(s) => s,
                Err(e) => {
                    final_text = format!("error: model step failed: {e}");
                    break;
                }
            };
            rec.in_tokens += step.in_tokens;
            rec.out_tokens += step.out_tokens;
            if let Some(text) = step.final_text {
                final_text = text;
                break;
            }
            // Record the assistant tool-call message.
            messages.push(json!({"role":"assistant","content":Value::Null,
                "tool_calls": step.tool_calls.iter().map(|tc| json!({
                    "id": tc.id, "type":"function",
                    "function": {"name": tc.name, "arguments": tc.args.to_string()}
                })).collect::<Vec<_>>()}));
            // Split spawns (run concurrently, capped) from local tools.
            let mut spawn_tasks = Vec::new();
            let mut local_results: Vec<(String, String)> = Vec::new();
            for tc in &step.tool_calls {
                rec.tool_calls += 1;
                match tc.name.as_str() {
                    "read" => {
                        let path = tc.args.get("path").and_then(Value::as_str).unwrap_or("");
                        local_results.push((tc.id.clone(), self.read_file(path)));
                    }
                    "grep" => {
                        let pat = tc.args.get("pattern").and_then(Value::as_str).unwrap_or("");
                        local_results.push((tc.id.clone(), self.grep(pat)));
                    }
                    "spawn_subagent" if allow_spawn => {
                        let kind = tc
                            .args
                            .get("agent_type")
                            .and_then(Value::as_str)
                            .unwrap_or("explore")
                            .to_string();
                        let task = tc
                            .args
                            .get("task")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let id = tc.id.clone();
                        let me = Arc::clone(self);
                        spawn_tasks.push(async move {
                            let _permit = me.sem.acquire().await.unwrap();
                            me.sub_agents.fetch_add(1, Ordering::Relaxed);
                            let (sys, r) = if kind == "verify" {
                                (VERIFY_PROMPT, "verify")
                            } else {
                                (EXPLORE_PROMPT, "explore")
                            };
                            // depth-2: children cannot spawn.
                            let digest = me.run_agent(r, sys, &task, false).await;
                            (id, digest)
                        });
                    }
                    other => {
                        local_results.push((tc.id.clone(), format!("error: unknown tool {other}")))
                    }
                }
            }
            for (id, content) in local_results {
                messages.push(json!({"role":"tool","tool_call_id": id, "content": content}));
            }
            for (id, digest) in futures::future::join_all(spawn_tasks).await {
                messages.push(json!({"role":"tool","tool_call_id": id, "content": digest}));
            }
        }
        self.records.lock().push(rec);
        final_text
    }
}

// ============================================================================
// Resources
// ============================================================================

fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return 0;
    }
    let max = usage.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    }
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let http = reqwest::Client::builder().build()?;
    eprintln!(
        "muzen-small: materializing {}/{}#{} via GitHub API…",
        args.owner, args.repo, args.pr
    );
    let material = materialize_pr(&http, &args.owner, &args.repo, args.pr).await?;
    eprintln!(
        "  {} changed file(s), {} bytes of diff",
        material.files.len(),
        material.unified_diff.len()
    );

    // Pick brain.
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty());
    let (mode, brain): (&str, Arc<dyn Brain>) = if !args.force_mock && api_key.is_some() {
        (
            "real-openai",
            Arc::new(OpenAiBrain {
                client: http.clone(),
                base_url: std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
                api_key: api_key.unwrap(),
                model: args.model.clone(),
            }),
        )
    } else {
        (
            "mock-no-key",
            Arc::new(MockBrain {
                first_file: material.files[0].clone(),
            }),
        )
    };
    eprintln!("  model backend: {mode}  |  orchestration: depth-2 (lead + explore/verify), spawn cap {SUBAGENT_CONCURRENCY}");

    let orch = Arc::new(Orchestrator {
        brain,
        repo_root: material.repo_root.path().to_path_buf(),
        files: material.files.clone(),
        sem: Semaphore::new(SUBAGENT_CONCURRENCY),
        sub_agents: AtomicU64::new(0),
        records: Mutex::new(Vec::new()),
    });
    let objective = format!(
        "## PR {}/{} #{}\n\nUnified diff under review:\n\n{}",
        args.owner, args.repo, args.pr, material.unified_diff
    );

    eprintln!("  running orchestrated review…");
    let started = Instant::now();
    let lead_output = orch.run_agent("lead", LEAD_PROMPT, &objective, true).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    // Aggregate metrics.
    let records = orch.records.lock();
    let (mut in_tok, mut out_tok, mut tool_calls) = (0u64, 0u64, 0u64);
    let per_agent: Vec<Value> = records.iter().map(|r| {
        in_tok += r.in_tokens; out_tok += r.out_tokens; tool_calls += r.tool_calls;
        json!({"role": r.role, "turns": r.turns, "tool_calls": r.tool_calls, "input_tokens": r.in_tokens, "output_tokens": r.out_tokens})
    }).collect();
    let findings: Value =
        serde_json::from_str(&lead_output).unwrap_or_else(|_| json!({"raw": lead_output}));
    let n_findings = findings
        .get("findings")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);

    let report = json!({
        "tool": "muzen-small", "mode": mode, "orchestration": "depth-2 (lead + explore/verify)",
        "pr": format!("{}/{}#{}", args.owner, args.repo, args.pr), "model": args.model,
        "tracking": {
            "tokens": {"input": in_tok, "output": out_tok, "total": in_tok + out_tok},
            "tool_calls": tool_calls,
            "sub_agents": orch.sub_agents.load(Ordering::Relaxed),
            "agents_total": records.len(),
            "findings": n_findings,
            "per_agent": per_agent,
        },
        "resources": {
            "elapsed_ms": elapsed_ms,
            "subagent_concurrency_cap": SUBAGENT_CONCURRENCY,
            "peak_rss_bytes": peak_rss_bytes(),
            "peak_rss_mib": format!("{:.1}", peak_rss_bytes() as f64 / 1048576.0),
        },
        "changed_files": material.files,
        "findings": findings,
    });
    emit(&args, &report)?;
    eprintln!("muzen-small: done — {} sub-agents, {} agents total, {} tool calls, {} tokens, {:.1} MiB peak RSS",
        orch.sub_agents.load(Ordering::Relaxed), records.len(), tool_calls, in_tok + out_tok, peak_rss_bytes() as f64 / 1048576.0);
    Ok(())
}

fn emit(args: &Args, report: &Value) -> Result<()> {
    let rendered = serde_json::to_string_pretty(report)?;
    match &args.out {
        Some(path) => {
            std::fs::write(path, &rendered)?;
            eprintln!("  wrote report to {path}");
        }
        None => println!("{rendered}"),
    }
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: &Path) {}
