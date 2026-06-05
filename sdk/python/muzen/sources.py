from __future__ import annotations

from typing import List, Optional, Tuple

from .types import ReviewSource, ReviewSourceLike


class MuzenSourceError(ValueError):
    pass


class _GithubBuilder:
    def pull_request(self, *, owner: str, repo: str, number: int) -> ReviewSource:
        _assert_repo_source_parts("github", owner, repo, number)
        return ReviewSource(
            type="github_pull_request",
            owner=owner,
            repo=repo,
            number=number,
        )


class _GitlabBuilder:
    def merge_request(self, *, owner: str, repo: str, number: int) -> ReviewSource:
        _assert_repo_source_parts("gitlab", owner, repo, number)
        return ReviewSource(
            type="gitlab_merge_request",
            owner=owner,
            repo=repo,
            number=number,
        )


github = _GithubBuilder()
gitlab = _GitlabBuilder()


def local(repo: str, *, changed_files: Optional[List[str]] = None) -> ReviewSource:
    if not repo.strip():
        raise MuzenSourceError("local source path is empty")
    return ReviewSource(type="local", repo=repo, changed_files=changed_files or [])


def parse_review_source(source: ReviewSourceLike) -> ReviewSource:
    if isinstance(source, ReviewSource):
        return source
    if source.startswith("github:"):
        owner, repo, number = _parse_repo_change(source, source[len("github:") :], "#")
        return github.pull_request(owner=owner, repo=repo, number=number)
    if source.startswith("gitlab:"):
        owner, repo, number = _parse_repo_change(source, source[len("gitlab:") :], "!")
        return gitlab.merge_request(owner=owner, repo=repo, number=number)
    if source.startswith("local:"):
        return local(source[len("local:") :])
    raise MuzenSourceError(
        "expected github:owner/repo#number, gitlab:owner/repo!number, or local:path"
    )


def source_key(source: ReviewSource) -> str:
    if source.type == "local":
        return f"local:{source.repo}"
    if source.type == "github_pull_request":
        return f"github:{source.owner}/{source.repo}#{source.number}"
    if source.type == "gitlab_merge_request":
        return f"gitlab:{source.owner}/{source.repo}!{source.number}"
    raise MuzenSourceError(f"unknown review source type: {source.type}")


def _parse_repo_change(
    input_value: str,
    rest: str,
    delimiter: str,
) -> Tuple[str, str, int]:
    delimiter_index = rest.rfind(delimiter)
    if delimiter_index == -1:
        raise MuzenSourceError(
            f"invalid review source {input_value}: missing {delimiter} review number delimiter"
        )
    path = rest[:delimiter_index]
    number_text = rest[delimiter_index + 1 :]
    repo_separator_index = path.rfind("/")
    if repo_separator_index == -1:
        raise MuzenSourceError(f"invalid review source {input_value}: missing owner/repo path")
    try:
        number = int(number_text)
    except ValueError as error:
        raise MuzenSourceError("review number must be a positive integer") from error
    return path[:repo_separator_index], path[repo_separator_index + 1 :], number


def _assert_repo_source_parts(provider: str, owner: str, repo: str, number: int) -> None:
    if not owner.strip():
        raise MuzenSourceError(f"{provider} owner is empty")
    if not repo.strip():
        raise MuzenSourceError(f"{provider} repo is empty")
    if number <= 0:
        raise MuzenSourceError(f"{provider} review number must be positive")
