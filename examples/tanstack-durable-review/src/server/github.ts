export interface GithubPullRequestParts {
  owner: string;
  repo: string;
  number: number;
}

const githubSourceKeyPattern = /^github:([^/]+)\/([^#]+)#([1-9]\d*)$/;
const shorthandPattern = /^([^/\s]+)\/([^#\s]+)#([1-9]\d*)$/;

export function parseGithubPullRequestInput(
  input: string,
): GithubPullRequestParts {
  const value = input.trim();
  if (!value) {
    throw new Error("GitHub PR input is empty");
  }

  const sourceKeyMatch = githubSourceKeyPattern.exec(value);
  if (sourceKeyMatch) {
    return partsFromMatch(sourceKeyMatch);
  }

  const shorthandMatch = shorthandPattern.exec(value);
  if (shorthandMatch) {
    return partsFromMatch(shorthandMatch);
  }

  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error(
      "GitHub PR must look like https://github.com/owner/repo/pull/123",
    );
  }

  if (url.hostname !== "github.com") {
    throw new Error("Only github.com PR URLs are supported by this example");
  }

  const [owner, repo, pullSegment, numberSegment] = url.pathname
    .split("/")
    .filter(Boolean);
  const number = Number(numberSegment);
  if (
    !owner ||
    !repo ||
    pullSegment !== "pull" ||
    !Number.isInteger(number) ||
    number <= 0
  ) {
    throw new Error(
      "GitHub PR must look like https://github.com/owner/repo/pull/123",
    );
  }

  return { owner, repo, number };
}

function partsFromMatch(match: RegExpExecArray): GithubPullRequestParts {
  return {
    owner: match[1],
    repo: match[2],
    number: Number(match[3]),
  };
}
