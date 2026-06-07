use super::prelude::*;
use super::support::*;

#[test]
fn path_policy_blocks_parent_escape() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello").unwrap();
    let repo = test_repo(temp.path());
    let escaped = repo.normalize_tool_path(Path::new("../outside"));
    assert!(escaped.is_err());
}

#[test]
fn path_policy_blocks_dot_git() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();
    fs::write(temp.path().join(".git/config"), "secret").unwrap();
    let repo = test_repo(temp.path());
    let denied = repo.normalize_tool_path(Path::new(".git/config"));
    assert!(denied.is_err());
}

#[cfg(unix)]
#[test]
fn path_policy_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        temp.path().join("link.txt"),
    )
    .unwrap();
    let repo = test_repo(temp.path());
    let files = repo.walk_files().unwrap();
    assert!(!files.iter().any(|path| path == Path::new("link.txt")));
}

