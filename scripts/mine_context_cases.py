#!/usr/bin/env python3
"""Mine context-engine evaluation cases from merged git history.

For a commit pair (base, follow_up) where the follow-up landed shortly after
the base and touched the same area, the base diff is the "change under
review" and the follow-up's touched files (minus files already in the base
diff) are ground-truth context the engine should have surfaced.

Output cases pin the base commit so the harness can materialize the exact
tree with `git archive`. Mining is deterministic for a pinned revision.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
from pathlib import Path

CODE_EXTENSIONS = {".rs", ".ts", ".tsx", ".js", ".py", ".go"}
EXCLUDED_PREFIXES = ("bench/", "fixtures/", "docs/")
CASE_SCHEMA_VERSION = "muzen.context-eval-case.v2"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path("."), help="Git repository to mine.")
    parser.add_argument(
        "--rev",
        default="HEAD",
        help="Pinned revision to mine up to. Pin a SHA for reproducible output.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("bench/context-engine/cases/mined"),
        help="Directory for mined case JSON files. Existing mined cases are replaced.",
    )
    parser.add_argument("--max-cases", type=int, default=40)
    parser.add_argument(
        "--window",
        type=int,
        default=8,
        help="How many subsequent commits to scan for a qualifying follow-up.",
    )
    parser.add_argument("--max-changed-files", type=int, default=8)
    parser.add_argument("--max-expected-files", type=int, default=12)
    return parser.parse_args()


def git(repo: Path, *args: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return completed.stdout


def diff_files(repo: Path, sha: str, statuses: str) -> list[str]:
    """Code files touched by `sha` whose git status letter is in `statuses`."""
    files: list[str] = []
    for line in git(repo, "diff-tree", "--no-commit-id", "--name-status", "-r", sha).splitlines():
        parts = line.split("\t")
        if len(parts) < 2:
            continue
        status, path = parts[0], parts[-1]
        if status[0] not in statuses:
            continue
        if path.startswith(EXCLUDED_PREFIXES):
            continue
        if os.path.splitext(path)[1] in CODE_EXTENSIONS:
            files.append(path)
    return files


def exists_at(repo: Path, sha: str, path: str) -> bool:
    completed = subprocess.run(
        ["git", "-C", str(repo), "cat-file", "-e", f"{sha}:{path}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return completed.returncode == 0


def subject(repo: Path, sha: str) -> str:
    return git(repo, "log", "-1", "--format=%s", sha).strip()


def shares_area(base_files: list[str], follow_files: list[str]) -> bool:
    base_dirs = {os.path.dirname(path) for path in base_files}
    follow_dirs = {os.path.dirname(path) for path in follow_files}
    return bool(set(base_files) & set(follow_files)) or bool(base_dirs & follow_dirs)


def mine(args: argparse.Namespace) -> list[dict]:
    shas = git(args.repo, "rev-list", "--no-merges", "--reverse", args.rev).split()
    diffs = {sha: (diff_files(args.repo, sha, "AM"), diff_files(args.repo, sha, "M")) for sha in shas}
    cases: list[dict] = []
    for index, base in enumerate(shas):
        if len(cases) >= args.max_cases:
            break
        base_files = diffs[base][0]
        if not 1 <= len(base_files) <= args.max_changed_files:
            continue
        for follow in shas[index + 1 : index + 1 + args.window]:
            follow_files = diffs[follow][1]
            if not shares_area(base_files, follow_files):
                continue
            expected = [
                path
                for path in follow_files
                if path not in set(base_files) and exists_at(args.repo, base, path)
            ]
            if not 1 <= len(expected) <= args.max_expected_files:
                continue
            cases.append(build_case(args.repo, base, follow, base_files, expected))
            break
    return cases


def build_case(
    repo: Path, base: str, follow: str, changed_files: list[str], expected: list[str]
) -> dict:
    name = f"mined-{base[:12]}"
    return {
        "schemaVersion": CASE_SCHEMA_VERSION,
        "name": name,
        "repoSource": {"kind": "git", "commit": base},
        "changedFiles": sorted(changed_files),
        "description": (
            f"Mined pair: base {base[:12]} ({subject(repo, base)}) followed by "
            f"{follow[:12]} ({subject(repo, follow)})."
        ),
        "minedFrom": {"baseCommit": base, "followUpCommit": follow},
        "cases": [
            {
                "id": f"{name}-pack",
                "command": "pack",
                "purpose": "general-review",
                "maxTokens": 12000,
                "expectedPaths": sorted(expected),
            }
        ],
    }


def main() -> int:
    args = parse_args()
    cases = mine(args)
    if args.output_dir.exists():
        shutil.rmtree(args.output_dir)
    args.output_dir.mkdir(parents=True)
    for case in cases:
        path = args.output_dir / f"{case['name']}.json"
        path.write_text(json.dumps(case, indent=2, sort_keys=False) + "\n")
    print(f"mined {len(cases)} cases into {args.output_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
