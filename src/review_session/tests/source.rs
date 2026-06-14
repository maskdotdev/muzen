use super::super::*;
use std::str::FromStr;

#[tokio::test]
async fn parses_github_source_shorthand() {
    let source = ReviewSource::from_str("github:maskdotdev/heimdaal#123").unwrap();

    assert_eq!(
        source,
        ReviewSource::GithubPullRequest {
            owner: "maskdotdev".to_string(),
            repo: "heimdaal".to_string(),
            number: 123
        }
    );
    assert_eq!(source.source_key(), "github:maskdotdev/heimdaal#123");
}

#[tokio::test]
async fn parses_gitlab_source_shorthand_with_nested_owner() {
    let source = ReviewSource::from_str("gitlab:platform/reviews/heimdaal!42").unwrap();

    assert_eq!(
        source,
        ReviewSource::GitlabMergeRequest {
            owner: "platform/reviews".to_string(),
            repo: "heimdaal".to_string(),
            number: 42
        }
    );
    assert_eq!(source.source_key(), "gitlab:platform/reviews/heimdaal!42");
}

#[tokio::test]
async fn parses_raw_snapshot_source_shorthand() {
    let source = ReviewSource::from_str("raw_snapshot:/tmp/muzen-snapshot").unwrap();

    assert_eq!(source, ReviewSource::raw_snapshot("/tmp/muzen-snapshot"));
    assert_eq!(source.source_key(), "raw_snapshot:/tmp/muzen-snapshot");
}

#[tokio::test]
async fn builds_non_git_provider_sources() {
    let perforce = ReviewSource::perforce_changelist("perforce.example:1666", "12345").unwrap();
    let custom = ReviewSource::custom("acme", "review-123").unwrap();

    assert_eq!(
        perforce.source_key(),
        "perforce:perforce.example:1666@12345"
    );
    assert_eq!(custom.source_key(), "custom:acme:review-123");
}

#[tokio::test]
async fn rejects_invalid_source_shorthand() {
    let error = ReviewSource::from_str("github:maskdotdev/heimdaal").unwrap_err();

    assert!(error
        .to_string()
        .contains("missing `#` review number delimiter"));
}
