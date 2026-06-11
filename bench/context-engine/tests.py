#!/usr/bin/env python3
"""Tests for the context-engine eval harness and corpus miner.

Run: python3 bench/context-engine/tests.py
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
MINER = ROOT / "scripts" / "mine_context_cases.py"


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


run = load_module("context_eval_run", HERE / "run.py")


class RankingMetricsTest(unittest.TestCase):
    def test_recall_at_k_counts_only_top_k(self):
        retrieved = ["a", "b", "c", "d", "e", "f"]
        expected = {"a", "f"}
        self.assertEqual(run.recall_at_k(retrieved, expected, 5), 0.5)
        self.assertEqual(run.recall_at_k(retrieved, expected, 10), 1.0)

    def test_ndcg_perfect_ranking_is_one(self):
        self.assertAlmostEqual(run.ndcg_at_k(["a", "b", "x"], {"a", "b"}, 10), 1.0)

    def test_ndcg_rewards_earlier_hits(self):
        early = run.ndcg_at_k(["a", "x", "y"], {"a"}, 10)
        late = run.ndcg_at_k(["x", "y", "a"], {"a"}, 10)
        self.assertGreater(early, late)
        self.assertEqual(run.ndcg_at_k(["x", "y"], {"a"}, 10), 0.0)

    def test_tokens_to_first_relevant_accumulates_through_hit(self):
        evidence = [
            {"path": "x", "tokenEstimate": 100},
            {"path": "a", "tokenEstimate": 50},
            {"path": "y", "tokenEstimate": 9000},
        ]
        self.assertEqual(run.tokens_to_first_relevant(evidence, {"a"}), 150)
        self.assertIsNone(run.tokens_to_first_relevant(evidence, {"missing"}))


class CaseValidationTest(unittest.TestCase):
    def valid_case_file(self) -> dict:
        return {
            "schemaVersion": run.CASE_SCHEMA_VERSION,
            "name": "t",
            "repoSource": {"kind": "git", "commit": "0" * 40, "origin": "self"},
            "changedFiles": ["src/lib.rs"],
            "cases": [{"id": "t-1", "command": "pack", "expectedPaths": ["src/a.rs"]}],
        }

    def test_valid_case_file_passes(self):
        run.validate_case_file(self.valid_case_file(), Path("t.json"))

    def test_case_without_ground_truth_is_rejected(self):
        case_file = self.valid_case_file()
        case_file["cases"][0]["expectedPaths"] = []
        with self.assertRaises(SystemExit):
            run.validate_case_file(case_file, Path("t.json"))

    def test_wrong_schema_version_is_rejected(self):
        case_file = self.valid_case_file()
        case_file["schemaVersion"] = "muzen.context-eval-case.v1"
        with self.assertRaises(SystemExit):
            run.validate_case_file(case_file, Path("t.json"))

    def test_git_source_requires_pinned_commit(self):
        case_file = self.valid_case_file()
        case_file["repoSource"] = {"kind": "git", "origin": "self"}
        with self.assertRaises(SystemExit):
            run.validate_case_file(case_file, Path("t.json"))

    def test_git_source_requires_origin(self):
        case_file = self.valid_case_file()
        case_file["repoSource"] = {"kind": "git", "commit": "0" * 40}
        with self.assertRaises(SystemExit):
            run.validate_case_file(case_file, Path("t.json"))

    def test_self_origin_resolves_to_this_repository(self):
        source = {"kind": "git", "commit": "0" * 40, "origin": "self"}
        self.assertEqual(run.resolve_origin(source), str(run.ROOT))

    def test_missing_external_origin_path_is_rejected(self):
        source = {"kind": "git", "commit": "0" * 40, "origin": "/nonexistent/corpus-repo"}
        with self.assertRaises(SystemExit):
            run.resolve_origin(source)


class RegressionGateTest(unittest.TestCase):
    def summary(self, recall_at_10: float, ndcg_at_10: float) -> dict:
        return {"metrics": {"meanRecallAt10": recall_at_10, "meanNdcgAt10": ndcg_at_10}}

    def test_drop_beyond_tolerance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps({"metrics": {"meanRecallAt10": 0.5, "meanNdcgAt10": 0.5}})
            )
            regressions = run.check_regression(self.summary(0.4, 0.5), baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("meanRecallAt10", regressions[0])

    def test_drop_within_tolerance_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps({"metrics": {"meanRecallAt10": 0.5, "meanNdcgAt10": 0.5}})
            )
            self.assertEqual(run.check_regression(self.summary(0.49, 0.5), baseline, 0.02), [])

    def test_missing_baseline_is_not_a_failure(self):
        regressions = run.check_regression(
            self.summary(0.0, 0.0), Path("/nonexistent/baseline.json"), 0.02
        )
        self.assertEqual(regressions, [])


class MinerDeterminismTest(unittest.TestCase):
    def test_miner_is_deterministic_for_pinned_rev(self):
        pinned = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "HEAD"],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout.strip()
        with tempfile.TemporaryDirectory() as tmp:
            outputs = []
            for run_dir in ("a", "b"):
                out_dir = Path(tmp) / run_dir
                subprocess.run(
                    [
                        sys.executable,
                        str(MINER),
                        "--rev",
                        pinned,
                        "--output-dir",
                        str(out_dir),
                        "--max-cases",
                        "5",
                    ],
                    check=True,
                    cwd=ROOT,
                    stdout=subprocess.DEVNULL,
                )
                outputs.append(
                    {path.name: path.read_text() for path in sorted(out_dir.glob("*.json"))}
                )
            self.assertEqual(outputs[0], outputs[1])
            self.assertEqual(len(outputs[0]), 5)


if __name__ == "__main__":
    unittest.main()
