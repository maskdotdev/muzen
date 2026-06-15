use std::fs;
use std::process::Command;
use std::sync::Arc;

use serde_json::Value;

use crate::runner_protocol::JsonRpcResponse;

use super::*;

struct StaticSourceTransport {
    root: PathBuf,
    changed_files: Vec<String>,
}

impl RunnerCallbackTransport for StaticSourceTransport {
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        assert_eq!(method, "source.materialize");
        assert_eq!(
            params["source"]["type"],
            Value::String("custom".to_string())
        );
        Ok(json!({
            "root": self.root,
            "changedFiles": self.changed_files,
        }))
    }

    fn notify(&self, _method: &str, _params: Value) -> Result<()> {
        Ok(())
    }

    fn respond(&self, _response: &JsonRpcResponse) -> Result<()> {
        Ok(())
    }
}

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
        callback: false,
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
fn materializes_raw_snapshot_source_from_host_bundle() {
    let bundle = tempfile::tempdir().expect("snapshot bundle");
    fs::create_dir_all(bundle.path().join("src")).expect("src dir");
    fs::write(bundle.path().join("src/lib.rs"), "pub fn fixture() {}\n").expect("snapshot file");
    let source = ReviewSource::raw_snapshot_with_changed_files(bundle.path(), ["src/lib.rs"]);

    let materialized = materialize_run_source(None, Some(&source), &[], None, None).unwrap();

    assert_eq!(materialized.repo_root(), bundle.path());
    assert_eq!(materialized.changed_files(), &["src/lib.rs".to_string()]);
}

#[test]
fn materializes_custom_source_through_callback_provider() {
    let bundle = tempfile::tempdir().expect("snapshot bundle");
    fs::create_dir_all(bundle.path().join("src")).expect("src dir");
    fs::write(bundle.path().join("src/lib.rs"), "pub fn fixture() {}\n").expect("snapshot file");
    let source = ReviewSource::custom("acme", "review-123").unwrap();
    let provider = RunSourceProviderParams {
        base_url: None,
        callback: true,
    };
    let transport: Arc<dyn RunnerCallbackTransport> = Arc::new(StaticSourceTransport {
        root: bundle.path().to_path_buf(),
        changed_files: vec!["src/lib.rs".to_string()],
    });

    let materialized =
        materialize_run_source(None, Some(&source), &[], Some(&provider), Some(&transport))
            .unwrap();

    assert_eq!(materialized.repo_root(), bundle.path());
    assert_eq!(materialized.changed_files(), &["src/lib.rs".to_string()]);
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
        callback: false,
    };

    let materialized =
        materialize_run_source(None, Some(&source), &[], Some(&provider), None).unwrap();

    assert!(materialized.repo_root().join("src/lib.rs").is_file());
    assert_eq!(materialized.changed_files(), &["src/lib.rs".to_string()]);
    let inline_diff = materialized.inline_diff().expect("inline diff");
    assert!(inline_diff.contains("diff --git"));
    assert!(inline_diff.contains("+pub fn fixture() {}"));
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
