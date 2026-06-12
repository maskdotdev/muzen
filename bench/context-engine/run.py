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
- `truthSource` may be set on a case file or case; otherwise it is inferred as
  `fixture`, `mined_followup`, or `curated`. This separates causal fixtures
  from future-follow-up stress labels in the reported cohorts.
- Redaction and prompt-injection violations always fail the run.

A committed baseline (`baseline.json`) gates ranking regressions: the run
fails when gated quality metrics drop more than `--tolerance` below it. Optional
ablation reports rerun the same public CLI with one context signal or optimizer
component disabled and write metric deltas without using hidden harness hooks.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import fnmatch
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
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
ABLATION_SCHEMA_VERSION = "muzen.context-eval-ablation.v1"
CONTEXT_SIGNAL_ABLATIONS = (
    "graph",
    "co-change",
    "path-proximity",
    "lexical-change",
    "test-coverage",
    "semantic-change",
    "pack-repair",
    "pack-path-diversity",
    "skeleton-reserve",
    "rank-diversity",
    "token-efficiency",
)
GATED_METRICS = (
    "meanRecallAt5",
    "meanRecallAt10",
    "meanNdcgAt10",
    "meanRecallAt25",
    "meanCandidateRecall",
    "sufficiencyInsufficientWhenIncomplete",
    "firstRelevantRate",
    "meanUsefulEvidencePer1kTokens",
)
GATED_MAX_METRICS = {"meanTokensToFirstRelevant": 128.0}
GATED_MAX_RATE_METRICS = {
    "candidatePresentMissRate": 0.005,
    "candidatePresentMissCaseRate": 0.01,
    "meanCandidatePresentMissRate": 0.005,
}
GATED_COHORTS = (
    ("byKind", "pack"),
    ("bySourceGroup", "external"),
    ("bySourceGroup", "self"),
    ("byTruthSource", "fixture"),
    ("byTruthSource", "mined_followup"),
    ("byTruthSource", "curated"),
)
TRUTH_SOURCES = {"fixture", "mined_followup", "curated"}
_DERIVED_CACHE_LOCKS_GUARD = threading.Lock()
_DERIVED_CACHE_LOCKS: dict[str, threading.Lock] = {}


@dataclass(frozen=True)
class CaseResult:
    id: str
    case_set: str
    source_kind: str
    source_group: str
    truth_source: str
    kind: str
    strict: bool
    recall: float
    precision: float
    recall_at_5: float
    recall_at_10: float
    recall_at_25: float
    ndcg_at_10: float
    candidate_recall: float
    first_relevant_rank: int | None
    tokens_to_first_relevant: int | None
    secret_redaction_correct: bool
    prompt_injection_resistant: bool
    useful_evidence_per_1k_tokens: float
    latency_ms: float
    expected_paths: list[str]
    candidate_expected_count: int
    retrieved_paths: list[str]
    candidate_missed_paths: list[str]
    candidate_present_missed_paths: list[str]
    candidate_present_missed_omissions: list[dict[str, Any]]
    selected_tail_candidates: list[dict[str, Any]]
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
    sufficiency_false_sufficient: bool


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
        help=(
            "Existing muzen binary. Defaults to one cargo build --bin muzen, "
            "then reuses target/debug/muzen for every case."
        ),
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
        "--jobs",
        type=positive_int,
        default=default_eval_jobs(),
        help=(
            "Run independent cases in parallel. Defaults to bounded CPU "
            "parallelism. Cases sharing one derived cache are serialized so "
            "cache writes stay deterministic."
        ),
    )
    parser.add_argument(
        "--case-id",
        action="append",
        default=[],
        help=(
            "Run one exact case id. Repeatable. Filtered runs are diagnostic: "
            "they may not write baselines and skip the regression gate."
        ),
    )
    parser.add_argument(
        "--case-glob",
        action="append",
        default=[],
        help=(
            "Run case ids matching a shell glob, for example '*-pack'. "
            "Repeatable. Filtered runs are diagnostic only."
        ),
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
    parser.add_argument(
        "--ablate-context-signal",
        action="append",
        choices=CONTEXT_SIGNAL_ABLATIONS,
        default=[],
        help=(
            "Pass through one public context signal or optimizer ablation to the muzen CLI. "
            "Repeatable; intended for single-variant debugging."
        ),
    )
    parser.add_argument(
        "--ablation-report",
        type=Path,
        help=(
            "Write an ablation report by rerunning variants with one context "
            "signal or optimizer component disabled. Variants are reported as "
            "deltas and are not regression-gated."
        ),
    )
    parser.add_argument(
        "--ablation-signal",
        action="append",
        choices=CONTEXT_SIGNAL_ABLATIONS,
        help=(
            "Signal or optimizer component to include in --ablation-report. "
            "Repeatable; defaults to all supported ablations."
        ),
    )
    return parser.parse_args()


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def default_eval_jobs() -> int:
    return max(1, min(os.cpu_count() or 1, 4))


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
    validate_host_context(case_file, path, "case file")
    validate_truth_source(case_file, path, "case file")
    for case in case_file["cases"]:
        if not case.get("expectedPaths"):
            raise SystemExit(f"{path}: case {case.get('id')!r} has no ground-truth expectedPaths")
        validate_host_context(case, path, f"case {case.get('id')!r}")
        validate_truth_source(case, path, f"case {case.get('id')!r}")


def validate_truth_source(doc: dict[str, Any], path: Path, label: str) -> None:
    truth_source = doc.get("truthSource")
    if truth_source is None:
        return
    if truth_source not in TRUTH_SOURCES:
        allowed = ", ".join(sorted(TRUTH_SOURCES))
        raise SystemExit(f"{path}: {label} truthSource must be one of {allowed}")


def validate_host_context(doc: dict[str, Any], path: Path, label: str) -> None:
    metadata = doc.get("hostMetadata")
    if metadata is not None and not isinstance(metadata, dict):
        raise SystemExit(f"{path}: {label} hostMetadata must be a JSON object")
    instructions = doc.get("hostInstructions")
    if instructions is None:
        return
    if not isinstance(instructions, list):
        raise SystemExit(f"{path}: {label} hostInstructions must be a JSON array")
    for index, instruction in enumerate(instructions):
        if not isinstance(instruction, dict):
            raise SystemExit(f"{path}: {label} hostInstructions[{index}] must be an object")
        if not isinstance(instruction.get("kind"), str) or not instruction["kind"].strip():
            raise SystemExit(f"{path}: {label} hostInstructions[{index}].kind must be a string")
        if not isinstance(instruction.get("text"), str) or not instruction["text"].strip():
            raise SystemExit(f"{path}: {label} hostInstructions[{index}].text must be a string")
        if not isinstance(instruction.get("trusted"), bool):
            raise SystemExit(f"{path}: {label} hostInstructions[{index}].trusted must be a boolean")


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


def prepare_corpus(case_files: list[dict[str, Any]]) -> None:
    """Serial setup avoids clone/diff races before parallel case execution."""
    seen: set[tuple[str, str | None, str | None, str | None]] = set()
    for case_file in case_files:
        source = case_file["repoSource"]
        key = (
            source["kind"],
            source.get("origin"),
            source.get("commit"),
            source.get("path"),
        )
        if key in seen:
            continue
        seen.add(key)
        materialize_repo(source)
        materialize_diff(source)


def case_selection_active(args: argparse.Namespace) -> bool:
    return bool(getattr(args, "case_id", []) or getattr(args, "case_glob", []))


def case_matches_selection(case: dict[str, Any], args: argparse.Namespace) -> bool:
    if not case_selection_active(args):
        return True
    case_id = str(case.get("id", ""))
    exact_ids = set(getattr(args, "case_id", []) or [])
    if case_id in exact_ids:
        return True
    return any(
        fnmatch.fnmatchcase(case_id, pattern)
        for pattern in (getattr(args, "case_glob", []) or [])
    )


def select_case_files(
    case_files: list[dict[str, Any]], args: argparse.Namespace
) -> tuple[list[dict[str, Any]], dict[str, Any] | None]:
    total_count = sum(len(case_file["cases"]) for case_file in case_files)
    if not case_selection_active(args):
        return case_files, None

    requested_ids = set(getattr(args, "case_id", []) or [])
    seen_requested_ids: set[str] = set()
    selected: list[dict[str, Any]] = []
    selected_count = 0
    for case_file in case_files:
        selected_cases = []
        for case in case_file["cases"]:
            case_id = str(case.get("id", ""))
            if case_id in requested_ids:
                seen_requested_ids.add(case_id)
            if case_matches_selection(case, args):
                selected_cases.append(case)
        if selected_cases:
            selected.append({**case_file, "cases": selected_cases})
            selected_count += len(selected_cases)

    missing = sorted(requested_ids - seen_requested_ids)
    if missing:
        raise SystemExit("case selection referenced unknown case id(s): " + ", ".join(missing))
    if selected_count == 0:
        raise SystemExit("case selection matched no cases")

    return selected, {
        "caseIds": list(getattr(args, "case_id", []) or []),
        "caseGlobs": list(getattr(args, "case_glob", []) or []),
        "selectedCaseCount": selected_count,
        "totalCaseCount": total_count,
        "diagnosticOnly": True,
    }


def validate_case_selection_mode(
    args: argparse.Namespace, case_selection: dict[str, Any] | None
) -> None:
    if case_selection and args.write_baseline:
        raise SystemExit("filtered diagnostic runs cannot write the regression baseline")


def default_muzen_binary() -> Path:
    suffix = ".exe" if sys.platform == "win32" else ""
    return ROOT / "target" / "debug" / f"muzen{suffix}"


def muzen_build_input_paths(root: Path = ROOT) -> list[Path]:
    paths = [root / "Cargo.toml", root / "Cargo.lock"]
    for source_root in [root / "src", root / "crates"]:
        if source_root.exists():
            paths.extend(source_root.rglob("*.rs"))
    return [path for path in paths if path.exists()]


def resolve_muzen_bin(muzen_bin: Path | None) -> Path:
    if muzen_bin:
        return muzen_bin
    subprocess.run(["cargo", "build", "--quiet", "--bin", "muzen"], cwd=ROOT, check=True)
    return default_muzen_binary()


def validate_muzen_binary_freshness(muzen_bin: Path) -> None:
    default_binary = default_muzen_binary().resolve()
    if muzen_bin.resolve() != default_binary:
        return
    if not muzen_bin.exists():
        raise SystemExit(f"muzen binary not found at {muzen_bin}")
    inputs = muzen_build_input_paths()
    if not inputs:
        return
    binary_mtime = muzen_bin.stat().st_mtime
    newest_input = max(inputs, key=lambda path: path.stat().st_mtime)
    newest_mtime = newest_input.stat().st_mtime
    if newest_mtime > binary_mtime + 0.001:
        try:
            newest_display = newest_input.relative_to(ROOT)
        except ValueError:
            newest_display = newest_input
        raise SystemExit(
            f"{muzen_bin} is older than {newest_display}; "
            "run `cargo build --bin muzen` or omit --muzen-bin so the eval "
            "runner builds the binary once."
        )


def eval_run_metadata(args: argparse.Namespace) -> dict[str, Any]:
    local_onnx_model_dir = getattr(args, "local_onnx_model_dir", None)
    forced_semantic_tier = "none"
    if local_onnx_model_dir:
        forced_semantic_tier = "local_onnx"
    elif getattr(args, "hosted_semantic", False):
        forced_semantic_tier = "hosted"
    metadata: dict[str, Any] = {
        "muzenBin": str(args.muzen_bin),
        "muzenBinMtimeUnixMs": int(args.muzen_bin.stat().st_mtime * 1000)
        if args.muzen_bin.exists()
        else None,
        "defaultBinaryFreshnessChecked": args.muzen_bin.resolve()
        == default_muzen_binary().resolve(),
        "semantic": {
            "forcedTier": forced_semantic_tier,
            "hostedEmbeddingModel": getattr(args, "hosted_embedding_model", None),
            "hostedEmbeddingBaseUrl": getattr(args, "hosted_embedding_base_url", None),
            "hostedMaxEmbeddingInputs": getattr(
                args, "hosted_max_embedding_inputs", None
            ),
            "localOnnxModelDir": str(local_onnx_model_dir)
            if local_onnx_model_dir
            else None,
        },
        "rerank": {
            "enabled": bool(getattr(args, "rerank_base_url", None)),
            "baseUrl": getattr(args, "rerank_base_url", None),
            "model": getattr(args, "rerank_model", None),
        },
        "ablateContextSignals": list(getattr(args, "ablate_context_signal", []) or []),
    }
    head = git_output(["rev-parse", "HEAD"])
    if head:
        metadata["gitHead"] = head
    dirty = git_output(["status", "--porcelain"])
    if dirty is not None:
        metadata["gitDirty"] = bool(dirty.strip())
    return metadata


def git_output(args: list[str]) -> str | None:
    completed = subprocess.run(
        ["git", "-C", str(ROOT), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    if completed.returncode != 0:
        return None
    return completed.stdout.strip()


def base_command(muzen_bin: Path) -> list[str]:
    return [str(muzen_bin)]


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
    cache_lock = derived_cache_lock(cache_root)
    host_metadata = merged_host_metadata(case_file, case)
    host_instructions = merged_host_instructions(case_file, case)
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
    command.extend(context_ablation_command_args(args))
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

    host_tmp = tempfile.TemporaryDirectory(prefix="muzen-context-host-")
    try:
        host_tmp_path = Path(host_tmp.name)
        if host_metadata:
            metadata_path = host_tmp_path / "host-metadata.json"
            metadata_path.write_text(json.dumps(host_metadata, sort_keys=True))
            command.extend(["--host-metadata-json", str(metadata_path)])
        if host_instructions:
            instructions_path = host_tmp_path / "host-instructions.json"
            instructions_path.write_text(json.dumps(host_instructions, sort_keys=True))
            command.extend(["--host-instruction-json", str(instructions_path)])

        with cache_lock:
            started = time.perf_counter()
            completed = subprocess.run(
                command,
                cwd=ROOT,
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
    finally:
        host_tmp.cleanup()
    latency_ms = (time.perf_counter() - started) * 1000
    if completed.returncode != 0:
        raise SystemExit(
            f"context case failed for {case['id']} with code {completed.returncode}\n"
            f"stderr:\n{completed.stderr}"
        )
    return json.loads(completed.stdout), latency_ms


def derived_cache_lock(cache_root: Path) -> threading.Lock:
    key = str(cache_root)
    with _DERIVED_CACHE_LOCKS_GUARD:
        lock = _DERIVED_CACHE_LOCKS.get(key)
        if lock is None:
            lock = threading.Lock()
            _DERIVED_CACHE_LOCKS[key] = lock
        return lock


def context_ablation_command_args(args: argparse.Namespace) -> list[str]:
    command_args: list[str] = []
    for signal in getattr(args, "ablate_context_signal", []):
        command_args.extend(["--ablate-context-signal", signal])
    return command_args


def merged_host_metadata(case_file: dict[str, Any], case: dict[str, Any]) -> dict[str, Any]:
    metadata: dict[str, Any] = {}
    metadata.update(case_file.get("hostMetadata") or {})
    metadata.update(case.get("hostMetadata") or {})
    return metadata


def merged_host_instructions(case_file: dict[str, Any], case: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        *(case_file.get("hostInstructions") or []),
        *(case.get("hostInstructions") or []),
    ]


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


def first_relevant_rank(retrieved: list[str], expected: set[str]) -> int | None:
    for index, path in enumerate(retrieved, start=1):
        if path in expected:
            return index
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
        first_relevant_path_rank = first_relevant_rank(eval_retrieved, eval_expected)
        first_relevant_tokens = tokens_to_first_relevant(eval_evidence, eval_expected)
        candidate_expected = eval_expected
    else:
        # Every expected path is itself a changed file (fixture invariant
        # cases): grade against the full sets.
        ranked_recall_at_5 = recall_at_k(retrieved_unique, expected_set, 5)
        ranked_recall_at_10 = recall_at_k(retrieved_unique, expected_set, 10)
        ranked_recall_at_25 = recall_at_k(retrieved_unique, expected_set, 25)
        ranked_ndcg_at_10 = ndcg_at_k(retrieved_unique, expected_set, 10)
        first_relevant_path_rank = first_relevant_rank(retrieved_unique, expected_set)
        first_relevant_tokens = tokens_to_first_relevant(evidence, expected_set)
        candidate_expected = expected_set
    omitted_paths = (
        [
            candidate["path"]
            for candidate in omitted
            if isinstance(candidate, dict) and candidate.get("path")
        ]
        if isinstance(omitted, list)
        else []
    )
    candidate_paths = set(eval_retrieved if eval_expected else retrieved_unique)
    candidate_paths.update(path for path in omitted_paths if path not in changed_set)
    if not eval_expected:
        candidate_paths.update(omitted_paths)
    candidate_missed_paths = sorted(candidate_expected - candidate_paths)
    candidate_recall = (
        len(candidate_expected - set(candidate_missed_paths)) / len(candidate_expected)
        if candidate_expected
        else 1.0
    )
    ranked_missed_paths = sorted(
        (eval_expected - set(eval_retrieved)) if eval_expected else (expected_set - retrieved_set)
    )
    candidate_present_missed_paths = sorted(
        set(ranked_missed_paths) - set(candidate_missed_paths)
    )
    candidate_present_missed_omissions = omitted_details_for_paths(
        omitted, candidate_present_missed_paths
    )
    selected_tail_candidates = selected_tail_details(result, evidence)
    sufficiency = result.get("sufficiency") or {}
    sufficiency_blocking_gaps = sum(
        1
        for gap in sufficiency.get("gaps", [])
        if any(kind != "no_related_tests" for kind in gap.get("missing", []))
    )
    false_sufficient = sufficiency.get("status") == "sufficient" and (
        bool(missed_paths) or bool(missing_expected_ranges)
    )
    source = case_file["repoSource"]
    return CaseResult(
        id=case["id"],
        case_set=case_file.get("name", case["id"]),
        source_kind=source["kind"],
        source_group=source_group(source),
        truth_source=truth_source(case_file, case),
        kind=case.get("kind", case.get("command", "query")),
        strict=bool(case.get("strict")),
        recall=hit_count / len(expected_set) if expected_set else 1.0,
        precision=hit_count / len(retrieved_set) if retrieved_set else 0.0,
        recall_at_5=ranked_recall_at_5,
        recall_at_10=ranked_recall_at_10,
        recall_at_25=ranked_recall_at_25,
        ndcg_at_10=ranked_ndcg_at_10,
        candidate_recall=candidate_recall,
        first_relevant_rank=first_relevant_path_rank,
        tokens_to_first_relevant=first_relevant_tokens,
        secret_redaction_correct=not forbidden_content_hits and not missing_required_content,
        prompt_injection_resistant=not trusted_forbidden_paths,
        useful_evidence_per_1k_tokens=useful_per_1k,
        latency_ms=latency_ms,
        expected_paths=expected_paths,
        candidate_expected_count=len(candidate_expected),
        retrieved_paths=retrieved_unique,
        candidate_missed_paths=candidate_missed_paths,
        candidate_present_missed_paths=candidate_present_missed_paths,
        candidate_present_missed_omissions=candidate_present_missed_omissions,
        selected_tail_candidates=selected_tail_candidates,
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
        sufficiency_false_sufficient=false_sufficient,
    )


def source_group(source: dict[str, Any]) -> str:
    if source["kind"] == "fixture":
        return "fixture"
    return "self" if source.get("origin") == "self" else "external"


def truth_source(case_file: dict[str, Any], case: dict[str, Any]) -> str:
    explicit = case.get("truthSource") or case_file.get("truthSource")
    if explicit:
        return explicit
    if case_file["repoSource"]["kind"] == "fixture":
        return "fixture"
    if case_file.get("minedFrom"):
        return "mined_followup"
    return "curated"


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


def omitted_details_for_paths(
    omitted: Any, paths: list[str]
) -> list[dict[str, Any]]:
    if not isinstance(omitted, list) or not paths:
        return []
    missed = set(paths)
    details = []
    for candidate in omitted:
        if not isinstance(candidate, dict) or candidate.get("path") not in missed:
            continue
        graph_paths = candidate.get("graphPaths")
        if not isinstance(graph_paths, list):
            graph_paths = []
        details.append(
            {
                "evidenceId": candidate.get("evidenceId"),
                "kind": candidate.get("kind"),
                "path": candidate.get("path"),
                "score": candidate.get("score"),
                "rankIndex": candidate.get("rankIndex"),
                "tokenEstimate": candidate.get("tokenEstimate"),
                "reason": candidate.get("reason"),
                "graphPaths": graph_paths,
            }
        )
    return details


def selected_tail_details(
    result: dict[str, Any], evidence: list[dict[str, Any]], limit: int = 8
) -> list[dict[str, Any]]:
    selected_candidates = result.get("selectedCandidates")
    if not isinstance(selected_candidates, list):
        return []
    relationships = result.get("relationships")
    graph_paths_by_id: dict[str, list[dict[str, Any]]] = {}
    if isinstance(relationships, list):
        for relationship in relationships:
            if not isinstance(relationship, dict):
                continue
            graph_path = {
                "kind": relationship.get("kind"),
                "confidence": relationship.get("confidence"),
                "path": relationship.get("reason"),
            }
            for endpoint in (relationship.get("from"), relationship.get("to")):
                if isinstance(endpoint, str):
                    graph_paths_by_id.setdefault(endpoint, []).append(graph_path)
    evidence_by_id = {
        entry.get("id"): entry
        for entry in evidence
        if isinstance(entry, dict) and entry.get("id")
    }
    details = []
    for candidate in selected_candidates[-limit:]:
        if not isinstance(candidate, dict):
            continue
        evidence_id = candidate.get("evidenceId")
        entry = evidence_by_id.get(evidence_id) or {}
        details.append(
            {
                "evidenceId": evidence_id,
                "kind": entry.get("kind"),
                "path": entry.get("path"),
                "score": candidate.get("score"),
                "rankIndex": candidate.get("rankIndex"),
                "tokenEstimate": entry.get("tokenEstimate"),
                "representation": entry.get("representation"),
                "graphPaths": graph_paths_by_id.get(evidence_id, []),
            }
        )
    return details


def metric_block(results: list[CaseResult]) -> dict[str, Any]:
    count = len(results)
    if count == 0:
        raise ValueError("cannot summarize an empty result set")
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
    insufficient_when_incomplete = (
        sum(1 for r in incomplete if r.sufficiency_status == "insufficient")
        / len(incomplete)
        if incomplete
        else None
    )
    candidate_expected_count = sum(result.candidate_expected_count for result in results)
    candidate_present_misses = sum(
        len(result.candidate_present_missed_paths) for result in results
    )
    per_case_candidate_present_miss_rates = [
        len(result.candidate_present_missed_paths) / result.candidate_expected_count
        if result.candidate_expected_count
        else 0.0
        for result in results
    ]
    return {
        "meanRecall": sum(result.recall for result in results) / count,
        "meanPrecision": sum(result.precision for result in results) / count,
        "meanRecallAt5": sum(result.recall_at_5 for result in results) / count,
        "meanRecallAt10": sum(result.recall_at_10 for result in results) / count,
        "meanRecallAt25": sum(result.recall_at_25 for result in results) / count,
        "meanNdcgAt10": sum(result.ndcg_at_10 for result in results) / count,
        "meanCandidateRecall": sum(result.candidate_recall for result in results) / count,
        "candidatePresentMissRate": (
            candidate_present_misses / candidate_expected_count
            if candidate_expected_count
            else 0.0
        ),
        "candidatePresentMissCaseRate": sum(
            1 for result in results if result.candidate_present_missed_paths
        )
        / count,
        "meanCandidatePresentMissRate": sum(per_case_candidate_present_miss_rates) / count,
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
        "sufficiencyInsufficientWhenIncomplete": insufficient_when_incomplete,
        "sufficiencySufficientWhenComplete": sufficient_when_complete,
        "sufficiencyFalseSufficientCount": sum(
            1 for result in results if result.sufficiency_false_sufficient
        ),
    }


def cohort_summary(results: list[CaseResult], attribute: str) -> dict[str, Any]:
    grouped: dict[str, list[CaseResult]] = {}
    for result in results:
        grouped.setdefault(str(getattr(result, attribute)), []).append(result)
    return {
        name: {"caseCount": len(group), "metrics": metric_block(group)}
        for name, group in sorted(grouped.items())
    }


def weak_cases(results: list[CaseResult], limit: int = 12) -> list[dict[str, Any]]:
    ranked = sorted(
        results,
        key=lambda result: (
            result.recall_at_10,
            result.ndcg_at_10,
            result.recall_at_25,
            result.id,
        ),
    )
    return [
        {
            "id": result.id,
            "caseSet": result.case_set,
            "sourceGroup": result.source_group,
            "truthSource": result.truth_source,
            "kind": result.kind,
            "recallAt10": result.recall_at_10,
            "recallAt25": result.recall_at_25,
            "ndcgAt10": result.ndcg_at_10,
            "candidateRecall": result.candidate_recall,
            "firstRelevantRank": result.first_relevant_rank,
            "tokensToFirstRelevant": result.tokens_to_first_relevant,
            "candidateMissedPaths": result.candidate_missed_paths,
            "candidatePresentMissedPaths": result.candidate_present_missed_paths,
            "candidatePresentMissedOmissions": result.candidate_present_missed_omissions,
            "selectedTailCandidates": result.selected_tail_candidates,
            "missedPaths": result.missed_paths,
            "sufficiencyStatus": result.sufficiency_status,
        }
        for result in ranked[:limit]
    ]


def slow_cases(results: list[CaseResult], limit: int = 10) -> list[dict[str, Any]]:
    ranked = sorted(results, key=lambda result: (-result.latency_ms, result.id))
    return [
        {
            "id": result.id,
            "caseSet": result.case_set,
            "sourceGroup": result.source_group,
            "truthSource": result.truth_source,
            "kind": result.kind,
            "latencyMs": result.latency_ms,
            "tokenEstimate": result.token_estimate,
            "omitted": result.omitted,
            "recallAt10": result.recall_at_10,
            "recallAt25": result.recall_at_25,
            "candidatePresentMissCount": len(result.candidate_present_missed_paths),
        }
        for result in ranked[:limit]
    ]


def mean_number(values: list[float | int]) -> float | None:
    return sum(values) / len(values) if values else None


def percentile_number(values: list[float | int], percentile: float) -> float | int | None:
    if not values:
        return None
    ordered = sorted(values)
    index = round((len(ordered) - 1) * percentile)
    return ordered[index]


def omission_pressure(results: list[CaseResult], limit: int = 12) -> dict[str, Any]:
    omissions: list[dict[str, Any]] = []
    cases_with_misses = [
        result for result in results if result.candidate_present_missed_paths
    ]
    score_beats_tail_cases = []
    for result in cases_with_misses:
        case_omissions = result.candidate_present_missed_omissions
        omissions.extend(case_omissions)
        missed_scores = [
            omission["score"]
            for omission in case_omissions
            if isinstance(omission.get("score"), int | float)
        ]
        tail_scores = [
            candidate["score"]
            for candidate in result.selected_tail_candidates
            if isinstance(candidate.get("score"), int | float)
        ]
        if not missed_scores or not tail_scores:
            continue
        best_missed_score = max(missed_scores)
        worst_tail_score = min(tail_scores)
        margin = best_missed_score - worst_tail_score
        if margin > 0:
            score_beats_tail_cases.append(
                {
                    "id": result.id,
                    "sourceGroup": result.source_group,
                    "truthSource": result.truth_source,
                    "bestMissedScore": best_missed_score,
                    "worstTailScore": worst_tail_score,
                    "margin": margin,
                    "candidatePresentMissCount": len(
                        result.candidate_present_missed_paths
                    ),
                    "firstRelevantRank": result.first_relevant_rank,
                    "tokensToFirstRelevant": result.tokens_to_first_relevant,
                }
            )

    reason_counts: dict[str, int] = {}
    graph_path_omissions = 0
    total_graph_paths = 0
    rank_values: list[int] = []
    score_values: list[float] = []
    token_values: list[int] = []
    for omission in omissions:
        reason = omission.get("reason")
        if isinstance(reason, str):
            reason_counts[reason] = reason_counts.get(reason, 0) + 1
        graph_paths = omission.get("graphPaths")
        if isinstance(graph_paths, list) and graph_paths:
            graph_path_omissions += 1
            total_graph_paths += len(graph_paths)
        rank_index = omission.get("rankIndex")
        if isinstance(rank_index, int):
            rank_values.append(rank_index)
        score = omission.get("score")
        if isinstance(score, int | float):
            score_values.append(float(score))
        token_estimate = omission.get("tokenEstimate")
        if isinstance(token_estimate, int):
            token_values.append(token_estimate)

    score_beats_tail_cases.sort(key=lambda case: (-case["margin"], case["id"]))
    return {
        "candidatePresentMissOmissions": {
            "count": len(omissions),
            "caseCount": len(cases_with_misses),
            "byReason": dict(sorted(reason_counts.items())),
            "withGraphPathCount": graph_path_omissions,
            "withoutGraphPathCount": len(omissions) - graph_path_omissions,
            "totalGraphPaths": total_graph_paths,
            "meanScore": mean_number(score_values),
            "medianRankIndex": percentile_number(rank_values, 0.5),
            "p90RankIndex": percentile_number(rank_values, 0.9),
            "meanTokenEstimate": mean_number(token_values),
            "scoreBeatsSelectedTailCaseCount": len(score_beats_tail_cases),
            "scoreBeatsSelectedTailCases": score_beats_tail_cases[:limit],
        }
    }


def summarize(results: list[CaseResult]) -> dict[str, Any]:
    count = len(results)
    failures = sorted(
        {
            result.id
            for result in results
            if not result.secret_redaction_correct
            or not result.prompt_injection_resistant
            or result.sufficiency_false_sufficient
            or (result.strict and (result.missed_paths or result.missing_expected_ranges))
        }
    )
    return {
        "schemaVersion": SUMMARY_SCHEMA_VERSION,
        "generatedAtUnixMs": int(time.time() * 1000),
        "caseCount": count,
        "ok": not failures,
        "failures": failures,
        "metrics": metric_block(results),
        "cohorts": {
            "byKind": cohort_summary(results, "kind"),
            "bySourceGroup": cohort_summary(results, "source_group"),
            "byTruthSource": cohort_summary(results, "truth_source"),
            "byStrict": cohort_summary(results, "strict"),
        },
        "weakCases": weak_cases(results),
        "slowCases": slow_cases(results),
        "diagnostics": {
            "omissionPressure": omission_pressure(results),
        },
        "cases": [result.__dict__ for result in results],
    }


def check_regression(
    summary: dict[str, Any], baseline_path: Path, tolerance: float
) -> list[str]:
    if not baseline_path.exists():
        return []
    baseline_doc = json.loads(baseline_path.read_text())
    baseline = baseline_doc["metrics"]
    regressions = []
    regressions.extend(
        metric_regressions("overall", summary["metrics"], baseline, tolerance)
    )
    regressions.extend(max_metric_regressions("overall", summary["metrics"], baseline))
    regressions.extend(max_rate_metric_regressions("overall", summary["metrics"], baseline))
    baseline_cohorts = baseline_doc.get("cohorts") or {}
    for cohort_group, cohort_name in GATED_COHORTS:
        current_metrics = (
            summary.get("cohorts", {})
            .get(cohort_group, {})
            .get(cohort_name, {})
            .get("metrics")
        )
        baseline_metrics = (
            baseline_cohorts.get(cohort_group, {})
            .get(cohort_name, {})
            .get("metrics")
        )
        if current_metrics is None or baseline_metrics is None:
            continue
        label = f"{cohort_group}.{cohort_name}"
        regressions.extend(
            metric_regressions(label, current_metrics, baseline_metrics, tolerance)
        )
        regressions.extend(max_metric_regressions(label, current_metrics, baseline_metrics))
        regressions.extend(max_rate_metric_regressions(label, current_metrics, baseline_metrics))
    return regressions


def metric_regressions(
    label: str, current_metrics: dict[str, Any], baseline_metrics: dict[str, Any], tolerance: float
) -> list[str]:
    regressions = []
    for metric in GATED_METRICS:
        if metric not in baseline_metrics:
            continue
        current = current_metrics[metric]
        if current is None or baseline_metrics[metric] is None:
            continue
        floor = baseline_metrics[metric] - tolerance
        if current < floor:
            regressions.append(
                f"{label}.{metric} regressed: {current:.4f} < "
                f"baseline {baseline_metrics[metric]:.4f} - tolerance {tolerance}"
            )
    return regressions


def max_metric_regressions(
    label: str, current_metrics: dict[str, Any], baseline_metrics: dict[str, Any]
) -> list[str]:
    regressions = []
    for metric, tolerance in GATED_MAX_METRICS.items():
        if metric not in baseline_metrics:
            continue
        current = current_metrics[metric]
        baseline = baseline_metrics[metric]
        if current is None or baseline is None:
            continue
        ceiling = baseline + tolerance
        if current > ceiling:
            regressions.append(
                f"{label}.{metric} regressed: {current:.1f} > "
                f"baseline {baseline:.1f} + tolerance {tolerance:.1f}"
            )
    return regressions


def max_rate_metric_regressions(
    label: str, current_metrics: dict[str, Any], baseline_metrics: dict[str, Any]
) -> list[str]:
    regressions = []
    for metric, tolerance in GATED_MAX_RATE_METRICS.items():
        if metric not in baseline_metrics:
            continue
        current = current_metrics[metric]
        baseline = baseline_metrics[metric]
        if current is None or baseline is None:
            continue
        ceiling = baseline + tolerance
        if current > ceiling:
            regressions.append(
                f"{label}.{metric} regressed: {current:.4f} > "
                f"baseline {baseline:.4f} + tolerance {tolerance:.4f}"
            )
    return regressions


def baseline_metric_subset(metrics: dict[str, Any]) -> dict[str, Any]:
    return {
        metric: metrics[metric]
        for metric in (
            "meanRecall",
            "meanPrecision",
            "meanRecallAt5",
            "meanRecallAt10",
            "meanRecallAt25",
            "meanNdcgAt10",
            "meanCandidateRecall",
            "candidatePresentMissRate",
            "candidatePresentMissCaseRate",
            "meanCandidatePresentMissRate",
            "sufficiencyInsufficientWhenIncomplete",
            "firstRelevantRate",
            "meanTokensToFirstRelevant",
            "meanUsefulEvidencePer1kTokens",
        )
    }


def write_baseline(summary: dict[str, Any], baseline_path: Path) -> None:
    baseline = {
        "schemaVersion": "muzen.context-eval-baseline.v1",
        "caseCount": summary["caseCount"],
        "metrics": baseline_metric_subset(summary["metrics"]),
        "cohorts": {
            cohort_group: {
                cohort_name: {
                    "caseCount": cohort["caseCount"],
                    "metrics": baseline_metric_subset(cohort["metrics"]),
                }
                for cohort_name, cohort in cohorts.items()
            }
            for cohort_group, cohorts in summary["cohorts"].items()
        },
    }
    baseline_path.write_text(json.dumps(baseline, indent=2) + "\n")


def run_suite(case_files: list[dict[str, Any]], args: argparse.Namespace) -> dict[str, Any]:
    tasks = [
        (case_file, case)
        for case_file in case_files
        for case in case_file["cases"]
    ]
    if getattr(args, "jobs", 1) <= 1:
        results = [score_case(case_file, case, args) for case_file, case in tasks]
        return summarize(results)

    prepare_corpus(case_files)
    results: list[CaseResult | None] = [None] * len(tasks)
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = {
            pool.submit(score_case, case_file, case, args): index
            for index, (case_file, case) in enumerate(tasks)
        }
        for future in concurrent.futures.as_completed(futures):
            index = futures[future]
            try:
                results[index] = future.result()
            except BaseException:
                for pending in futures:
                    pending.cancel()
                raise
    completed_results = [result for result in results if result is not None]
    if len(completed_results) != len(tasks):
        raise SystemExit("parallel context eval finished without every case result")
    return summarize(completed_results)


def print_summary(summary: dict[str, Any], *, label: str = "context-engine eval") -> None:
    metrics = summary["metrics"]
    first_relevant = metrics["meanTokensToFirstRelevant"]
    first_relevant_text = f"{first_relevant:.0f}" if first_relevant is not None else "n/a"
    print(
        f"{label}: "
        f"{summary['caseCount']} cases, "
        f"recall@10 {metrics['meanRecallAt10']:.3f}, "
        f"nDCG@10 {metrics['meanNdcgAt10']:.3f}, "
        f"recall@25 {metrics['meanRecallAt25']:.3f}, "
        f"candidate-present-miss {metrics['candidatePresentMissRate']:.3f}, "
        f"tokens-to-first-relevant {first_relevant_text}, "
        f"mean precision {metrics['meanPrecision']:.3f}, "
        f"mean latency {metrics['meanLatencyMs']:.1f} ms"
    )


def numeric_delta(current: Any, baseline: Any) -> float | None:
    if (
        isinstance(current, (int, float))
        and not isinstance(current, bool)
        and isinstance(baseline, (int, float))
        and not isinstance(baseline, bool)
    ):
        return current - baseline
    return None


def metric_deltas(current: dict[str, Any], baseline: dict[str, Any]) -> dict[str, Any]:
    deltas: dict[str, Any] = {}
    for metric, current_value in current.items():
        if metric not in baseline:
            continue
        deltas[metric] = numeric_delta(current_value, baseline[metric])
    return deltas


def ablation_cohort_deltas(
    baseline_summary: dict[str, Any], ablated_summary: dict[str, Any]
) -> dict[str, Any]:
    cohorts: dict[str, Any] = {}
    for cohort_group, cohort_name in GATED_COHORTS:
        baseline = (
            baseline_summary.get("cohorts", {})
            .get(cohort_group, {})
            .get(cohort_name)
        )
        current = (
            ablated_summary.get("cohorts", {})
            .get(cohort_group, {})
            .get(cohort_name)
        )
        if not baseline or not current:
            continue
        cohorts.setdefault(cohort_group, {})[cohort_name] = {
            "caseCount": current["caseCount"],
            "metrics": baseline_metric_subset(current["metrics"]),
            "deltaVsBaseline": metric_deltas(
                baseline_metric_subset(current["metrics"]),
                baseline_metric_subset(baseline["metrics"]),
            ),
        }
    return cohorts


def ablation_variant_args(args: argparse.Namespace, signal: str) -> argparse.Namespace:
    variant = argparse.Namespace(**vars(args))
    existing = list(getattr(args, "ablate_context_signal", []))
    variant.ablate_context_signal = [*existing, signal]
    return variant


def ablation_entry(
    signal: str, baseline_summary: dict[str, Any], ablated_summary: dict[str, Any]
) -> dict[str, Any]:
    current_metrics = baseline_metric_subset(ablated_summary["metrics"])
    baseline_metrics = baseline_metric_subset(baseline_summary["metrics"])
    return {
        "disabledSignals": [signal],
        "caseCount": ablated_summary["caseCount"],
        "metrics": current_metrics,
        "deltaVsBaseline": metric_deltas(current_metrics, baseline_metrics),
        "cohorts": ablation_cohort_deltas(baseline_summary, ablated_summary),
        "weakCases": ablated_summary["weakCases"],
    }


def run_ablation_report(
    case_files: list[dict[str, Any]],
    args: argparse.Namespace,
    baseline_summary: dict[str, Any],
) -> dict[str, Any]:
    signals = args.ablation_signal or list(CONTEXT_SIGNAL_ABLATIONS)
    variants = []
    for signal in signals:
        variant_args = ablation_variant_args(args, signal)
        summary = run_suite(case_files, variant_args)
        print_summary(summary, label=f"context-engine ablation {signal}")
        variants.append(ablation_entry(signal, baseline_summary, summary))
    return {
        "schemaVersion": ABLATION_SCHEMA_VERSION,
        "generatedAtUnixMs": int(time.time() * 1000),
        "baseline": {
            "caseCount": baseline_summary["caseCount"],
            "metrics": baseline_metric_subset(baseline_summary["metrics"]),
        },
        "variants": variants,
    }


def main() -> int:
    args = parse_args()
    args.muzen_bin = resolve_muzen_bin(args.muzen_bin)
    validate_muzen_binary_freshness(args.muzen_bin)
    case_files = load_case_files(args.cases_dir)
    case_files, case_selection = select_case_files(case_files, args)
    validate_case_selection_mode(args, case_selection)

    summary = run_suite(case_files, args)
    summary["runMetadata"] = eval_run_metadata(args)
    if case_selection:
        summary["caseSelection"] = case_selection
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(summary, indent=2) + "\n")

    print_summary(summary)
    if args.write_baseline:
        write_baseline(summary, args.baseline)
        print(f"baseline written to {args.baseline}")
        return 0
    exit_code = 0
    if summary["failures"]:
        print("failed cases: " + ", ".join(summary["failures"]), file=sys.stderr)
        exit_code = 1
    if case_selection:
        print("filtered diagnostic run: regression gate skipped", file=sys.stderr)
    else:
        for regression in check_regression(summary, args.baseline, args.tolerance):
            print(regression, file=sys.stderr)
            exit_code = 1
    if args.ablation_report:
        report = run_ablation_report(case_files, args, summary)
        if case_selection:
            report["caseSelection"] = case_selection
        args.ablation_report.parent.mkdir(parents=True, exist_ok=True)
        args.ablation_report.write_text(json.dumps(report, indent=2) + "\n")
        print(f"ablation report written to {args.ablation_report}")
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
