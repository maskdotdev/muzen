use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::review_sessions::ReviewSessionError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReviewSource {
    Local {
        repo: PathBuf,
    },
    RawSnapshot {
        root: PathBuf,
    },
    GithubPullRequest {
        owner: String,
        repo: String,
        number: u64,
    },
    GitlabMergeRequest {
        owner: String,
        repo: String,
        number: u64,
    },
    PerforceChangelist {
        server: String,
        changelist: String,
        #[serde(default)]
        client: Option<String>,
        #[serde(default)]
        depot_paths: Vec<String>,
    },
    Custom {
        provider: String,
        id: String,
    },
}

impl ReviewSource {
    pub fn local(repo: impl Into<PathBuf>) -> Self {
        Self::Local { repo: repo.into() }
    }

    pub fn raw_snapshot(root: impl Into<PathBuf>) -> Self {
        Self::RawSnapshot { root: root.into() }
    }

    pub fn github_pull_request(
        owner: impl Into<String>,
        repo: impl Into<String>,
        number: u64,
    ) -> Result<Self, ReviewSessionError> {
        let owner = owner.into();
        let repo = repo.into();
        validate_repo_source_parts("github", &owner, &repo, number)?;
        Ok(Self::GithubPullRequest {
            owner,
            repo,
            number,
        })
    }

    pub fn gitlab_merge_request(
        owner: impl Into<String>,
        repo: impl Into<String>,
        number: u64,
    ) -> Result<Self, ReviewSessionError> {
        let owner = owner.into();
        let repo = repo.into();
        validate_repo_source_parts("gitlab", &owner, &repo, number)?;
        Ok(Self::GitlabMergeRequest {
            owner,
            repo,
            number,
        })
    }

    pub fn perforce_changelist(
        server: impl Into<String>,
        changelist: impl Into<String>,
    ) -> Result<Self, ReviewSessionError> {
        let server = server.into();
        let changelist = changelist.into();
        validate_non_empty_source_part("perforce", "server", &server)?;
        validate_non_empty_source_part("perforce", "changelist", &changelist)?;
        Ok(Self::PerforceChangelist {
            server,
            changelist,
            client: None,
            depot_paths: Vec::new(),
        })
    }

    pub fn custom(
        provider: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self, ReviewSessionError> {
        let provider = provider.into();
        let id = id.into();
        validate_non_empty_source_part("custom", "provider", &provider)?;
        validate_non_empty_source_part("custom", "id", &id)?;
        Ok(Self::Custom { provider, id })
    }

    pub fn source_key(&self) -> String {
        match self {
            Self::Local { repo } => format!("local:{}", repo.display()),
            Self::RawSnapshot { root } => format!("raw_snapshot:{}", root.display()),
            Self::GithubPullRequest {
                owner,
                repo,
                number,
            } => format!("github:{owner}/{repo}#{number}"),
            Self::GitlabMergeRequest {
                owner,
                repo,
                number,
            } => format!("gitlab:{owner}/{repo}!{number}"),
            Self::PerforceChangelist {
                server, changelist, ..
            } => format!("perforce:{server}@{changelist}"),
            Self::Custom { provider, id } => format!("custom:{provider}:{id}"),
        }
    }

    pub(crate) fn local_repo(&self) -> Option<&Path> {
        match self {
            Self::Local { repo } => Some(repo.as_path()),
            Self::RawSnapshot { root } => Some(root.as_path()),
            Self::GithubPullRequest { .. }
            | Self::GitlabMergeRequest { .. }
            | Self::PerforceChangelist { .. }
            | Self::Custom { .. } => None,
        }
    }
}

impl FromStr for ReviewSource {
    type Err = ReviewSessionError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = input.strip_prefix("github:") {
            let (owner, repo, number) = parse_repo_change(input, rest, '#')?;
            return Self::github_pull_request(owner, repo, number);
        }
        if let Some(rest) = input.strip_prefix("gitlab:") {
            let (owner, repo, number) = parse_repo_change(input, rest, '!')?;
            return Self::gitlab_merge_request(owner, repo, number);
        }
        if let Some(rest) = input.strip_prefix("local:") {
            if rest.trim().is_empty() {
                return Err(ReviewSessionError::InvalidSource {
                    input: input.to_string(),
                    reason: "local source path is empty".to_string(),
                });
            }
            return Ok(Self::local(PathBuf::from(rest)));
        }
        if let Some(rest) = input.strip_prefix("raw_snapshot:") {
            if rest.trim().is_empty() {
                return Err(ReviewSessionError::InvalidSource {
                    input: input.to_string(),
                    reason: "raw snapshot path is empty".to_string(),
                });
            }
            return Ok(Self::raw_snapshot(PathBuf::from(rest)));
        }
        Err(ReviewSessionError::InvalidSource {
            input: input.to_string(),
            reason:
                "expected github:owner/repo#number, gitlab:owner/repo!number, local:path, or raw_snapshot:path"
                    .to_string(),
        })
    }
}

impl std::fmt::Display for ReviewSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.source_key())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewSourceLike {
    Source(ReviewSource),
    Shorthand(String),
    LocalPath(PathBuf),
}

impl ReviewSourceLike {
    pub fn resolve(self) -> Result<ReviewSource, ReviewSessionError> {
        match self {
            Self::Source(source) => Ok(source),
            Self::Shorthand(source) => ReviewSource::from_str(&source),
            Self::LocalPath(path) => Ok(ReviewSource::local(path)),
        }
    }
}

impl From<ReviewSource> for ReviewSourceLike {
    fn from(value: ReviewSource) -> Self {
        Self::Source(value)
    }
}

impl From<String> for ReviewSourceLike {
    fn from(value: String) -> Self {
        Self::Shorthand(value)
    }
}

impl From<&str> for ReviewSourceLike {
    fn from(value: &str) -> Self {
        Self::Shorthand(value.to_string())
    }
}

impl From<PathBuf> for ReviewSourceLike {
    fn from(value: PathBuf) -> Self {
        Self::LocalPath(value)
    }
}

impl From<&Path> for ReviewSourceLike {
    fn from(value: &Path) -> Self {
        Self::LocalPath(value.to_path_buf())
    }
}

fn parse_repo_change(
    input: &str,
    rest: &str,
    delimiter: char,
) -> Result<(String, String, u64), ReviewSessionError> {
    let delimiter_index =
        rest.rfind(delimiter)
            .ok_or_else(|| ReviewSessionError::InvalidSource {
                input: input.to_string(),
                reason: format!("missing `{delimiter}` review number delimiter"),
            })?;
    let path = &rest[..delimiter_index];
    let number = rest[delimiter_index + delimiter.len_utf8()..]
        .parse::<u64>()
        .map_err(|_| ReviewSessionError::InvalidSource {
            input: input.to_string(),
            reason: "review number must be a positive integer".to_string(),
        })?;
    let (owner, repo) = path
        .rsplit_once('/')
        .ok_or_else(|| ReviewSessionError::InvalidSource {
            input: input.to_string(),
            reason: "missing owner/repo path".to_string(),
        })?;
    Ok((owner.to_string(), repo.to_string(), number))
}

fn validate_non_empty_source_part(
    provider: &str,
    field: &str,
    value: &str,
) -> Result<(), ReviewSessionError> {
    if value.trim().is_empty() {
        return Err(ReviewSessionError::InvalidSource {
            input: provider.to_string(),
            reason: format!("{field} is empty"),
        });
    }
    Ok(())
}

fn validate_repo_source_parts(
    provider: &str,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<(), ReviewSessionError> {
    if owner.trim().is_empty() {
        return Err(ReviewSessionError::InvalidSource {
            input: provider.to_string(),
            reason: "owner is empty".to_string(),
        });
    }
    if repo.trim().is_empty() {
        return Err(ReviewSessionError::InvalidSource {
            input: provider.to_string(),
            reason: "repo is empty".to_string(),
        });
    }
    if number == 0 {
        return Err(ReviewSessionError::InvalidSource {
            input: provider.to_string(),
            reason: "review number must be greater than zero".to_string(),
        });
    }
    Ok(())
}
