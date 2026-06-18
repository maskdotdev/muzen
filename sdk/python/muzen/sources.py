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


def local(repo: str) -> ReviewSource:
    if not repo.strip():
        raise MuzenSourceError("local source path is empty")
    return ReviewSource(type="local", repo=repo)


def raw_snapshot(root: str) -> ReviewSource:
    if not root.strip():
        raise MuzenSourceError("raw snapshot root is empty")
    return ReviewSource(type="raw_snapshot", root=root)


def perforce(
    server: str,
    changelist: str,
    *,
    client: Optional[str] = None,
    depot_paths: Optional[List[str]] = None,
) -> ReviewSource:
    if not server.strip():
        raise MuzenSourceError("perforce server is empty")
    if not changelist.strip():
        raise MuzenSourceError("perforce changelist is empty")
    return ReviewSource(
        type="perforce_changelist",
        server=server,
        changelist=changelist,
        client=client,
        depot_paths=depot_paths or [],
    )


def custom_source(provider: str, id: str) -> ReviewSource:
    if not provider.strip():
        raise MuzenSourceError("custom source provider is empty")
    if not id.strip():
        raise MuzenSourceError("custom source id is empty")
    return ReviewSource(type="custom", provider=provider, id=id)


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
    if source.startswith("raw_snapshot:"):
        return raw_snapshot(source[len("raw_snapshot:") :])
    if source.startswith("perforce:"):
        server, changelist = _parse_perforce(source[len("perforce:") :], source)
        return perforce(server, changelist)
    if source.startswith("custom:"):
        provider, id = _parse_custom(source[len("custom:") :], source)
        return custom_source(provider, id)
    raise MuzenSourceError(
        "expected github:owner/repo#number, gitlab:owner/repo!number, local:path, raw_snapshot:path, perforce:server@changelist, or custom:provider:id"
    )


def source_key(source: ReviewSource) -> str:
    if source.type == "local":
        return f"local:{source.repo}"
    if source.type == "github_pull_request":
        return f"github:{source.owner}/{source.repo}#{source.number}"
    if source.type == "gitlab_merge_request":
        return f"gitlab:{source.owner}/{source.repo}!{source.number}"
    if source.type == "raw_snapshot":
        return f"raw_snapshot:{source.root}"
    if source.type == "perforce_changelist":
        return f"perforce:{source.server}@{source.changelist}"
    if source.type == "custom":
        return f"custom:{source.provider}:{source.id}"
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


def _parse_perforce(rest: str, input_value: str) -> Tuple[str, str]:
    delimiter_index = rest.rfind("@")
    if delimiter_index == -1:
        raise MuzenSourceError(
            f"invalid review source {input_value}: missing @ changelist delimiter"
        )
    return rest[:delimiter_index], rest[delimiter_index + 1 :]


def _parse_custom(rest: str, input_value: str) -> Tuple[str, str]:
    delimiter_index = rest.find(":")
    if delimiter_index == -1:
        raise MuzenSourceError(
            f"invalid review source {input_value}: missing provider:id delimiter"
        )
    return rest[:delimiter_index], rest[delimiter_index + 1 :]
