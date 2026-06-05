import type {
  GithubPullRequestSource,
  GitlabMergeRequestSource,
  LocalReviewSource,
  ReviewSource,
  ReviewSourceLike,
} from "./types.js";

export const github = {
  pullRequest(input: {
    owner: string;
    repo: string;
    number: number;
  }): GithubPullRequestSource {
    assertRepoSourceParts("github", input.owner, input.repo, input.number);
    return {
      type: "github_pull_request",
      owner: input.owner,
      repo: input.repo,
      number: input.number,
    };
  },
};

export const gitlab = {
  mergeRequest(input: {
    owner: string;
    repo: string;
    number: number;
  }): GitlabMergeRequestSource {
    assertRepoSourceParts("gitlab", input.owner, input.repo, input.number);
    return {
      type: "gitlab_merge_request",
      owner: input.owner,
      repo: input.repo,
      number: input.number,
    };
  },
};

export function local(
  repo: string,
  options: { changedFiles?: string[] } = {},
): LocalReviewSource {
  if (repo.trim().length === 0) {
    throw new MuzenSourceError("local source path is empty");
  }
  return {
    type: "local",
    repo,
    changedFiles: options.changedFiles ?? [],
  };
}

export function parseReviewSource(source: ReviewSourceLike): ReviewSource {
  if (typeof source !== "string") {
    return source;
  }
  if (source.startsWith("github:")) {
    const parsed = parseRepoChange(source, source.slice("github:".length), "#");
    return github.pullRequest(parsed);
  }
  if (source.startsWith("gitlab:")) {
    const parsed = parseRepoChange(source, source.slice("gitlab:".length), "!");
    return gitlab.mergeRequest(parsed);
  }
  if (source.startsWith("local:")) {
    return local(source.slice("local:".length));
  }
  throw new MuzenSourceError(
    "expected github:owner/repo#number, gitlab:owner/repo!number, or local:path",
  );
}

export function sourceKey(source: ReviewSource): string {
  switch (source.type) {
    case "local":
      return `local:${source.repo}`;
    case "github_pull_request":
      return `github:${source.owner}/${source.repo}#${source.number}`;
    case "gitlab_merge_request":
      return `gitlab:${source.owner}/${source.repo}!${source.number}`;
  }
}

export class MuzenSourceError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "MuzenSourceError";
  }
}

function parseRepoChange(
  input: string,
  rest: string,
  delimiter: "#" | "!",
): { owner: string; repo: string; number: number } {
  const delimiterIndex = rest.lastIndexOf(delimiter);
  if (delimiterIndex === -1) {
    throw new MuzenSourceError(
      `invalid review source ${input}: missing ${delimiter} review number delimiter`,
    );
  }
  const path = rest.slice(0, delimiterIndex);
  const number = Number(rest.slice(delimiterIndex + 1));
  const repoSeparatorIndex = path.lastIndexOf("/");
  if (repoSeparatorIndex === -1) {
    throw new MuzenSourceError(
      `invalid review source ${input}: missing owner/repo path`,
    );
  }
  return {
    owner: path.slice(0, repoSeparatorIndex),
    repo: path.slice(repoSeparatorIndex + 1),
    number,
  };
}

function assertRepoSourceParts(
  provider: string,
  owner: string,
  repo: string,
  number: number,
): void {
  if (owner.trim().length === 0) {
    throw new MuzenSourceError(`${provider} owner is empty`);
  }
  if (repo.trim().length === 0) {
    throw new MuzenSourceError(`${provider} repo is empty`);
  }
  if (!Number.isInteger(number) || number <= 0) {
    throw new MuzenSourceError(`${provider} review number must be positive`);
  }
}
