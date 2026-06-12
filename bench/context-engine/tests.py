#!/usr/bin/env python3
"""Tests for the context-engine eval harness and corpus miner.

Run: python3 bench/context-engine/tests.py
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import subprocess
import sys
import tempfile
import time
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
compare = load_module("context_eval_compare", HERE / "compare.py")


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

    def test_first_relevant_rank_is_one_based(self):
        self.assertEqual(run.first_relevant_rank(["x", "a", "b"], {"a"}), 2)
        self.assertIsNone(run.first_relevant_rank(["x"], {"a"}))


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

    def test_host_metadata_must_be_object(self):
        case_file = self.valid_case_file()
        case_file["hostMetadata"] = ["not", "object"]
        with self.assertRaises(SystemExit):
            run.validate_case_file(case_file, Path("t.json"))

    def test_host_instruction_schema_is_validated(self):
        case_file = self.valid_case_file()
        case_file["cases"][0]["hostInstructions"] = [
            {"kind": "ticket", "text": "requires api parity", "trusted": True}
        ]
        run.validate_case_file(case_file, Path("t.json"))
        case_file["cases"][0]["hostInstructions"][0]["trusted"] = "yes"
        with self.assertRaises(SystemExit):
            run.validate_case_file(case_file, Path("t.json"))

    def test_host_context_merges_case_file_and_case_values(self):
        case_file = {
            "hostMetadata": {"ticket": "parent", "ci": "green"},
            "hostInstructions": [{"kind": "ticket", "text": "parent", "trusted": True}],
        }
        case = {
            "hostMetadata": {"ticket": "child"},
            "hostInstructions": [{"kind": "ci", "text": "child", "trusted": True}],
        }
        self.assertEqual(
            run.merged_host_metadata(case_file, case),
            {"ticket": "child", "ci": "green"},
        )
        self.assertEqual(len(run.merged_host_instructions(case_file, case)), 2)

    def test_truth_source_is_validated_and_inferred(self):
        case_file = self.valid_case_file()
        case_file["truthSource"] = "curated"
        run.validate_case_file(case_file, Path("t.json"))
        self.assertEqual(run.truth_source(case_file, case_file["cases"][0]), "curated")

        case_file["cases"][0]["truthSource"] = "mined_followup"
        self.assertEqual(
            run.truth_source(case_file, case_file["cases"][0]), "mined_followup"
        )

        case_file["cases"][0]["truthSource"] = "made_up"
        with self.assertRaises(SystemExit):
            run.validate_case_file(case_file, Path("t.json"))

    def test_truth_source_infers_fixture_and_mined_followup(self):
        fixture = {
            "repoSource": {"kind": "fixture"},
        }
        self.assertEqual(run.truth_source(fixture, {}), "fixture")
        mined = {
            "repoSource": {"kind": "git", "origin": "self"},
            "minedFrom": {"baseCommit": "a", "followUpCommit": "b"},
        }
        self.assertEqual(run.truth_source(mined, {}), "mined_followup")


class RegressionGateTest(unittest.TestCase):
    def summary(self, recall_at_10: float, ndcg_at_10: float) -> dict:
        return {
            "metrics": {
                "meanRecallAt5": recall_at_10,
                "meanRecallAt10": recall_at_10,
                "meanNdcgAt10": ndcg_at_10,
                "meanRecallAt25": recall_at_10,
                "meanCandidateRecall": 1.0,
                "candidatePresentMissRate": 0.0,
                "candidatePresentMissCaseRate": 0.0,
                "meanCandidatePresentMissRate": 0.0,
                "sufficiencyInsufficientWhenIncomplete": 1.0,
                "firstRelevantRate": 1.0,
                "meanTokensToFirstRelevant": 1000.0,
                "meanUsefulEvidencePer1kTokens": 1.0,
            },
            "cohorts": {},
        }

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

    def test_external_cohort_drop_beyond_tolerance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {
                        "metrics": {
                            "meanRecallAt10": 0.5,
                            "meanNdcgAt10": 0.5,
                            "meanRecallAt25": 0.5,
                            "meanCandidateRecall": 1.0,
                        },
                        "cohorts": {
                            "bySourceGroup": {
                                "external": {
                                    "metrics": {
                                        "meanRecallAt10": 0.5,
                                        "meanNdcgAt10": 0.5,
                                        "meanRecallAt25": 0.5,
                                        "meanCandidateRecall": 1.0,
                                    }
                                }
                            }
                        },
                    }
                )
            )
            summary = self.summary(0.5, 0.5)
            summary["cohorts"] = {
                "bySourceGroup": {
                    "external": {
                        "metrics": {
                            "meanRecallAt10": 0.4,
                            "meanNdcgAt10": 0.5,
                            "meanRecallAt25": 0.5,
                            "meanCandidateRecall": 1.0,
                        }
                    }
                }
            }
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("bySourceGroup.external.meanRecallAt10", regressions[0])

    def test_curated_truth_source_drop_beyond_tolerance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {
                        "metrics": {},
                        "cohorts": {
                            "byTruthSource": {
                                "curated": {
                                    "metrics": {
                                        "meanRecallAt10": 1.0,
                                    }
                                }
                            }
                        },
                    }
                )
            )
            summary = self.summary(1.0, 1.0)
            summary["cohorts"] = {
                "byTruthSource": {
                    "curated": {
                        "metrics": {
                            "meanRecallAt10": 0.5,
                        }
                    }
                }
            }
            regressions = run.check_regression(summary, baseline, 0.02)

            self.assertEqual(len(regressions), 1)
            self.assertIn("byTruthSource.curated.meanRecallAt10", regressions[0])

    def test_write_baseline_includes_cohort_metrics(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            summary = run.summarize(
                [
                    case_result("self", source_group="self", truth_source="fixture"),
                    case_result("curated", source_group="fixture", truth_source="curated"),
                    case_result(
                        "external",
                        source_group="external",
                        truth_source="mined_followup",
                    ),
                ]
            )
            run.write_baseline(summary, baseline)
            written = json.loads(baseline.read_text())
            self.assertIn("cohorts", written)
            self.assertIn("external", written["cohorts"]["bySourceGroup"])
            self.assertIn("curated", written["cohorts"]["byTruthSource"])
            self.assertIn("fixture", written["cohorts"]["byTruthSource"])
            self.assertIn("mined_followup", written["cohorts"]["byTruthSource"])
            self.assertIn(
                "meanRecallAt25",
                written["cohorts"]["bySourceGroup"]["external"]["metrics"],
            )
            self.assertIn(
                "meanCandidateRecall",
                written["cohorts"]["bySourceGroup"]["external"]["metrics"],
            )
            self.assertIn(
                "candidatePresentMissRate",
                written["cohorts"]["bySourceGroup"]["external"]["metrics"],
            )
            self.assertIn(
                "sufficiencyInsufficientWhenIncomplete",
                written["cohorts"]["bySourceGroup"]["external"]["metrics"],
            )

    def test_candidate_recall_drop_beyond_tolerance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {
                        "metrics": {
                            "meanRecallAt10": 0.5,
                            "meanNdcgAt10": 0.5,
                            "meanRecallAt25": 0.5,
                            "meanCandidateRecall": 0.9,
                        }
                    }
                )
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["meanCandidateRecall"] = 0.7
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("meanCandidateRecall", regressions[0])

    def test_first_relevant_rate_drop_beyond_tolerance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {
                        "metrics": {
                            "firstRelevantRate": 0.9,
                        }
                    }
                )
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["firstRelevantRate"] = 0.7
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("firstRelevantRate", regressions[0])

    def test_tokens_to_first_relevant_regression_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps({"metrics": {"meanTokensToFirstRelevant": 1000.0}})
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["meanTokensToFirstRelevant"] = 1200.0
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("meanTokensToFirstRelevant", regressions[0])

    def test_candidate_present_miss_rate_regression_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps({"metrics": {"candidatePresentMissRate": 0.10}})
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["candidatePresentMissRate"] = 0.106
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("candidatePresentMissRate", regressions[0])

    def test_candidate_present_miss_case_rate_regression_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps({"metrics": {"candidatePresentMissCaseRate": 0.10}})
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["candidatePresentMissCaseRate"] = 0.111
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("candidatePresentMissCaseRate", regressions[0])

    def test_mean_candidate_present_miss_rate_regression_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps({"metrics": {"meanCandidatePresentMissRate": 0.10}})
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["meanCandidatePresentMissRate"] = 0.106
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("meanCandidatePresentMissRate", regressions[0])

    def test_useful_evidence_drop_beyond_tolerance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps({"metrics": {"meanUsefulEvidencePer1kTokens": 1.0}})
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["meanUsefulEvidencePer1kTokens"] = 0.9
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("meanUsefulEvidencePer1kTokens", regressions[0])

    def test_insufficient_when_incomplete_drop_beyond_tolerance_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {"metrics": {"sufficiencyInsufficientWhenIncomplete": 1.0}}
                )
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["sufficiencyInsufficientWhenIncomplete"] = 0.5
            regressions = run.check_regression(summary, baseline, 0.02)
            self.assertEqual(len(regressions), 1)
            self.assertIn("sufficiencyInsufficientWhenIncomplete", regressions[0])

    def test_undefined_gated_metric_is_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {
                        "metrics": {
                            "sufficiencyInsufficientWhenIncomplete": None,
                        }
                    }
                )
            )
            summary = self.summary(0.5, 0.5)
            summary["metrics"]["sufficiencyInsufficientWhenIncomplete"] = None

            self.assertEqual(run.check_regression(summary, baseline, 0.02), [])


def case_result(
    case_id: str,
    *,
    source_group: str = "self",
    truth_source: str = "curated",
    strict: bool = False,
    missed_paths: list[str] | None = None,
    sufficiency_status: str | None = None,
    false_sufficient: bool = False,
) -> run.CaseResult:
    return run.CaseResult(
        id=case_id,
        case_set=case_id,
        source_kind="git",
        source_group=source_group,
        truth_source=truth_source,
        kind="pack",
        strict=strict,
        recall=1.0,
        precision=1.0,
        recall_at_5=1.0,
        recall_at_10=1.0,
        recall_at_25=1.0,
        ndcg_at_10=1.0,
        candidate_recall=1.0,
        first_relevant_rank=1,
        tokens_to_first_relevant=10,
        secret_redaction_correct=True,
        prompt_injection_resistant=True,
        useful_evidence_per_1k_tokens=1.0,
        latency_ms=1.0,
        expected_paths=["src/a.rs"],
        candidate_expected_count=1,
        retrieved_paths=["src/a.rs"],
        candidate_missed_paths=[],
        candidate_present_missed_paths=[],
        candidate_present_missed_omissions=[],
        selected_tail_candidates=[],
        missed_paths=missed_paths or [],
        unexpected_paths=[],
        forbidden_content_hits=[],
        missing_required_content=[],
        trusted_forbidden_paths=[],
        missing_expected_ranges=[],
        token_estimate=10,
        omitted=0,
        sufficiency_status=sufficiency_status,
        sufficiency_blocking_gaps=0,
        sufficiency_false_sufficient=false_sufficient,
    )


class SummaryProofTest(unittest.TestCase):
    def test_summary_reports_source_cohorts_and_weak_cases(self):
        good = case_result("good", source_group="self")
        weak = case_result(
            "weak",
            source_group="external",
            missed_paths=["src/a.rs"],
            sufficiency_status="insufficient",
        )
        weak = run.CaseResult(
            **{
                **weak.__dict__,
                "recall_at_10": 0.0,
                "recall_at_25": 0.0,
                "ndcg_at_10": 0.0,
                "tokens_to_first_relevant": None,
                "candidate_present_missed_paths": ["src/a.rs"],
                "candidate_present_missed_omissions": [
                    {
                        "evidenceId": "ev-1",
                        "kind": "file_span",
                        "path": "src/a.rs",
                        "score": 0.5,
                        "rankIndex": 12,
                        "tokenEstimate": 100,
                        "reason": "budget_exhausted",
                    }
                ],
                "selected_tail_candidates": [
                    {
                        "evidenceId": "tail-1",
                        "kind": "file_span",
                        "path": "src/tail.rs",
                        "score": 0.42,
                        "rankIndex": 20,
                        "tokenEstimate": 300,
                        "representation": "full_content",
                    }
                ],
            }
        )
        summary = run.summarize([good, weak])
        self.assertEqual(summary["cohorts"]["bySourceGroup"]["self"]["caseCount"], 1)
        self.assertEqual(summary["cohorts"]["bySourceGroup"]["external"]["caseCount"], 1)
        self.assertEqual(summary["cohorts"]["byTruthSource"]["curated"]["caseCount"], 2)
        self.assertEqual(summary["metrics"]["candidatePresentMissRate"], 0.5)
        self.assertEqual(summary["metrics"]["candidatePresentMissCaseRate"], 0.5)
        self.assertEqual(summary["metrics"]["meanCandidatePresentMissRate"], 0.5)
        self.assertEqual(
            summary["metrics"]["sufficiencyInsufficientWhenIncomplete"], 1.0
        )
        self.assertEqual(summary["weakCases"][0]["id"], "weak")
        self.assertEqual(summary["weakCases"][0]["truthSource"], "curated")
        self.assertIn("candidateRecall", summary["weakCases"][0])
        self.assertIn("firstRelevantRank", summary["weakCases"][0])
        self.assertIn("tokensToFirstRelevant", summary["weakCases"][0])
        self.assertIn("candidatePresentMissedPaths", summary["weakCases"][0])
        self.assertEqual(
            summary["weakCases"][0]["candidatePresentMissedOmissions"][0]["reason"],
            "budget_exhausted",
        )
        self.assertEqual(
            summary["weakCases"][0]["selectedTailCandidates"][0]["evidenceId"],
            "tail-1",
        )

    def test_selected_tail_details_join_scores_to_evidence(self):
        result = {
            "selectedCandidates": [
                {"evidenceId": "a", "score": 0.9, "rankIndex": 0},
                {"evidenceId": "b", "score": 0.4, "rankIndex": 20},
            ]
        }
        evidence = [
            {
                "id": "a",
                "kind": "file_span",
                "path": "src/a.rs",
                "tokenEstimate": 100,
                "representation": "full_content",
            },
            {
                "id": "b",
                "kind": "test",
                "path": "tests/b.rs",
                "tokenEstimate": 50,
                "representation": "skeleton",
            },
        ]

        tail = run.selected_tail_details(result, evidence, limit=1)

        self.assertEqual(
            tail,
            [
                {
                    "evidenceId": "b",
                    "kind": "test",
                    "path": "tests/b.rs",
                    "score": 0.4,
                    "rankIndex": 20,
                    "tokenEstimate": 50,
                    "representation": "skeleton",
                }
            ],
        )

    def test_false_sufficient_is_a_failure(self):
        result = case_result(
            "false-sufficient",
            missed_paths=["src/a.rs"],
            sufficiency_status="sufficient",
            false_sufficient=True,
        )
        summary = run.summarize([result])
        self.assertFalse(summary["ok"])
        self.assertEqual(summary["failures"], ["false-sufficient"])
        self.assertEqual(summary["metrics"]["sufficiencyFalseSufficientCount"], 1)


class CaseSelectionTest(unittest.TestCase):
    def case_files(self) -> list[dict]:
        return [
            {
                "name": "a",
                "repoSource": {"kind": "fixture", "path": "fixtures/a"},
                "cases": [
                    {"id": "mined-alpha-pack"},
                    {"id": "mined-alpha-query"},
                ],
            },
            {
                "name": "b",
                "repoSource": {"kind": "fixture", "path": "fixtures/b"},
                "cases": [{"id": "curated-beta-pack"}],
            },
        ]

    def args(
        self, case_id: list[str] | None = None, case_glob: list[str] | None = None
    ) -> argparse.Namespace:
        return argparse.Namespace(case_id=case_id or [], case_glob=case_glob or [])

    def test_no_selection_keeps_case_files_unmarked(self):
        case_files = self.case_files()
        selected, selection = run.select_case_files(case_files, self.args())

        self.assertIs(selected, case_files)
        self.assertIsNone(selection)

    def test_exact_and_glob_selection_preserve_order_without_duplicates(self):
        selected, selection = run.select_case_files(
            self.case_files(),
            self.args(
                case_id=["curated-beta-pack"],
                case_glob=["mined-alpha-*"],
            ),
        )

        self.assertEqual(
            [case["id"] for case_file in selected for case in case_file["cases"]],
            ["mined-alpha-pack", "mined-alpha-query", "curated-beta-pack"],
        )
        self.assertEqual(selection["selectedCaseCount"], 3)
        self.assertTrue(selection["diagnosticOnly"])

    def test_unknown_exact_case_is_rejected(self):
        with self.assertRaises(SystemExit):
            run.select_case_files(self.case_files(), self.args(case_id=["missing"]))

    def test_empty_glob_selection_is_rejected(self):
        with self.assertRaises(SystemExit):
            run.select_case_files(self.case_files(), self.args(case_glob=["nope-*"]))

    def test_filtered_run_cannot_write_baseline(self):
        args = self.args(case_id=["curated-beta-pack"])
        args.write_baseline = True
        _selected, selection = run.select_case_files(self.case_files(), args)

        with self.assertRaises(SystemExit):
            run.validate_case_selection_mode(args, selection)


class ParallelSuiteTest(unittest.TestCase):
    def test_parallel_suite_preserves_case_order(self):
        case_files = [
            {
                "repoSource": {"kind": "fixture", "path": "fixtures/a"},
                "cases": [{"id": "slow"}, {"id": "fast"}],
            },
            {
                "repoSource": {"kind": "fixture", "path": "fixtures/b"},
                "cases": [{"id": "middle"}],
            },
        ]
        args = type("Args", (), {"jobs": 3})()
        original_prepare = run.prepare_corpus
        original_score = run.score_case
        prepared = []

        def fake_prepare(files):
            prepared.extend(file["repoSource"]["path"] for file in files)

        def fake_score(case_file, case, args):
            if case["id"] == "slow":
                time.sleep(0.03)
            elif case["id"] == "middle":
                time.sleep(0.01)
            return case_result(case["id"])

        try:
            run.prepare_corpus = fake_prepare
            run.score_case = fake_score
            summary = run.run_suite(case_files, args)
        finally:
            run.prepare_corpus = original_prepare
            run.score_case = original_score

        self.assertEqual(prepared, ["fixtures/a", "fixtures/b"])
        self.assertEqual(
            [case["id"] for case in summary["cases"]],
            ["slow", "fast", "middle"],
        )

    def test_jobs_must_be_positive(self):
        self.assertEqual(run.positive_int("2"), 2)
        with self.assertRaises(argparse.ArgumentTypeError):
            run.positive_int("0")


class AblationReportTest(unittest.TestCase):
    def test_context_ablation_args_pass_through_to_cli(self):
        args = type(
            "Args",
            (),
            {"ablate_context_signal": ["graph", "co-change"]},
        )()

        self.assertEqual(
            run.context_ablation_command_args(args),
            [
                "--ablate-context-signal",
                "graph",
                "--ablate-context-signal",
                "co-change",
            ],
        )

    def test_ablation_entry_reports_metric_and_cohort_deltas(self):
        baseline = run.summarize(
            [
                case_result("base-self", source_group="self", truth_source="fixture"),
                case_result("base-external", source_group="external", truth_source="mined_followup"),
            ]
        )
        weak_external = case_result(
            "weak-external",
            source_group="external",
            truth_source="mined_followup",
            missed_paths=["src/a.rs"],
            sufficiency_status="insufficient",
        )
        weak_external = run.CaseResult(
            **{
                **weak_external.__dict__,
                "recall_at_10": 0.0,
                "recall_at_25": 0.0,
                "ndcg_at_10": 0.0,
                "candidate_present_missed_paths": ["src/a.rs"],
            }
        )
        ablated = run.summarize(
            [
                case_result("base-self", source_group="self", truth_source="fixture"),
                weak_external,
            ]
        )

        entry = run.ablation_entry("graph", baseline, ablated)

        self.assertEqual(entry["disabledSignals"], ["graph"])
        self.assertLess(entry["deltaVsBaseline"]["meanRecallAt10"], 0)
        self.assertLess(
            entry["cohorts"]["bySourceGroup"]["external"]["deltaVsBaseline"][
                "meanRecallAt10"
            ],
            0,
        )
        self.assertIn("weakCases", entry)


class SummaryCompareTest(unittest.TestCase):
    def test_metric_deltas_compare_common_metrics(self):
        baseline = {"metrics": {"meanRecallAt10": 0.5, "meanNdcgAt10": 0.25}}
        candidate = {"metrics": {"meanRecallAt10": 0.6, "meanNdcgAt10": 0.20}}

        deltas = compare.metric_deltas(
            baseline, candidate, ["meanRecallAt10", "meanNdcgAt10"]
        )

        self.assertEqual(deltas[0].name, "meanRecallAt10")
        self.assertAlmostEqual(deltas[0].delta, 0.1)
        self.assertAlmostEqual(deltas[1].delta, -0.05)

    def test_case_deltas_include_present_miss_delta(self):
        baseline = {
            "cases": [
                {
                    "id": "case-a",
                    "kind": "pack",
                    "source_group": "external",
                    "truth_source": "mined_followup",
                    "recall_at_10": 0.0,
                    "recall_at_25": 0.0,
                    "ndcg_at_10": 0.0,
                    "first_relevant_rank": None,
                    "tokens_to_first_relevant": None,
                    "candidate_present_missed_paths": ["src/a.rs"],
                }
            ]
        }
        candidate = {
            "cases": [
                {
                    "id": "case-a",
                    "kind": "pack",
                    "source_group": "external",
                    "truth_source": "mined_followup",
                    "recall_at_10": 0.5,
                    "recall_at_25": 0.5,
                    "ndcg_at_10": 0.2,
                    "first_relevant_rank": 3,
                    "tokens_to_first_relevant": 200,
                    "candidate_present_missed_paths": [],
                }
            ]
        }

        rows = compare.case_deltas(baseline, candidate, kind="pack")

        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0].case_id, "case-a")
        self.assertEqual(rows[0].deltas["recall_at_10"], 0.5)
        self.assertIsNone(rows[0].deltas["first_relevant_rank"])
        self.assertEqual(rows[0].candidate_present_miss_delta, -1)


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
