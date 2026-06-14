use super::prelude::*;
use super::support::*;

#[test]
fn path_policy_blocks_parent_escape() {
    assert!(RepoPath::parse("../outside").is_err());
}

#[test]
fn path_policy_blocks_dot_git() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello").unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();
    fs::write(temp.path().join(".git/config"), "secret").unwrap();
    let snapshot = RepoSnapshot::build(
        temp.path(),
        &PathPolicyV1::bench(64, 10),
        &test_change_with_file("README.md"),
    )
    .unwrap();

    assert!(!snapshot
        .manifest
        .files
        .iter()
        .any(|file| file.rel_path.display() == ".git/config"));
}

#[cfg(unix)]
#[test]
fn path_policy_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello").unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        temp.path().join("link.txt"),
    )
    .unwrap();
    let snapshot = RepoSnapshot::build(
        temp.path(),
        &PathPolicyV1::bench(64, 10),
        &test_change_with_file("README.md"),
    )
    .unwrap();

    assert!(!snapshot
        .manifest
        .files
        .iter()
        .any(|file| file.rel_path.as_path() == Path::new("link.txt")));
}
