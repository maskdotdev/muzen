#!/usr/bin/env python3
"""Run deterministic Context Engine retrieval evaluations.

The harness intentionally drives the public `muzen context` CLI so the
reported metrics reflect the same surface developers and hosted workers use.

Case files use schema `muzen.context-eval-case.v2`:

- `repoSource` is either `{"kind": "fixture", "path": "fixtures/..."}` for a
  vendored fixture tree, or `{"kind": "git", "commit": "<sha>", "origin": ...}`
  for a pinned commit cloned into the corpus cache. `origin` is `"self"` for
  this repository or a cloneable path/URL for an external corpus repository.
- Cases with `"strict": true` fail the run on any missed expected path or
  range. Non-strict (mined) cases are graded with ranking metrics instead.
- Redaction and prompt-injection violations always fail the run.

A committed baseline (`baseline.json`) gates ranking regressions: the run
fails when recall@10 or nDCG@10 drops more than `--tolerance` below it.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from math import log2
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CASES = ROOT / "bench" / "context-engine" / "cases"
DEFAULT_OUTPUT = ROOT / "bench" / "results-context-engine" / "context-engine-summary.json"
DEFAULT_BASELINE = ROOT / "bench" / "context-engine" / "baseline.json"
CORPUS_CACHE = ROOT / "bench" / "context-engine" / "corpus"
CASE_SCHEMA_VERSION = "muzen.context-eval-case.v2"
SUMMARY_SCHEMA_VERSION = "muzen.context-eval-summary.v2"
GATED_METRICS = ("meanRecallAt10", "meanNdcgAt10")


@dataclass(frozen=True)
class CaseResult:
    id: str
    kind: str
    strict: bool
    recall: float
    precision: float
    recall_at_5: float
    recall_at_10: float
    recall_at_25: float
    ndcg_at_10: float
    tokens_to_first_relevant: int | None
    secret_redaction_correct: bool
    prompt_injection_resistant: bool
    useful_evidence_per_1k_tokens: float
    latency_ms: float
    expected_paths: list[str]
    retrieved_paths: list[str]
    missed_paths: list[str]
    unexpected_paths: list[str]
    forbidden_content_hits: list[str]
    missing_required_content: list[str]
    trusted_forbidden_paths: list[str]
    missing_expected_ranges: list[dict[str, Any]]
    token_estimate: int
    omitted: int
    sufficiency_status: str | None
    sufficiency_blocking_gaps: int


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cases-dir",
        type=Path,
        default=DEFAULT_CASES,
        help="Directory containing context eval case JSON files (searched recursively).",
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
    parser.add_argument(
        "--baseline",
        type=Path,
        default=DEFAULT_BASELINE,
        help="Committed baseline metrics for the regression gate.",
    )
    parser.add_argument(
        "--tolerance",
        type=float,
        default=0.02,
        help="Allowed drop in gated metrics before the run fails.",
    )
    parser.add_argument(
        "--write-baseline",
        action="store_true",
        help="Write the run's metrics as the new baseline instead of gating.",
    )
    parser.add_argument(
        "--hosted-semantic",
        action="store_true",
        help=(
            "Force hosted embeddings for every case (R8 acceptance). "
            "Requires the embedding credential in the environment; the "
            "regression gate still applies, so hosted must beat or match "
            "the deterministic baseline."
        ),
    )
    parser.add_argument(
        "--hosted-embedding-model",
        default="text-embedding-3-small",
        help="Embedding model for --hosted-semantic runs.",
    )
    parser.add_argument(
        "--hosted-embedding-base-url",
        help="OpenAI-compatible embeddings endpoint for --hosted-semantic runs.",
    )
    parser.add_argument(
        "--hosted-max-embedding-inputs",
        type=int,
        default=4096,
        help="Embedding input cap for --hosted-semantic runs (covers all chunks).",
    )
    parser.add_argument(
        "--local-onnx-model-dir",
        type=Path,
        help=(
            "Force local ONNX transformer embeddings for every case (R8 "
            "local-tier evaluation): a directory holding model.onnx/"
            "model_quantized.onnx and tokenizer.json."
        ),
    )
    parser.add_argument(
        "--rerank-base-url",
        help=(
            "Enable the cross-encoder rerank stage against a Cohere-style "
            "/rerank endpoint (Cohere, Jina, or an in-house server)."
        ),
    )
    parser.add_argument(
        "--rerank-model",
        help="Rerank model id for --rerank-base-url runs.",
    )
    parser.add_argument(
        "--rerank-credential-ref",
        help="Credential reference (env:NAME) for the rerank endpoint.",
    )
    return parser.parse_args()


def load_case_files(cases_dir: Path) -> list[dict[str, Any]]:
    case_paths = sorted(cases_dir.rglob("*.json"))
    if not case_paths:
        raise SystemExit(f"no case files found in {cases_dir}")
    case_files = []
    for path in case_paths:
        case_file = json.loads(path.read_text())
        validate_case_file(case_file, path)
        case_files.append(case_file)
    return case_files


def validate_case_file(case_file: dict[str, Any], path: Path) -> None:
    if case_file.get("schemaVersion") != CASE_SCHEMA_VERSION:
        raise SystemExit(f"{path}: expected schemaVersion {CASE_SCHEMA_VERSION}")
    source = case_file.get("repoSource") or {}
    if source.get("kind") not in {"fixture", "git"}:
        raise SystemExit(f"{path}: repoSource.kind must be 'fixture' or 'git'")
    if source["kind"] == "fixture" and not source.get("path"):
        raise SystemExit(f"{path}: fixture repoSource requires 'path'")
    if source["kind"] == "git" and not source.get("commit"):
        raise SystemExit(f"{path}: git repoSource requires a pinned 'commit'")
    if source["kind"] == "git" and not source.get("origin"):
        raise SystemExit(
            f"{path}: git repoSource requires an 'origin' "
            "('self' or a cloneable path/URL)"
        )
    if not case_file.get("cases"):
        raise SystemExit(f"{path}: case file declares no cases")
    for case in case_file["cases"]:
        if not case.get("expectedPaths"):
            raise SystemExit(f"{path}: case {case.get('id')!r} has no ground-truth expectedPaths")


def resolve_origin(source: dict[str, Any]) -> str:
    """Resolve a git repoSource origin to a cloneable location. 'self' is
    this repository; anything else is a path (with ~ expansion) or URL."""
    origin = source["origin"]
    if origin == "self":
        return str(ROOT)
    expanded = Path(origin).expanduser()
    if expanded.exists():
        return str(expanded)
    if "://" in origin or origin.startswith("git@"):
        return origin
    raise SystemExit(
        f"repoSource origin {origin!r} not found at {expanded}; "
        "clone the corpus repository there or update the case's origin"
    )


def materialize_repo(source: dict[str, Any]) -> Path:
    """Clone (not archive) so the checkout keeps .git: the engine mines
    co-change signal from the pinned commit's history (R4)."""
    if source["kind"] == "fixture":
        return ROOT / source["path"]
    commit = source["commit"]
    target = CORPUS_CACHE / commit[:12]
    if (target / ".git").exists():
        return target
    if target.exists():
        # Pre-R4 archive materialization without history: rebuild.
        shutil.rmtree(target)
    subprocess.run(
        ["git", "clone", "--quiet", "--no-checkout", resolve_origin(source), str(target)],
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(target), "checkout", "--quiet", "--detach", commit],
        check=True,
    )
    return target


def materialize_diff(source: dict[str, Any]) -> Path | None:
    """Write the pinned commit's unified diff next to the checkout. Hunks
    anchor changed-span detection and sufficiency coverage (R6)."""
    if source["kind"] != "git":
        return None
    commit = source["commit"]
    repo = materialize_repo(source)
    diff_path = CORPUS_CACHE / f"{commit[:12]}.diff"
    if not diff_path.exists():
        show = subprocess.run(
            ["git", "-C", str(repo), "show", commit, "--format=", "--no-color"],
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        )
        diff_path.write_text(show.stdout)
    return diff_path


def base_command(muzen_bin: Path | None) -> list[str]:
    if muzen_bin:
        return [str(muzen_bin)]
    return ["cargo", "run", "--quiet", "--bin", "muzen", "--"]


def run_context_case(
    case_file: dict[str, Any], case: dict[str, Any], args: argparse.Namespace
) -> tuple[dict[str, Any], float]:
    repo = materialize_repo(case_file["repoSource"])
    command = base_command(args.muzen_bin)
    command.append("context")
    if case.get("command") == "pack":
        command.append("pack")
    else:
        command.append("query")
    command.extend(["--repo", str(repo)])
    # Durable derived-data cache (R9): repeated invocations over the same
    # checkout re-derive only what changed. Keyed per corpus checkout.
    cache_root = CORPUS_CACHE / "derived" / repo.name
    cache_root.mkdir(parents=True, exist_ok=True)
    command.extend(["--derived-cache-root", str(cache_root)])
    for changed_file in case_file["changedFiles"]:
        command.extend(["--changed-file", changed_file])
    diff_path = materialize_diff(case_file["repoSource"])
    if diff_path is not None:
        command.extend(["--diff-file", str(diff_path)])
    if args.local_onnx_model_dir:
        # Forced local ONNX mode (R8 local-tier evaluation) supersedes
        # per-case semantic flags.
        command.append("--local-onnx-semantic")
        command.extend(["--onnx-model-dir", str(args.local_onnx_model_dir)])
        command.extend(
            ["--max-embedding-inputs", str(args.hosted_max_embedding_inputs)]
        )
    elif args.hosted_semantic:
        # Forced hosted mode (R8 acceptance) supersedes per-case semantic
        # flags: every case retrieves with real embeddings.
        command.append("--hosted-semantic")
        command.extend(
            ["--max-embedding-inputs", str(args.hosted_max_embedding_inputs)]
        )
        command.extend(["--hosted-embedding-model", args.hosted_embedding_model])
        if args.hosted_embedding_base_url:
            command.extend(
                ["--hosted-embedding-base-url", args.hosted_embedding_base_url]
            )
    elif case.get("localSemantic"):
        command.append("--local-semantic")
        command.extend(
            [
                "--max-embedding-inputs",
                str(case.get("maxEmbeddingInputs", 512)),
            ]
        )
    elif case.get("hostedSemantic"):
        command.append("--hosted-semantic")
        command.extend(
            [
                "--max-embedding-inputs",
                str(case.get("maxEmbeddingInputs", 512)),
            ]
        )
        if case.get("hostedEmbeddingBaseUrl"):
            command.extend(["--hosted-embedding-base-url", case["hostedEmbeddingBaseUrl"]])
        if case.get("hostedEmbeddingModel"):
            command.extend(["--hosted-embedding-model", case["hostedEmbeddingModel"]])
        if case.get("hostedEmbeddingCredentialRef"):
            command.extend(
                ["--hosted-embedding-credential-ref", case["hostedEmbeddingCredentialRef"]]
            )
    if args.rerank_base_url:
        command.extend(["--rerank", "--rerank-base-url", args.rerank_base_url])
        if args.rerank_model:
            command.extend(["--rerank-model", args.rerank_model])
        if args.rerank_credential_ref:
            command.extend(["--rerank-credential-ref", args.rerank_credential_ref])
    if case.get("command") == "pack":
        command.extend(["--purpose", case.get("purpose", "general-review")])
        command.extend(["--max-tokens", str(case.get("maxTokens", 12000))])
    else:
        command.extend(["--kind", case["kind"], "--max-results", str(case.get("maxResults", 20))])
        if "path" in case:
            command.extend(["--path", case["path"]])
        if "query" in case:
            command.extend(["--query", case["query"]])
        if "startLine" in case:
            command.extend(["--start-line", str(case["startLine"])])
        if "endLine" in case:
            command.extend(["--end-line", str(case["endLine"])])

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
            f"context case failed for {case['id']} with code {completed.returncode}\n"
            f"stderr:\n{completed.stderr}"
        )
    return json.loads(completed.stdout), latency_ms


def recall_at_k(retrieved: list[str], expected: set[str], k: int) -> float:
    if not expected:
        return 1.0
    return len(set(retrieved[:k]) & expected) / len(expected)


def ndcg_at_k(retrieved: list[str], expected: set[str], k: int) -> float:
    if not expected:
        return 1.0
    dcg = sum(
        1.0 / log2(position + 2)
        for position, path in enumerate(retrieved[:k])
        if path in expected
    )
    ideal = sum(1.0 / log2(position + 2) for position in range(min(len(expected), k)))
    return dcg / ideal if ideal else 0.0


def tokens_to_first_relevant(evidence: list[dict[str, Any]], expected: set[str]) -> int | None:
    consumed = 0
    for entry in evidence:
        consumed += int(entry.get("tokenEstimate", 0))
        if entry.get("path") in expected:
            return consumed
    return None


def score_case(
    case_file: dict[str, Any], case: dict[str, Any], args: argparse.Namespace
) -> CaseResult:
    result, latency_ms = run_context_case(case_file, case, args)
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
    serialized_result = json.dumps(result, sort_keys=True)
    forbidden_content_hits = [
        text for text in case.get("forbiddenContent", []) if text in serialized_result
    ]
    missing_required_content = [
        text for text in case.get("requiredContent", []) if text not in serialized_result
    ]
    forbidden_trusted_paths = set(case.get("forbiddenTrustedPaths", []))
    trusted_forbidden_paths = sorted(
        {
            entry.get("path")
            for entry in evidence
            if entry.get("path") in forbidden_trusted_paths
            and entry.get("trust") in {"kernel", "host_trusted"}
        }
    )
    missing_expected_ranges = [
        expected
        for expected in case.get("expectedRanges", [])
        if not any(evidence_matches_range(entry, expected) for entry in evidence)
    ]
    omitted = result.get("omittedCandidates", result.get("omitted", 0))
    # Ranking metrics grade context retrieval BEYOND the diff: the changed
    # files are the query, not the answer, so they neither earn nor occupy
    # ranked slots. Recall/precision/strict gating keep the full sets.
    changed_set = set(case_file.get("changedFiles", []))
    eval_expected = expected_set - changed_set
    eval_retrieved = [path for path in retrieved_unique if path not in changed_set]
    eval_evidence = [
        entry for entry in evidence if entry.get("path") not in changed_set
    ]
    if eval_expected:
        ranked_recall_at_5 = recall_at_k(eval_retrieved, eval_expected, 5)
        ranked_recall_at_10 = recall_at_k(eval_retrieved, eval_expected, 10)
        ranked_recall_at_25 = recall_at_k(eval_retrieved, eval_expected, 25)
        ranked_ndcg_at_10 = ndcg_at_k(eval_retrieved, eval_expected, 10)
        first_relevant_tokens = tokens_to_first_relevant(eval_evidence, eval_expected)
    else:
        # Every expected path is itself a changed file (fixture invariant
        # cases): grade against the full sets.
        ranked_recall_at_5 = recall_at_k(retrieved_unique, expected_set, 5)
        ranked_recall_at_10 = recall_at_k(retrieved_unique, expected_set, 10)
        ranked_recall_at_25 = recall_at_k(retrieved_unique, expected_set, 25)
        ranked_ndcg_at_10 = ndcg_at_k(retrieved_unique, expected_set, 10)
        first_relevant_tokens = tokens_to_first_relevant(evidence, expected_set)
    sufficiency = result.get("sufficiency") or {}
    sufficiency_blocking_gaps = sum(
        1
        for gap in sufficiency.get("gaps", [])
        if any(kind != "no_related_tests" for kind in gap.get("missing", []))
    )
    return CaseResult(
        id=case["id"],
        kind=case.get("kind", case.get("command", "query")),
        strict=bool(case.get("strict")),
        recall=hit_count / len(expected_set) if expected_set else 1.0,
        precision=hit_count / len(retrieved_set) if retrieved_set else 0.0,
        recall_at_5=ranked_recall_at_5,
        recall_at_10=ranked_recall_at_10,
        recall_at_25=ranked_recall_at_25,
        ndcg_at_10=ranked_ndcg_at_10,
        tokens_to_first_relevant=first_relevant_tokens,
        secret_redaction_correct=not forbidden_content_hits and not missing_required_content,
        prompt_injection_resistant=not trusted_forbidden_paths,
        useful_evidence_per_1k_tokens=useful_per_1k,
        latency_ms=latency_ms,
        expected_paths=expected_paths,
        retrieved_paths=retrieved_unique,
        missed_paths=missed_paths,
        unexpected_paths=unexpected_paths,
        forbidden_content_hits=forbidden_content_hits,
        missing_required_content=missing_required_content,
        trusted_forbidden_paths=trusted_forbidden_paths,
        missing_expected_ranges=missing_expected_ranges,
        token_estimate=token_estimate,
        omitted=len(omitted) if isinstance(omitted, list) else int(omitted),
        sufficiency_status=sufficiency.get("status"),
        sufficiency_blocking_gaps=sufficiency_blocking_gaps,
    )


def evidence_matches_range(entry: dict[str, Any], expected: dict[str, Any]) -> bool:
    expected_kind = expected.get("kind")
    if expected_kind is not None and entry.get("kind") != expected_kind:
        return False
    if entry.get("path") != expected.get("path"):
        return False
    range_value = entry.get("range") or {}
    return (
        range_value.get("startLine") == expected.get("startLine")
        and range_value.get("endLine") == expected.get("endLine")
    )


def summarize(results: list[CaseResult]) -> dict[str, Any]:
    count = len(results)
    failures = sorted(
        {
            result.id
            for result in results
            if not result.secret_redaction_correct
            or not result.prompt_injection_resistant
            or (result.strict and (result.missed_paths or result.missing_expected_ranges))
        }
    )
    first_relevant = [
        result.tokens_to_first_relevant
        for result in results
        if result.tokens_to_first_relevant is not None
    ]
    # Sufficiency calibration (R6): packs missing ground truth should
    # report blocking gaps; packs containing all ground truth should not
    # report insufficient.
    with_sufficiency = [r for r in results if r.sufficiency_status is not None]
    incomplete = [r for r in with_sufficiency if r.missed_paths]
    complete = [r for r in with_sufficiency if not r.missed_paths]
    gap_recall = (
        sum(
            1
            for r in incomplete
            if r.sufficiency_blocking_gaps > 0 or r.sufficiency_status == "insufficient"
        )
        / len(incomplete)
        if incomplete
        else None
    )
    sufficient_when_complete = (
        sum(1 for r in complete if r.sufficiency_status != "insufficient") / len(complete)
        if complete
        else None
    )
    return {
        "schemaVersion": SUMMARY_SCHEMA_VERSION,
        "generatedAtUnixMs": int(time.time() * 1000),
        "caseCount": count,
        "ok": not failures,
        "failures": failures,
        "metrics": {
            "meanRecall": sum(result.recall for result in results) / count,
            "meanPrecision": sum(result.precision for result in results) / count,
            "meanRecallAt5": sum(result.recall_at_5 for result in results) / count,
            "meanRecallAt10": sum(result.recall_at_10 for result in results) / count,
            "meanRecallAt25": sum(result.recall_at_25 for result in results) / count,
            "meanNdcgAt10": sum(result.ndcg_at_10 for result in results) / count,
            "firstRelevantRate": len(first_relevant) / count,
            "meanTokensToFirstRelevant": (
                sum(first_relevant) / len(first_relevant) if first_relevant else None
            ),
            "secretRedactionCorrectRate": sum(
                1 for result in results if result.secret_redaction_correct
            )
            / count,
            "promptInjectionResistanceRate": sum(
                1 for result in results if result.prompt_injection_resistant
            )
            / count,
            "meanUsefulEvidencePer1kTokens": sum(
                result.useful_evidence_per_1k_tokens for result in results
            )
            / count,
            "meanLatencyMs": sum(result.latency_ms for result in results) / count,
            "maxLatencyMs": max(result.latency_ms for result in results),
            "totalOmittedCandidates": sum(result.omitted for result in results),
            "sufficiencyGapRecall": gap_recall,
            "sufficiencySufficientWhenComplete": sufficient_when_complete,
        },
        "cases": [result.__dict__ for result in results],
    }


def check_regression(
    summary: dict[str, Any], baseline_path: Path, tolerance: float
) -> list[str]:
    if not baseline_path.exists():
        return []
    baseline = json.loads(baseline_path.read_text())["metrics"]
    regressions = []
    for metric in GATED_METRICS:
        if metric not in baseline:
            continue
        current = summary["metrics"][metric]
        floor = baseline[metric] - tolerance
        if current < floor:
            regressions.append(
                f"{metric} regressed: {current:.4f} < baseline {baseline[metric]:.4f} - "
                f"tolerance {tolerance}"
            )
    return regressions


def write_baseline(summary: dict[str, Any], baseline_path: Path) -> None:
    baseline = {
        "schemaVersion": "muzen.context-eval-baseline.v1",
        "caseCount": summary["caseCount"],
        "metrics": {
            metric: summary["metrics"][metric]
            for metric in (
                "meanRecall",
                "meanPrecision",
                "meanRecallAt5",
                "meanRecallAt10",
                "meanRecallAt25",
                "meanNdcgAt10",
                "firstRelevantRate",
                "meanTokensToFirstRelevant",
                "meanUsefulEvidencePer1kTokens",
            )
        },
    }
    baseline_path.write_text(json.dumps(baseline, indent=2) + "\n")


def main() -> int:
    args = parse_args()
    case_files = load_case_files(args.cases_dir)
    results = [
        score_case(case_file, case, args)
        for case_file in case_files
        for case in case_file["cases"]
    ]
    summary = summarize(results)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(summary, indent=2) + "\n")

    metrics = summary["metrics"]
    first_relevant = metrics["meanTokensToFirstRelevant"]
    first_relevant_text = f"{first_relevant:.0f}" if first_relevant is not None else "n/a"
    print(
        "context-engine eval: "
        f"{summary['caseCount']} cases, "
        f"recall@10 {metrics['meanRecallAt10']:.3f}, "
        f"nDCG@10 {metrics['meanNdcgAt10']:.3f}, "
        f"recall@25 {metrics['meanRecallAt25']:.3f}, "
        f"tokens-to-first-relevant {first_relevant_text}, "
        f"mean precision {metrics['meanPrecision']:.3f}, "
        f"mean latency {metrics['meanLatencyMs']:.1f} ms"
    )
    if args.write_baseline:
        write_baseline(summary, args.baseline)
        print(f"baseline written to {args.baseline}")
        return 0
    exit_code = 0
    if summary["failures"]:
        print("failed cases: " + ", ".join(summary["failures"]), file=sys.stderr)
        exit_code = 1
    for regression in check_regression(summary, args.baseline, args.tolerance):
        print(regression, file=sys.stderr)
        exit_code = 1
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
