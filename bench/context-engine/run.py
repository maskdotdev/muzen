#!/usr/bin/env python3
"""Run deterministic Context Engine retrieval evaluations.

The harness intentionally drives the public `muzen context query` CLI so the
reported metrics reflect the same surface developers and hosted workers use.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CASES = ROOT / "bench" / "context-engine" / "cases"
DEFAULT_OUTPUT = ROOT / "bench" / "results-context-engine" / "context-engine-summary.json"


@dataclass(frozen=True)
class CaseResult:
    id: str
    kind: str
    recall: float
    precision: float
    useful_evidence_per_1k_tokens: float
    latency_ms: float
    expected_paths: list[str]
    retrieved_paths: list[str]
    missed_paths: list[str]
    unexpected_paths: list[str]
    token_estimate: int
    omitted: int


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cases-dir",
        type=Path,
        default=DEFAULT_CASES,
        help="Directory containing context eval case JSON files.",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help="Path for the JSON summary artifact.",
    )
    parser.add_argument(
        "--muzen-bin",
        type=Path,
        help="Existing muzen binary. Defaults to cargo run --bin muzen.",
    )
    return parser.parse_args()


def load_case_files(cases_dir: Path) -> list[dict[str, Any]]:
    case_files = sorted(cases_dir.glob("*.json"))
    if not case_files:
        raise SystemExit(f"no case files found in {cases_dir}")
    return [json.loads(path.read_text()) for path in case_files]


def run_query(case_file: dict[str, Any], case: dict[str, Any], muzen_bin: Path | None) -> tuple[dict[str, Any], float]:
    repo = ROOT / case_file["repo"]
    if muzen_bin:
        command = [str(muzen_bin), "context", "query"]
    else:
        command = ["cargo", "run", "--quiet", "--bin", "muzen", "--", "context", "query"]
    command.extend(["--repo", str(repo)])
    for changed_file in case_file["changedFiles"]:
        command.extend(["--changed-file", changed_file])
    command.extend(["--kind", case["kind"], "--max-results", str(case.get("maxResults", 20))])
    if "path" in case:
        command.extend(["--path", case["path"]])
    if "query" in case:
        command.extend(["--query", case["query"]])

    started = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=ROOT,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    latency_ms = (time.perf_counter() - started) * 1000
    if completed.returncode != 0:
        raise SystemExit(
            f"context query failed for {case['id']} with code {completed.returncode}\n"
            f"stderr:\n{completed.stderr}"
        )
    return json.loads(completed.stdout), latency_ms


def score_case(case_file: dict[str, Any], case: dict[str, Any], muzen_bin: Path | None) -> CaseResult:
    result, latency_ms = run_query(case_file, case, muzen_bin)
    evidence = result.get("evidence", [])
    expected_paths = list(dict.fromkeys(case["expectedPaths"]))
    retrieved_paths = [entry["path"] for entry in evidence if entry.get("path")]
    retrieved_unique = list(dict.fromkeys(retrieved_paths))
    expected_set = set(expected_paths)
    retrieved_set = set(retrieved_unique)
    hit_count = len(expected_set & retrieved_set)
    missed_paths = sorted(expected_set - retrieved_set)
    unexpected_paths = sorted(retrieved_set - expected_set)
    token_estimate = sum(int(entry.get("tokenEstimate", 0)) for entry in evidence)
    useful_per_1k = (hit_count / token_estimate * 1000) if token_estimate else 0.0
    return CaseResult(
        id=case["id"],
        kind=case["kind"],
        recall=hit_count / len(expected_set) if expected_set else 1.0,
        precision=hit_count / len(retrieved_set) if retrieved_set else 0.0,
        useful_evidence_per_1k_tokens=useful_per_1k,
        latency_ms=latency_ms,
        expected_paths=expected_paths,
        retrieved_paths=retrieved_unique,
        missed_paths=missed_paths,
        unexpected_paths=unexpected_paths,
        token_estimate=token_estimate,
        omitted=int(result.get("omitted", 0)),
    )


def summarize(results: list[CaseResult]) -> dict[str, Any]:
    count = len(results)
    failures = [result.id for result in results if result.missed_paths]
    return {
        "schemaVersion": "muzen.context-eval-summary.v1",
        "generatedAtUnixMs": int(time.time() * 1000),
        "caseCount": count,
        "ok": not failures,
        "failures": failures,
        "metrics": {
            "meanRecall": sum(result.recall for result in results) / count,
            "meanPrecision": sum(result.precision for result in results) / count,
            "meanUsefulEvidencePer1kTokens": sum(
                result.useful_evidence_per_1k_tokens for result in results
            )
            / count,
            "meanLatencyMs": sum(result.latency_ms for result in results) / count,
            "maxLatencyMs": max(result.latency_ms for result in results),
            "totalOmittedCandidates": sum(result.omitted for result in results),
        },
        "cases": [result.__dict__ for result in results],
    }


def main() -> int:
    args = parse_args()
    case_files = load_case_files(args.cases_dir)
    results = [
        score_case(case_file, case, args.muzen_bin)
        for case_file in case_files
        for case in case_file["cases"]
    ]
    summary = summarize(results)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(summary, indent=2) + "\n")

    metrics = summary["metrics"]
    print(
        "context-engine eval: "
        f"{summary['caseCount']} cases, "
        f"mean recall {metrics['meanRecall']:.3f}, "
        f"mean precision {metrics['meanPrecision']:.3f}, "
        f"mean latency {metrics['meanLatencyMs']:.1f} ms"
    )
    if summary["failures"]:
        print("failed cases: " + ", ".join(summary["failures"]), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
