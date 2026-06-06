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
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Awaitable, Callable, Dict, List, Optional

SCHEMA_VERSION = "muzen.sdk-memory-benchmark.v1"
RUNNER_PROTOCOL_VERSION = "muzen.runner.v1"
REPO_ROOT = Path(__file__).resolve().parents[2]
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
    api_key = os.environ.get(args.api_key_env)
    if not api_key:
        raise RuntimeError(f"{args.api_key_env} is not set")
    runner_path = args.runner_path or default_runner_path()
    repo = str(Path(args.repo).resolve())
    llm = OpenAiCompatibleCallback(
        base_url=args.base_url,
        api_key=api_key,
        model=args.model,
        max_output_tokens=args.max_output_tokens,
    )
    sampler = MemorySampler(sample_ms=args.sample_ms)
    started_at_utc = utc_now()
    started = time.perf_counter()
    client = None
    result = None
    error_message: Optional[str] = None

    await sampler.start()
    try:
        client = await JsonRpcRunnerClient.start(
            runner_path,
            callbacks={"model.complete": llm.complete},
        )
        await client.request(
            "runner.handshake",
            {
                "protocolVersion": RUNNER_PROTOCOL_VERSION,
                "clientName": "python-real-sdk-memory-bench",
            },
        )
        sampler.sample()
        result = await client.request(
            "run.start",
            {
                "protocolVersion": RUNNER_PROTOCOL_VERSION,
                "runId": f"python-real-sdk-memory-{int(time.time() * 1000)}",
                "repo": repo,
                "source": {
                    "type": "local",
                    "repo": repo,
                    "changedFiles": args.changed_file,
                },
                "changedFiles": args.changed_file,
                "model": {"callback": True},
                "sessions": build_sessions(args),
                "limits": {
                    "maxActiveSessions": args.max_active or args.sessions,
                    "maxFileBytes": args.max_file_kb * 1024,
                    "maxSearchMatches": args.max_search_matches,
                },
            },
        )
        if args.hold_ms > 0:
            await asyncio.sleep(args.hold_ms / 1000)
        sampler.sample()
    except Exception as error:  # noqa: BLE001 - benchmark reports failure as JSON.
        error_message = str(error)
    finally:
        await sampler.stop()
        if client is not None:
            await client.close()

    elapsed_ms = max(1, int((time.perf_counter() - started) * 1000 + 0.999))
    failures = benchmark_failures(result, error_message, sampler, llm)
    report = {
        "schemaVersion": SCHEMA_VERSION,
        "sdk": {
            "language": "python",
            "package": "muzen runner-callback",
            "runtime": f"python {platform.python_version()}",
        },
        "mode": "local-runner-stdio-real-model-callback",
        "runner": {"path": runner_path},
        "provider": {
            "baseUrl": redact_url(args.base_url),
            "model": args.model,
            "apiKeyEnv": args.api_key_env,
        },
        "workload": {
            "repo": repo,
            "sessions": args.sessions,
            "maxActiveSessions": args.max_active or args.sessions,
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
        "modelCallbacks": llm.report(),
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


class JsonRpcRunnerClient:
    def __init__(
        self,
        process: asyncio.subprocess.Process,
        callbacks: Dict[str, Callable[[Any], Awaitable[Any]]],
    ) -> None:
        self._process = process
        self._callbacks = callbacks
        self._pending: Dict[int, asyncio.Future[Any]] = {}
        self._next_request_id = 1
        self._closed = False
        self._reader_task = asyncio.create_task(self._read_loop())

    @classmethod
    async def start(
        cls,
        runner_path: str,
        callbacks: Dict[str, Callable[[Any], Awaitable[Any]]],
    ) -> "JsonRpcRunnerClient":
        process = await asyncio.create_subprocess_exec(
            runner_path,
            "stdio",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        return cls(process, callbacks)

    async def request(self, method: str, params: Any = None) -> Any:
        if self._process.stdin is None:
            raise RuntimeError("muzen-runner stdin is closed")
        request_id = self._next_request_id
        self._next_request_id += 1
        future = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        await self._write({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return await future

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._process.stdin is not None:
            self._process.stdin.close()
        if self._process.returncode is None:
            self._process.terminate()
            try:
                await asyncio.wait_for(self._process.wait(), timeout=2)
            except asyncio.TimeoutError:
                self._process.kill()
                await self._process.wait()
        self._reader_task.cancel()
        await asyncio.gather(self._reader_task, return_exceptions=True)
        for future in self._pending.values():
            if not future.done():
                future.set_exception(RuntimeError("runner client closed"))
        self._pending.clear()

    async def _read_loop(self) -> None:
        assert self._process.stdout is not None
        try:
            while True:
                line = await self._process.stdout.readline()
                if not line:
                    break
                await self._handle_frame(json.loads(line.decode("utf-8")))
        except asyncio.CancelledError:
            raise
        except Exception as error:
            for future in self._pending.values():
                if not future.done():
                    future.set_exception(error)
            self._pending.clear()

    async def _handle_frame(self, frame: Dict[str, Any]) -> None:
        if "method" in frame and "id" in frame:
            await self._handle_callback(frame)
            return
        if "method" in frame:
            return
        request_id = frame.get("id")
        future = self._pending.pop(request_id, None)
        if future is None:
            return
        if frame.get("error"):
            future.set_exception(RuntimeError(frame["error"].get("message", "runner request failed")))
        else:
            future.set_result(frame.get("result"))

    async def _handle_callback(self, frame: Dict[str, Any]) -> None:
        callback = self._callbacks.get(frame["method"])
        if callback is None:
            await self._write(
                {
                    "jsonrpc": "2.0",
                    "id": frame.get("id"),
                    "error": {
                        "code": -32601,
                        "message": f"unknown callback {frame['method']}",
                        "data": {"kind": "method_not_found"},
                    },
                }
            )
            return
        try:
            result = await callback(frame.get("params"))
            await self._write({"jsonrpc": "2.0", "id": frame.get("id"), "result": result})
        except Exception as error:
            await self._write(
                {
                    "jsonrpc": "2.0",
                    "id": frame.get("id"),
                    "error": {
                        "code": -32002,
                        "message": str(error),
                        "data": {"kind": "runner_error"},
                    },
                }
            )

    async def _write(self, frame: Dict[str, Any]) -> None:
        if self._process.stdin is None:
            raise RuntimeError("muzen-runner stdin is closed")
        self._process.stdin.write((json.dumps(frame) + "\n").encode("utf-8"))
        await self._process.stdin.drain()


class OpenAiCompatibleCallback:
    def __init__(self, *, base_url: str, api_key: str, model: str, max_output_tokens: int) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.model = model
        self.max_output_tokens = max_output_tokens
        self.calls = 0
        self.input_tokens = 0
        self.output_tokens = 0
        self.total_tokens = 0
        self.errors = 0
        self.error_messages: List[str] = []

    async def complete(self, params: Any) -> Dict[str, Any]:
        return await asyncio.to_thread(self._complete_sync, params or {})

    def _complete_sync(self, params: Dict[str, Any]) -> Dict[str, Any]:
        self.calls += 1
        payload = {
            "model": self.model,
            "temperature": 0,
            "max_tokens": self.max_output_tokens,
            "messages": transcript_messages(params.get("transcript") or [])
            + [
                {
                    "role": "user",
                    "content": (
                        "For this live SDK memory benchmark, respond with one concise "
                        "sentence and do not request tools."
                    ),
                }
            ],
        }
        request = urllib.request.Request(
            f"{self.base_url}/chat/completions",
            data=json.dumps(payload).encode("utf-8"),
            headers={
                "Authorization": f"Bearer {self.api_key}",
                "Content-Type": "application/json",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                body = json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as error:
            body = error.read().decode("utf-8", errors="replace")
            self.errors += 1
            message = f"model request failed: {error.code} {body[:200]}"
            self.error_messages.append(message[:300])
            raise RuntimeError(message) from error
        except Exception as error:
            self.errors += 1
            self.error_messages.append(str(error)[:300])
            raise
        usage = body.get("usage") or {}
        input_tokens = usage.get("prompt_tokens") or usage.get("input_tokens") or 0
        output_tokens = usage.get("completion_tokens") or usage.get("output_tokens") or 0
        total_tokens = usage.get("total_tokens") or input_tokens + output_tokens
        self.input_tokens += input_tokens
        self.output_tokens += output_tokens
        self.total_tokens += total_tokens
        return {
            "content": (((body.get("choices") or [{}])[0].get("message") or {}).get("content"))
            or "Live model benchmark completed.",
            "usage": {
                "inputTokens": input_tokens,
                "outputTokens": output_tokens,
                "totalTokens": total_tokens,
            },
        }

    def report(self) -> Dict[str, int]:
        return {
            "calls": self.calls,
            "inputTokens": self.input_tokens,
            "outputTokens": self.output_tokens,
            "totalTokens": self.total_tokens,
            "errors": self.errors,
            "errorMessages": self.error_messages,
        }


def transcript_messages(items: List[Dict[str, Any]]) -> List[Dict[str, str]]:
    messages = []
    for item in items:
        if item.get("kind") == "system":
            messages.append({"role": "system", "content": item.get("content") or ""})
        elif item.get("kind") == "user":
            messages.append({"role": "user", "content": item.get("content") or ""})
        elif item.get("kind") == "assistant_text":
            messages.append({"role": "assistant", "content": item.get("content") or ""})
    if not messages:
        messages.append({"role": "user", "content": "Run a concise live SDK memory benchmark response."})
    return messages[-8:]


def build_sessions(args: argparse.Namespace) -> List[Dict[str, Any]]:
    sessions = []
    for index in range(args.sessions):
        role = ROLES[index % len(ROLES)]
        sessions.append(
            {
                "id": f"python-real-sdk-bench-session-{index}",
                "role": role,
                "objective": f"Live SDK memory benchmark as {role}; produce one concise benchmark response.",
                "budget": {
                    "maxTurns": args.max_turns,
                    "maxToolCalls": args.max_tool_calls,
                    "maxPromptTokens": 32_000,
                    "maxOutputTokens": args.max_output_tokens,
                },
            }
        )
    return sessions


def benchmark_failures(
    result: Optional[Dict[str, Any]],
    error: Optional[str],
    sampler: "MemorySampler",
    llm: OpenAiCompatibleCallback,
) -> List[str]:
    failures = []
    if error:
        failures.append(f"benchmark errored: {error}")
    if result is None:
        failures.append("no run result returned")
    else:
        if result.get("status") != "completed":
            failures.append(f"run status was {result.get('status')}")
        summary = result.get("summary") or {}
        if summary.get("completedSessions") != summary.get("sessions"):
            failures.append(f"only {summary.get('completedSessions', 0)}/{summary.get('sessions', 0)} sessions completed")
    if llm.calls == 0:
        failures.append("no live model callbacks were recorded")
    if llm.errors > 0:
        failures.append(f"{llm.errors} live model callback(s) failed")
    if sampler.peak_combined_rss_bytes == 0:
        failures.append("memory sampler recorded no RSS samples")
    return failures


def result_report(result: Optional[Dict[str, Any]]) -> Optional[Dict[str, Any]]:
    if result is None:
        return None
    return {
        "status": result.get("status"),
        "summary": result.get("summary"),
        "findings": len(result.get("findings") or []),
        "snapshots": result.get("snapshots") or [],
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
    output = subprocess.check_output(["ps", "-axo", "pid=,ppid=,rss=,comm="], text=True)
    processes = []
    for line in output.splitlines():
        parts = line.strip().split(None, 3)
        if len(parts) != 4:
            continue
        pid, ppid, rss_kb, command = parts
        if pid.isdigit() and ppid.isdigit() and rss_kb.isdigit():
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
    return [process for process in descendants if "muzen-runner" in Path(process["command"]).name]


def default_runner_path() -> str:
    env_path = os.environ.get("MUZEN_RUNNER_PATH")
    if env_path:
        return env_path
    for candidate in [REPO_ROOT / "target/release/muzen-runner", REPO_ROOT / "target/debug/muzen-runner"]:
        if candidate.exists():
            return str(candidate)
    return "muzen-runner"


def redact_url(url: str) -> str:
    return url.split("@")[-1] if "@" in url else url.rstrip("/")


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run Python SDK real model callback memory benchmark.")
    parser.add_argument("--repo", default=".")
    parser.add_argument("--sessions", type=positive_int, default=2)
    parser.add_argument("--max-active", type=positive_int)
    parser.add_argument("--max-turns", type=positive_int, default=1)
    parser.add_argument("--max-tool-calls", type=positive_int, default=1)
    parser.add_argument("--max-output-tokens", type=positive_int, default=48)
    parser.add_argument("--max-file-kb", type=positive_int, default=200)
    parser.add_argument("--max-search-matches", type=positive_int, default=120)
    parser.add_argument("--changed-file", action="append", default=[])
    parser.add_argument("--runner-path")
    parser.add_argument("--hold-ms", type=non_negative_int, default=1000)
    parser.add_argument("--sample-ms", type=positive_int, default=50)
    parser.add_argument("--output")
    parser.add_argument("--api-key-env", default="AI_API_KEY" if os.environ.get("AI_API_KEY") else "OPENAI_API_KEY")
    parser.add_argument(
        "--base-url",
        default=os.environ.get("AI_BASE_URL")
        or os.environ.get("OPENAI_BASE_URL")
        or os.environ.get("OAI_BASE_URL")
        or "https://api.openai.com/v1",
    )
    parser.add_argument("--model", default=os.environ.get("OPENAI_REVIEW_MODEL") or os.environ.get("AI_MODEL") or "gpt-4o-mini")
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
