use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::TempDir;

use crate::review_sources::ReviewSource;
use crate::runner_protocol::{
    RunSourceProviderParams, RunnerCallbackTransport, RUNNER_PROTOCOL_VERSION,
};

const GITHUB_BASE_URL: &str = "https://github.com";
const GITHUB_API_BASE_URL: &str = "https://api.github.com";
const GITLAB_BASE_URL: &str = "https://gitlab.com";
const MATERIALIZED_HEAD_REF: &str = "refs/remotes/origin/muzen-review-head";
const MATERIALIZED_BASE_REF: &str = "refs/remotes/origin/HEAD";
const MATERIALIZED_PR_BASE_REF: &str = "refs/remotes/origin/muzen-review-base";

pub(crate) struct MaterializedRunSource {
    repo_root: PathBuf,
    changed_files: Vec<String>,
    inline_diff: Option<String>,
    _temp_dir: Option<TempDir>,
}

impl MaterializedRunSource {
    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub(crate) fn changed_files(&self) -> &[String] {
        &self.changed_files
    }

    pub(crate) fn inline_diff(&self) -> Option<&str> {
        self.inline_diff.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderCheckoutPlan {
    pub(crate) source_key: String,
    pub(crate) remote_url: String,
    pub(crate) base_refspec: String,
    pub(crate) head_refspec: String,
    pub(crate) base_ref: String,
    pub(crate) head_ref: String,
    pub(crate) auth_scope_url: String,
    pub(crate) token_env: &'static str,
}

pub(crate) fn materialize_run_source(
    repo: Option<&Path>,
    source: Option<&ReviewSource>,
    changed_files: &[String],
    provider: Option<&RunSourceProviderParams>,
    transport: Option<&Arc<dyn RunnerCallbackTransport>>,
) -> Result<MaterializedRunSource> {
    if let Some(repo) = repo {
        return Ok(MaterializedRunSource {
            repo_root: repo.to_path_buf(),
            changed_files: changed_files.to_vec(),
            inline_diff: None,
            _temp_dir: None,
        });
    }

    let Some(source) = source else {
        anyhow::bail!("run.start requires either repo or source");
    };

    if provider.is_some_and(|provider| provider.callback) {
        return materialize_callback_source(source, changed_files, transport);
    }

    match source {
        ReviewSource::Local {
            repo,
            changed_files: source_changed_files,
        } => Ok(MaterializedRunSource {
            repo_root: repo.clone(),
            changed_files: override_or_source_changed_files(changed_files, source_changed_files),
            inline_diff: None,
            _temp_dir: None,
        }),
        ReviewSource::RawSnapshot {
            root,
            changed_files: source_changed_files,
        } => Ok(MaterializedRunSource {
            repo_root: root.clone(),
            changed_files: override_or_source_changed_files(changed_files, source_changed_files),
            inline_diff: None,
            _temp_dir: None,
        }),
        ReviewSource::GithubPullRequest { .. } | ReviewSource::GitlabMergeRequest { .. } => {
            materialize_provider_source(source, changed_files, provider)
        }
        ReviewSource::PerforceChangelist { .. } | ReviewSource::Custom { .. } => {
            anyhow::bail!(
                "source {} requires sourceProvider.callback or a host-materialized repo",
                source.source_key()
            )
        }
    }
}

fn materialize_callback_source(
    source: &ReviewSource,
    changed_files: &[String],
    transport: Option<&Arc<dyn RunnerCallbackTransport>>,
) -> Result<MaterializedRunSource> {
    let transport = transport
        .ok_or_else(|| anyhow::anyhow!("sourceProvider.callback requires interactive stdio"))?;
    let params = SourceMaterializeParams {
        protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
        source: source.clone(),
        changed_files: changed_files.to_vec(),
    };
    let value = transport.request("source.materialize", json!(params))?;
    let result = serde_json::from_value::<SourceMaterializeResult>(value)
        .context("invalid source.materialize result")?;
    Ok(MaterializedRunSource {
        repo_root: result.root,
        changed_files: if result.changed_files.is_empty() {
            changed_files.to_vec()
        } else {
            result.changed_files
        },
        inline_diff: None,
        _temp_dir: None,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceMaterializeParams {
    protocol_version: String,
    source: ReviewSource,
    changed_files: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceMaterializeResult {
    root: PathBuf,
    #[serde(default)]
    changed_files: Vec<String>,
}

pub(crate) fn provider_checkout_plan(
    source: &ReviewSource,
    provider: Option<&RunSourceProviderParams>,
) -> Result<ProviderCheckoutPlan> {
    match source {
        ReviewSource::GithubPullRequest {
            owner,
            repo,
            number,
        } => {
            let base_url = provider_base_url(provider, GITHUB_BASE_URL);
            Ok(ProviderCheckoutPlan {
                source_key: source.source_key(),
                remote_url: provider_remote_url(&base_url, owner, repo),
                base_refspec: format!("+HEAD:{MATERIALIZED_BASE_REF}"),
                head_refspec: format!("+refs/pull/{number}/head:{MATERIALIZED_HEAD_REF}"),
                base_ref: MATERIALIZED_BASE_REF.to_string(),
                head_ref: MATERIALIZED_HEAD_REF.to_string(),
                auth_scope_url: base_url,
                token_env: "GITHUB_TOKEN",
            })
        }
        ReviewSource::GitlabMergeRequest {
            owner,
            repo,
            number,
        } => {
            let base_url = provider_base_url(provider, GITLAB_BASE_URL);
            Ok(ProviderCheckoutPlan {
                source_key: source.source_key(),
                remote_url: provider_remote_url(&base_url, owner, repo),
                base_refspec: format!("+HEAD:{MATERIALIZED_BASE_REF}"),
                head_refspec: format!("+refs/merge-requests/{number}/head:{MATERIALIZED_HEAD_REF}"),
                base_ref: MATERIALIZED_BASE_REF.to_string(),
                head_ref: MATERIALIZED_HEAD_REF.to_string(),
                auth_scope_url: base_url,
                token_env: "GITLAB_TOKEN",
            })
        }
        ReviewSource::Local { .. } | ReviewSource::RawSnapshot { .. } => {
            anyhow::bail!("local sources do not require provider checkout planning")
        }
        ReviewSource::PerforceChangelist { .. } | ReviewSource::Custom { .. } => {
            anyhow::bail!(
                "source {} does not support git provider checkout planning",
                source.source_key()
            )
        }
    }
}

fn materialize_provider_source(
    source: &ReviewSource,
    changed_files: &[String],
    provider: Option<&RunSourceProviderParams>,
) -> Result<MaterializedRunSource> {
    let mut plan = provider_checkout_plan(source, provider)?;
    let provider_changed_files = resolve_provider_pr_checkout(source, provider, &mut plan)?;
    let temp_dir = tempfile::Builder::new()
        .prefix("muzen-provider-")
        .tempdir()
        .context("failed to create provider checkout directory")?;
    let repo_root = temp_dir.path().to_path_buf();

    run_git(&repo_root, &["init", "."], &plan)?;
    run_git(
        &repo_root,
        &["remote", "add", "origin", &plan.remote_url],
        &plan,
    )?;
    let _ = run_git(
        &repo_root,
        &["fetch", "--depth", "64", "origin", &plan.base_refspec],
        &plan,
    );
    run_git(
        &repo_root,
        &["fetch", "--depth", "64", "origin", &plan.head_refspec],
        &plan,
    )?;
    run_git(&repo_root, &["checkout", "--detach", &plan.head_ref], &plan)?;

    let inferred_changed_files = if !changed_files.is_empty() {
        changed_files.to_vec()
    } else if !provider_changed_files.is_empty() {
        provider_changed_files
    } else {
        infer_changed_files(&repo_root, &plan)
    };
    let inline_diff = infer_inline_diff(&repo_root, &plan);

    Ok(MaterializedRunSource {
        repo_root,
        changed_files: inferred_changed_files,
        inline_diff,
        _temp_dir: Some(temp_dir),
    })
}

fn resolve_provider_pr_checkout(
    source: &ReviewSource,
    provider: Option<&RunSourceProviderParams>,
    plan: &mut ProviderCheckoutPlan,
) -> Result<Vec<String>> {
    match source {
        ReviewSource::GithubPullRequest {
            owner,
            repo,
            number,
        } => {
            if !provider_base_url(provider, GITHUB_BASE_URL).starts_with("http") {
                return Ok(Vec::new());
            }
            let metadata = fetch_github_pull_request_metadata(owner, repo, *number, provider)?;
            plan.base_refspec = format!("+{}:{MATERIALIZED_PR_BASE_REF}", metadata.base.sha.trim());
            plan.head_refspec = format!("+{}:{MATERIALIZED_HEAD_REF}", metadata.head.sha.trim());
            plan.base_ref = MATERIALIZED_PR_BASE_REF.to_string();
            plan.head_ref = MATERIALIZED_HEAD_REF.to_string();
            Ok(fetch_github_pull_request_files(
                owner, repo, *number, provider,
            )?)
        }
        _ => Ok(Vec::new()),
    }
}

#[derive(Debug, Deserialize)]
struct GithubPullRequestMetadata {
    base: GithubPullRequestRef,
    head: GithubPullRequestRef,
}

#[derive(Debug, Deserialize)]
struct GithubPullRequestRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GithubPullRequestFile {
    filename: String,
}

fn fetch_github_pull_request_metadata(
    owner: &str,
    repo: &str,
    number: u64,
    provider: Option<&RunSourceProviderParams>,
) -> Result<GithubPullRequestMetadata> {
    github_get_json(&format!("/repos/{owner}/{repo}/pulls/{number}"), provider)
        .with_context(|| format!("failed to resolve GitHub pull request {owner}/{repo}#{number}"))
}

fn fetch_github_pull_request_files(
    owner: &str,
    repo: &str,
    number: u64,
    provider: Option<&RunSourceProviderParams>,
) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for page in 1.. {
        let page_files: Vec<GithubPullRequestFile> = github_get_json(
            &format!("/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}"),
            provider,
        )
        .with_context(|| {
            format!(
                "failed to resolve changed files for GitHub pull request {owner}/{repo}#{number}"
            )
        })?;
        if page_files.is_empty() {
            break;
        }
        files.extend(page_files.into_iter().map(|file| file.filename));
    }
    Ok(files)
}

fn github_get_json<T: for<'de> Deserialize<'de>>(
    path_and_query: &str,
    provider: Option<&RunSourceProviderParams>,
) -> Result<T> {
    let base_url = provider_api_base_url(provider, GITHUB_API_BASE_URL);
    let url = format!("{base_url}{path_and_query}");
    let client = reqwest::blocking::Client::new();
    let mut request = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "muzen-runner");
    if let Some(token) = provider_token("GITHUB_TOKEN") {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .with_context(|| format!("failed to call GitHub API {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("GitHub API {url} returned {status}");
    }
    response
        .json()
        .with_context(|| format!("invalid GitHub API response from {url}"))
}

fn infer_changed_files(repo_root: &Path, plan: &ProviderCheckoutPlan) -> Vec<String> {
    let primary_range = format!("{}...HEAD", plan.base_ref);
    let fallback_range = format!("{}..HEAD", plan.base_ref);
    for range in [primary_range, fallback_range] {
        if let Ok(output) = run_git_output(
            repo_root,
            &[
                "diff",
                "--name-only",
                "-z",
                "--diff-filter=ACMRTUXB",
                &range,
            ],
            plan,
        ) {
            let files = parse_nul_delimited_paths(&output);
            if !files.is_empty() {
                return files;
            }
        }
    }
    Vec::new()
}

fn infer_inline_diff(repo_root: &Path, plan: &ProviderCheckoutPlan) -> Option<String> {
    let primary_range = format!("{}...HEAD", plan.base_ref);
    let fallback_range = format!("{}..HEAD", plan.base_ref);
    for range in [primary_range, fallback_range] {
        if let Ok(output) = run_git_output(
            repo_root,
            &["diff", "--patch", "--diff-filter=ACMRTUXB", &range],
            plan,
        ) {
            let diff = String::from_utf8_lossy(&output).to_string();
            if !diff.trim().is_empty() {
                return Some(diff);
            }
        }
    }
    None
}

fn parse_nul_delimited_paths(output: &[u8]) -> Vec<String> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .filter_map(|path| String::from_utf8(path.to_vec()).ok())
        .collect()
}

fn override_or_source_changed_files(
    changed_files: &[String],
    source_changed_files: &[String],
) -> Vec<String> {
    if changed_files.is_empty() {
        source_changed_files.to_vec()
    } else {
        changed_files.to_vec()
    }
}

fn provider_base_url(provider: Option<&RunSourceProviderParams>, default_base_url: &str) -> String {
    provider
        .and_then(|provider| provider.base_url.as_deref())
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .unwrap_or(default_base_url)
        .trim_end_matches('/')
        .to_string()
}

fn provider_api_base_url(
    provider: Option<&RunSourceProviderParams>,
    default_base_url: &str,
) -> String {
    provider
        .and_then(|provider| provider.base_url.as_deref())
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .map(|base_url| {
            if base_url == GITHUB_BASE_URL {
                GITHUB_API_BASE_URL.to_string()
            } else {
                base_url.to_string()
            }
        })
        .unwrap_or(default_base_url.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn provider_remote_url(base_url: &str, owner: &str, repo: &str) -> String {
    format!(
        "{}/{}/{}.git",
        base_url.trim_end_matches('/'),
        owner.trim_matches('/'),
        repo.trim_matches('/')
    )
}

fn run_git(workdir: &Path, args: &[&str], plan: &ProviderCheckoutPlan) -> Result<()> {
    run_git_output(workdir, args, plan).map(|_| ())
}

fn run_git_output(workdir: &Path, args: &[&str], plan: &ProviderCheckoutPlan) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .current_dir(workdir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(token) = provider_token(plan.token_env) {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!(
                    "http.{}/.extraheader",
                    plan.auth_scope_url.trim_end_matches('/')
                ),
            )
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Bearer {token}"),
            );
    }

    let output = command.output().with_context(|| {
        format!(
            "failed to execute git while materializing {}",
            plan.source_key
        )
    })?;
    if output.status.success() {
        return Ok(output.stdout);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "git {} failed while materializing {}: {}",
        args.join(" "),
        plan.source_key,
        stderr.trim()
    );
}

fn provider_token(token_env: &str) -> Option<String> {
    env::var(token_env)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests;
