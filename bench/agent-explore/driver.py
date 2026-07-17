#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import json
import os
import time
from pathlib import Path
from threading import Lock
from typing import Any

from muzen import Agent, tool


def jailed(root: Path, requested: str) -> Path:
    candidate = (root / requested).resolve()
    if candidate != root and root not in candidate.parents:
        raise ValueError("path escapes --root: %s" % requested)
    return candidate


def files_under(root: Path, requested: str) -> list[dict[str, Any]]:
    base = jailed(root, requested)
    entries = []
    for path in sorted(item for item in base.rglob("*") if item.is_file()):
        resolved = path.resolve()
        if resolved != root and root not in resolved.parents:
            continue
        entries.append({"path": resolved.relative_to(root).as_posix(), "bytes": resolved.stat().st_size})
    return entries


async def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--transport", choices=("local_runner", "http"), required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--base-url")
    parser.add_argument("--model-base-url", required=True)
    parser.add_argument("--read-files", type=int, default=5)
    args = parser.parse_args()
    root = args.root.resolve()
    if args.transport == "http" and not args.base_url:
        parser.error("--base-url is required for http transport")
    if args.read_files < 1:
        parser.error("--read-files must be positive")

    counts = {"fs_list": 0, "fs_read": 0, "fs_grep": 0}
    count_lock = Lock()

    @tool
    def fs_list(path: str) -> dict[str, Any]:
        """Recursively list regular files below a repository path."""
        with count_lock:
            counts["fs_list"] += 1
        entries = files_under(root, path)
        return {"path": path, "files": entries, "totalFiles": len(entries)}

    @tool
    def fs_read(path: str) -> dict[str, Any]:
        """Read one UTF-8 repository file."""
        with count_lock:
            counts["fs_read"] += 1
        target = jailed(root, path)
        data = target.read_bytes()
        return {"path": path, "bytes": len(data), "content": data.decode("utf-8", errors="replace")}

    @tool
    def fs_grep(pattern: str, path: str) -> dict[str, Any]:
        """Search repository files for a fixed text pattern."""
        with count_lock:
            counts["fs_grep"] += 1
        matches: list[dict[str, Any]] = []
        total = 0
        for entry in files_under(root, path):
            target = jailed(root, entry["path"])
            for number, line in enumerate(target.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
                if pattern in line:
                    total += 1
                    if len(matches) < 100:
                        matches.append({"path": entry["path"], "line": number, "text": line[:500]})
        return {"pattern": pattern, "path": path, "matches": matches, "totalMatches": total, "truncated": total > len(matches)}

    options: dict[str, Any] = {
        "instructions": "Explore the repository with the provided filesystem tools, then summarize what you saw.",
        "model": "openai:muzen-agent-explore",
        "tools": [fs_list, fs_read, fs_grep],
        "transport": args.transport,
        "api_key": "bench-test-key",
        "model_base_url": args.model_base_url,
    }
    if args.transport == "local_runner":
        options["base_url"] = args.model_base_url
        options.pop("model_base_url")
    else:
        options["base_url"] = args.base_url

    started = time.perf_counter()
    agent = Agent(**options)
    try:
        result = await agent.run("Explore src and report a concise summary.")
        result.raise_for_status()
    finally:
        await agent.close()
    duration_ms = round((time.perf_counter() - started) * 1000, 3)
    expected = {"fs_list": 1, "fs_read": args.read_files, "fs_grep": 1}
    if counts != expected:
        raise RuntimeError("tool count mismatch: expected %r, got %r" % (expected, counts))
    if not result.text.strip():
        raise RuntimeError("agent returned an empty summary")
    print(json.dumps({
        "turns": sum(counts.values()) + 1,
        "toolCalls": sum(counts.values()),
        "durationMs": duration_ms,
        "summaryText": result.text,
    }, separators=(",", ":")))


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Exception as error:
        print("muzen agent explore Python driver: %s" % error, file=os.sys.stderr)
        raise SystemExit(1)
