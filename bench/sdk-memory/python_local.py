#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import json
import os
import platform
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]
SDK_ROOT = REPO_ROOT / "sdk/python"
if str(SDK_ROOT) not in sys.path:
    sys.path.insert(0, str(SDK_ROOT))

from muzen import (  # noqa: E402
    ReviewAgentBudget,
    ReviewAgentSession,
    ReviewLimits,
    ReviewOptions,
    create_muzen,
    local,
)

SCHEMA_VERSION = "muzen.sdk-memory-benchmark.v1"
ROLES = [
    "correctness",
    "security",
    "performance",
    "maintainability",
    "architecture",
    "validator",
]


async def main() -> int:
    args = parse_args()
    runner_path = args.runner_path or default_runner_path()
    repo = str(Path(args.repo).resolve())
    max_active_sessions = args.max_active or args.sessions
    sampler = MemorySampler(sample_ms=args.sample_ms)
    started_at_utc = utc_now()
    started = time.perf_counter()
    client = None
    result = None
    error_message: Optional[str] = None

    await sampler.start()
    try:
        client = await create_muzen(
            runner_path=runner_path,
            client_name="muzen-py-memory-bench",
        )
        sampler.sample()
        review = await client.review(
            local(repo, changed_files=args.changed_file),
            ReviewOptions(
                sessions=build_sessions(args),
                limits=ReviewLimits(
                    max_active_sessions=max_active_sessions,
                    max_file_bytes=args.max_file_kb * 1024,
                    max_search_matches=args.max_search_matches,
                ),
            ),
        )
        result = await review.wait()
        if args.hold_ms > 0:
            await asyncio.sleep(args.hold_ms / 1000)
        sampler.sample()
    except Exception as error:  # noqa: BLE001 - benchmark reports the failure in JSON.
        error_message = str(error)
    finally:
        await sampler.stop()
        if client is not None:
            try:
                await client.close()
            except Exception:
                pass

    elapsed_ms = max(1, int((time.perf_counter() - started) * 1000 + 0.999))
    failures = benchmark_failures(result, error_message, sampler)
    report = {
        "schemaVersion": SCHEMA_VERSION,
        "sdk": {
            "language": "python",
            "package": "muzen",
            "version": "0.1.0-preview.0",
            "runtime": f"python {platform.python_version()}",
        },
        "mode": "local-runner-stdio",
        "runner": {
            "path": runner_path,
        },
        "workload": {
            "repo": repo,
            "sessions": args.sessions,
            "maxActiveSessions": max_active_sessions,
            "maxTurns": args.max_turns,
            "maxToolCalls": args.max_tool_calls,
            "maxFileKb": args.max_file_kb,
            "maxSearchMatches": args.max_search_matches,
            "changedFiles": args.changed_file,
        },
        "timing": {
            "startedAtUtc": started_at_utc,
            "finishedAtUtc": utc_now(),
            "elapsedMs": elapsed_ms,
            "holdMs": args.hold_ms,
            "sampleMs": args.sample_ms,
        },
        "memory": sampler.report(),
        "result": result_report(result),
        "benchmarkValid": len(failures) == 0,
        "benchmarkFailures": failures,
    }
    output = json.dumps(report, indent=2)
    if args.output:
        output_path = Path(args.output)
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(output + "\n", encoding="utf-8")
    else:
        print(output)
    return 0 if len(failures) == 0 else 1


def build_sessions(args: argparse.Namespace) -> List[ReviewAgentSession]:
    sessions = []
    for index in range(args.sessions):
        role = ROLES[index % len(ROLES)]
        sessions.append(
            ReviewAgentSession(
                id=f"python-sdk-bench-session-{index}",
                role=role,
                objective=(
                    f"SDK memory benchmark as {role}: gather diff, file, and search "
                    "evidence, then record one concise benchmark finding."
                ),
                budget=ReviewAgentBudget(
                    max_turns=args.max_turns,
                    max_tool_calls=args.max_tool_calls,
                    max_prompt_tokens=32_000,
                    max_output_tokens=args.max_output_tokens,
                ),
            )
        )
    return sessions


def benchmark_failures(result: Any, error: Optional[str], sampler: "MemorySampler") -> List[str]:
    failures = []
    if error:
        failures.append(f"benchmark errored: {error}")
    if result is None:
        failures.append("no review result returned")
    else:
        if result.status != "completed":
            failures.append(f"review status was {result.status}")
        if result.coverage.files_reviewed == 0:
            failures.append("no files were reviewed")
    if sampler.peak_combined_rss_bytes == 0:
        failures.append("memory sampler recorded no RSS samples")
    return failures


def result_report(result: Any) -> Optional[Dict[str, Any]]:
    if result is None:
        return None
    return {
        "status": result.status,
        "conclusion": result.conclusion,
        "summary": result.summary,
        "findings": len(result.findings),
        "coverage": {
            "filesConsidered": result.coverage.files_considered,
            "filesReviewed": result.coverage.files_reviewed,
            "filesSkipped": result.coverage.files_skipped,
        },
        "metadata": result.metadata,
    }


class MemorySampler:
    def __init__(self, *, sample_ms: int) -> None:
        self.sample_ms = sample_ms
        self.started = time.perf_counter()
        self.samples: List[Dict[str, Any]] = []
        self.runner_pids: set[int] = set()
        self.peak_client_rss_bytes = 0
        self.peak_runner_rss_bytes = 0
        self.peak_combined_rss_bytes = 0
        self._task: Optional[asyncio.Task[None]] = None

    async def start(self) -> None:
        self.sample()
        self._task = asyncio.create_task(self._sample_loop())

    async def stop(self) -> None:
        if self._task is not None:
            self._task.cancel()
            await asyncio.gather(self._task, return_exceptions=True)
        self.sample()

    async def _sample_loop(self) -> None:
        while True:
            await asyncio.sleep(self.sample_ms / 1000)
            self.sample()

    def sample(self) -> None:
        table = process_table()
        client_rss_bytes = rss_bytes_for_pid(table, os.getpid())
        runners = runner_descendants(table, os.getpid())
        runner_rss_bytes = sum(process["rssBytes"] for process in runners)
        combined_rss_bytes = client_rss_bytes + runner_rss_bytes
        for process in runners:
            self.runner_pids.add(process["pid"])
        self.peak_client_rss_bytes = max(self.peak_client_rss_bytes, client_rss_bytes)
        self.peak_runner_rss_bytes = max(self.peak_runner_rss_bytes, runner_rss_bytes)
        self.peak_combined_rss_bytes = max(self.peak_combined_rss_bytes, combined_rss_bytes)
        self.samples.append(
            {
                "atMs": max(0, round((time.perf_counter() - self.started) * 1000)),
                "clientRssBytes": client_rss_bytes,
                "runnerRssBytes": runner_rss_bytes,
                "combinedRssBytes": combined_rss_bytes,
                "runnerPids": [process["pid"] for process in runners],
            }
        )

    def report(self) -> Dict[str, Any]:
        last = self.samples[-1] if self.samples else {}
        return {
            "peakClientRssBytes": self.peak_client_rss_bytes,
            "peakRunnerRssBytes": self.peak_runner_rss_bytes,
            "peakCombinedRssBytes": self.peak_combined_rss_bytes,
            "finalClientRssBytes": last.get("clientRssBytes", 0),
            "finalRunnerRssBytes": last.get("runnerRssBytes", 0),
            "finalCombinedRssBytes": last.get("combinedRssBytes", 0),
            "sampleCount": len(self.samples),
            "runnerPids": sorted(self.runner_pids),
            "samples": self.samples,
        }


def process_table() -> List[Dict[str, Any]]:
    output = subprocess.check_output(
        ["ps", "-axo", "pid=,ppid=,rss=,comm="],
        text=True,
        encoding="utf-8",
    )
    processes = []
    for line in output.splitlines():
        parts = line.strip().split(None, 3)
        if len(parts) != 4:
            continue
        pid, ppid, rss_kb, command = parts
        if not pid.isdigit() or not ppid.isdigit() or not rss_kb.isdigit():
            continue
        processes.append(
            {
                "pid": int(pid),
                "ppid": int(ppid),
                "rssBytes": int(rss_kb) * 1024,
                "command": command,
            }
        )
    return processes


def rss_bytes_for_pid(table: List[Dict[str, Any]], pid: int) -> int:
    for process in table:
        if process["pid"] == pid:
            return int(process["rssBytes"])
    return 0


def runner_descendants(table: List[Dict[str, Any]], parent_pid: int) -> List[Dict[str, Any]]:
    by_parent: Dict[int, List[Dict[str, Any]]] = {}
    for process in table:
        by_parent.setdefault(process["ppid"], []).append(process)
    descendants = []
    stack = list(by_parent.get(parent_pid, []))
    while stack:
        process = stack.pop()
        descendants.append(process)
        stack.extend(by_parent.get(process["pid"], []))
    return [
        process
        for process in descendants
        if "muzen-runner" in Path(process["command"]).name
    ]


def default_runner_path() -> str:
    env_path = os.environ.get("MUZEN_RUNNER_PATH")
    if env_path:
        return env_path
    for candidate in [
        REPO_ROOT / "target/release/muzen-runner",
        REPO_ROOT / "target/debug/muzen-runner",
    ]:
        if candidate.exists():
            return str(candidate)
    return "muzen-runner"


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run Python SDK local memory benchmark.")
    parser.add_argument("--repo", default=".")
    parser.add_argument("--sessions", type=positive_int, default=50)
    parser.add_argument("--max-active", type=positive_int)
    parser.add_argument("--max-turns", type=positive_int, default=4)
    parser.add_argument("--max-tool-calls", type=positive_int, default=8)
    parser.add_argument("--max-output-tokens", type=positive_int, default=512)
    parser.add_argument("--max-file-kb", type=positive_int, default=200)
    parser.add_argument("--max-search-matches", type=positive_int, default=120)
    parser.add_argument("--changed-file", action="append", default=[])
    parser.add_argument("--runner-path")
    parser.add_argument("--hold-ms", type=non_negative_int, default=1000)
    parser.add_argument("--sample-ms", type=positive_int, default=25)
    parser.add_argument("--output")
    return parser.parse_args()


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return parsed


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be a non-negative integer")
    return parsed


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
