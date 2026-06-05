use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tempfile::TempDir;

use crate::review_session::ReviewSource;

use super::types::RunSourceProviderParams;

const GITHUB_BASE_URL: &str = "https://github.com";
const GITLAB_BASE_URL: &str = "https://gitlab.com";
const MATERIALIZED_HEAD_REF: &str = "refs/remotes/origin/muzen-review-head";
const MATERIALIZED_BASE_REF: &str = "refs/remotes/origin/HEAD";

pub(crate) struct MaterializedRunSource {
    repo_root: PathBuf,
    changed_files: Vec<String>,
    _temp_dir: Option<TempDir>,
}

impl MaterializedRunSource {
    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub(crate) fn changed_files(&self) -> &[String] {
        &self.changed_files
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
) -> Result<MaterializedRunSource> {
    if let Some(repo) = repo {
        return Ok(MaterializedRunSource {
            repo_root: repo.to_path_buf(),
            changed_files: changed_files.to_vec(),
            _temp_dir: None,
        });
    }

    let Some(source) = source else {
        anyhow::bail!("run.start requires either repo or source");
    };

    match source {
        ReviewSource::Local {
            repo,
            changed_files: source_changed_files,
        } => Ok(MaterializedRunSource {
            repo_root: repo.clone(),
            changed_files: override_or_source_changed_files(changed_files, source_changed_files),
            _temp_dir: None,
        }),
        ReviewSource::GithubPullRequest { .. } | ReviewSource::GitlabMergeRequest { .. } => {
            materialize_provider_source(source, changed_files, provider)
        }
    }
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
        ReviewSource::Local { .. } => {
            anyhow::bail!("local sources do not require provider checkout planning")
        }
    }
}

fn materialize_provider_source(
    source: &ReviewSource,
    changed_files: &[String],
    provider: Option<&RunSourceProviderParams>,
) -> Result<MaterializedRunSource> {
    let plan = provider_checkout_plan(source, provider)?;
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

    let inferred_changed_files = if changed_files.is_empty() {
        infer_changed_files(&repo_root, &plan)
    } else {
        changed_files.to_vec()
    };

    Ok(MaterializedRunSource {
        repo_root,
        changed_files: inferred_changed_files,
        _temp_dir: Some(temp_dir),
    })
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
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;

    #[test]
    fn plans_github_pull_request_checkout() {
        let source = ReviewSource::github_pull_request("maskdotdev", "heimdaal", 123).unwrap();

        let plan = provider_checkout_plan(&source, None).unwrap();

        assert_eq!(
            plan.remote_url,
            "https://github.com/maskdotdev/heimdaal.git"
        );
        assert_eq!(
            plan.head_refspec,
            "+refs/pull/123/head:refs/remotes/origin/muzen-review-head"
        );
        assert_eq!(plan.token_env, "GITHUB_TOKEN");
        assert!(!plan.remote_url.contains("TOKEN"));
    }

    #[test]
    fn plans_gitlab_merge_request_checkout_with_nested_namespace() {
        let source = ReviewSource::gitlab_merge_request("platform/tools", "heimdaal", 77).unwrap();
        let provider = RunSourceProviderParams {
            base_url: Some("https://gitlab.example.test/".to_string()),
        };

        let plan = provider_checkout_plan(&source, Some(&provider)).unwrap();

        assert_eq!(
            plan.remote_url,
            "https://gitlab.example.test/platform/tools/heimdaal.git"
        );
        assert_eq!(
            plan.head_refspec,
            "+refs/merge-requests/77/head:refs/remotes/origin/muzen-review-head"
        );
        assert_eq!(plan.token_env, "GITLAB_TOKEN");
    }

    #[test]
    fn materializes_provider_source_from_local_git_remote() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let root = tempfile::tempdir().expect("temp root");
        let remote = root.path().join("maskdotdev").join("heimdaal.git");
        fs::create_dir_all(remote.parent().expect("remote parent")).expect("remote parent");
        git(root.path(), &["init", "--bare", remote.to_str().unwrap()]);

        let work = root.path().join("work");
        fs::create_dir_all(&work).expect("work dir");
        git(&work, &["init", "."]);
        fs::write(work.join("README.md"), "# fixture\n").expect("write base");
        git(&work, &["add", "README.md"]);
        git(
            &work,
            &[
                "-c",
                "user.name=Muzen Test",
                "-c",
                "user.email=muzen@example.test",
                "commit",
                "-m",
                "base",
            ],
        );
        git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&work, &["push", "origin", "HEAD:master"]);
        git(
            root.path(),
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/master",
            ],
        );
        fs::create_dir_all(work.join("src")).expect("src dir");
        fs::write(work.join("src/lib.rs"), "pub fn fixture() {}\n").expect("write head");
        git(&work, &["add", "src/lib.rs"]);
        git(
            &work,
            &[
                "-c",
                "user.name=Muzen Test",
                "-c",
                "user.email=muzen@example.test",
                "commit",
                "-m",
                "head",
            ],
        );
        git(&work, &["push", "origin", "HEAD:refs/pull/123/head"]);

        let source = ReviewSource::github_pull_request("maskdotdev", "heimdaal", 123).unwrap();
        let provider = RunSourceProviderParams {
            base_url: Some(format!("file://{}", root.path().display())),
        };

        let materialized =
            materialize_run_source(None, Some(&source), &[], Some(&provider)).unwrap();

        assert!(materialized.repo_root().join("src/lib.rs").is_file());
        assert_eq!(materialized.changed_files(), &["src/lib.rs".to_string()]);
    }

    fn git(workdir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(workdir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git executes");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
