import type {
  CustomReviewSource,
  GithubPullRequestSource,
  GitlabMergeRequestSource,
  LocalReviewSource,
  PerforceChangelistSource,
  RawSnapshotReviewSource,
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

export const perforce = {
  changelist(input: {
    server: string;
    changelist: string | number;
    client?: string;
    depotPaths?: string[];
  }): PerforceChangelistSource {
    assertNonEmptySourcePart("perforce", "server", input.server);
    const changelist = String(input.changelist);
    assertNonEmptySourcePart("perforce", "changelist", changelist);
    return {
      type: "perforce_changelist",
      server: input.server,
      changelist,
      client: input.client,
      depotPaths: input.depotPaths ?? [],
    };
  },
};

export function local(repo: string): LocalReviewSource {
  if (repo.trim().length === 0) {
    throw new MuzenSourceError("local source path is empty");
  }
  return {
    type: "local",
    repo,
  };
}

export function rawSnapshot(root: string): RawSnapshotReviewSource {
  if (root.trim().length === 0) {
    throw new MuzenSourceError("raw snapshot path is empty");
  }
  return {
    type: "raw_snapshot",
    root,
  };
}

export function customSource(input: {
  provider: string;
  id: string;
}): CustomReviewSource {
  assertNonEmptySourcePart("custom", "provider", input.provider);
  assertNonEmptySourcePart("custom", "id", input.id);
  return {
    type: "custom",
    provider: input.provider,
    id: input.id,
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
  if (source.startsWith("raw_snapshot:")) {
    return rawSnapshot(source.slice("raw_snapshot:".length));
  }
  throw new MuzenSourceError(
    "expected github:owner/repo#number, gitlab:owner/repo!number, local:path, or raw_snapshot:path",
  );
}

export function sourceKey(source: ReviewSource): string {
  switch (source.type) {
    case "local":
      return `local:${source.repo}`;
    case "raw_snapshot":
      return `raw_snapshot:${source.root}`;
    case "github_pull_request":
      return `github:${source.owner}/${source.repo}#${source.number}`;
    case "gitlab_merge_request":
      return `gitlab:${source.owner}/${source.repo}!${source.number}`;
    case "perforce_changelist":
      return `perforce:${source.server}@${source.changelist}`;
    case "custom":
      return `custom:${source.provider}:${source.id}`;
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

function assertNonEmptySourcePart(
  provider: string,
  field: string,
  value: string,
): void {
  if (value.trim().length === 0) {
    throw new MuzenSourceError(`${provider} ${field} is empty`);
  }
}
