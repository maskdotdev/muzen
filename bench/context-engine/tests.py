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
    graph_debug: dict | None = None,
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
        candidate_expected_paths=["src/a.rs"],
        ranked_retrieved_paths=["src/a.rs"],
        candidate_expected_count=1,
        retrieved_paths=["src/a.rs"],
        candidate_missed_paths=[],
        candidate_present_missed_paths=[],
        candidate_present_missed_omissions=[],
        selected_tail_candidates=[],
        selected_evictable_candidates=[],
        missed_paths=missed_paths or [],
        unexpected_paths=[],
        forbidden_content_hits=[],
        missing_required_content=[],
        trusted_forbidden_paths=[],
        missing_expected_ranges=[],
        token_estimate=10,
        selected_token_breakdown={
            "byRepresentation": {"full_content": {"count": 1, "tokens": 10}},
            "byKind": {"file_span": {"count": 1, "tokens": 10}},
            "changedTokens": 0,
            "topPathsByTokens": [{"path": "src/a.rs", "tokens": 10}],
        },
        omitted=0,
        sufficiency_status=sufficiency_status,
        sufficiency_blocking_gaps=0,
        sufficiency_false_sufficient=false_sufficient,
        graph_debug=graph_debug,
        cli_performance=None,
        performance={},
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
                "ranked_retrieved_paths": ["src/tail.rs"],
                "candidate_present_missed_paths": ["src/a.rs"],
                "candidate_present_missed_omissions": [
                    {
                        "evidenceId": "ev-1",
                        "kind": "file_span",
                        "path": "src/a.rs",
                        "signals": {"graphDistance": 1, "lexicalChangeScore": 0.5},
                        "score": 0.5,
                        "rankIndex": 12,
                        "tokenEstimate": 100,
                        "reason": "budget_exhausted",
                        "budgetState": {
                            "remainingTokens": 20,
                            "fullContentRemainingTokens": 0,
                            "fullContentShortfallTokens": 100,
                            "skeletonTokenEstimate": 30,
                            "skeletonShortfallTokens": 10,
                        },
                        "graphPaths": [{"kind": "imports"}],
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
                "selected_evictable_candidates": [
                    {
                        "evidenceId": "evictable-1",
                        "kind": "file_span",
                        "path": "src/evictable.rs",
                        "score": 0.42,
                        "rankIndex": 20,
                        "tokenEstimate": 40,
                        "representation": "full_content",
                    }
                ],
                "selected_token_breakdown": {
                    "byRepresentation": {
                        "full_content": {"count": 2, "tokens": 900},
                        "skeleton": {"count": 3, "tokens": 120},
                    },
                    "byKind": {"file_span": {"count": 5, "tokens": 1020}},
                    "changedTokens": 300,
                    "topPathsByTokens": [{"path": "src/tail.rs", "tokens": 300}],
                },
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
        self.assertEqual(
            summary["weakCases"][0]["selectedEvictableCandidates"][0]["evidenceId"],
            "evictable-1",
        )
        pressure = summary["diagnostics"]["omissionPressure"][
            "candidatePresentMissOmissions"
        ]
        self.assertEqual(pressure["count"], 1)
        self.assertEqual(pressure["caseCount"], 1)
        self.assertEqual(pressure["byReason"], {"budget_exhausted": 1})
        self.assertEqual(pressure["withGraphPathCount"], 1)
        self.assertEqual(pressure["medianRankIndex"], 12)
        self.assertEqual(pressure["p90RankIndex"], 12)
        self.assertEqual(pressure["meanTokenEstimate"], 100)
        self.assertEqual(pressure["budgetState"]["meanRemainingTokens"], 20)
        self.assertEqual(
            pressure["budgetState"]["meanFullContentRemainingTokens"], 0
        )
        self.assertEqual(
            pressure["budgetState"]["meanFullContentShortfallTokens"], 100
        )
        self.assertEqual(pressure["budgetState"]["skeletonAvailableCount"], 1)
        self.assertEqual(pressure["budgetState"]["skeletonFitsRemainingCount"], 0)
        self.assertEqual(pressure["budgetState"]["meanSkeletonTokenEstimate"], 30)
        self.assertEqual(pressure["budgetState"]["meanSkeletonShortfallTokens"], 10)
        self.assertEqual(pressure["signals"]["graphDistanceCounts"], {"1": 1})
        self.assertEqual(pressure["signals"]["meanLexicalChangeScore"], 0.5)
        self.assertEqual(pressure["scoreBeatsSelectedTailCaseCount"], 1)
        self.assertEqual(pressure["scoreBeatsSelectedTailCases"][0]["id"], "weak")
        self.assertEqual(pressure["fullRepairShortfallCaseCount"], 1)
        self.assertEqual(
            pressure["fullRepairShortfallCases"][0]["shortfalls"][0]["shortfallTokens"],
            60,
        )
        ranked_causes = summary["diagnostics"]["rankedMissCauses"]
        self.assertEqual(ranked_causes["top25MissedPathCount"], 1)
        self.assertEqual(ranked_causes["selectedAfter25Count"], 0)
        self.assertEqual(ranked_causes["candidatePresentOmittedCount"], 1)
        self.assertEqual(ranked_causes["candidateAbsentCount"], 0)
        self.assertEqual(
            ranked_causes["candidatePresentOmittedCases"][0]["path"], "src/a.rs"
        )
        budget_pressure = summary["diagnostics"]["selectionBudgetPressure"]
        self.assertEqual(budget_pressure["candidatePresentMissCasesWithSkeletons"], 1)
        self.assertEqual(
            budget_pressure["meanCandidatePresentMissOmissionTokens"], 100
        )
        self.assertEqual(
            budget_pressure["candidatePresentMissCasesWithSkeletonsSample"][0]["id"],
            "weak",
        )
        self.assertIn("selectedTokenBreakdown", summary["weakCases"][0])

    def test_summary_separates_strict_and_probable_sufficiency(self):
        sufficient = case_result("sufficient", sufficiency_status="sufficient")
        probable = case_result("probable", sufficiency_status="probably_sufficient")
        conservative = case_result("conservative", sufficiency_status="insufficient")
        incomplete = case_result(
            "incomplete",
            missed_paths=["src/a.rs"],
            sufficiency_status="insufficient",
        )

        summary = run.summarize([sufficient, probable, conservative, incomplete])

        self.assertEqual(
            summary["metrics"]["sufficiencySufficientWhenComplete"],
            1 / 3,
        )
        self.assertEqual(
            summary["metrics"]["sufficiencyProbablySufficientWhenComplete"],
            1 / 3,
        )
        self.assertEqual(
            summary["metrics"]["sufficiencyNotInsufficientWhenComplete"],
            2 / 3,
        )
        self.assertEqual(
            summary["metrics"]["sufficiencyInsufficientWhenIncomplete"],
            1.0,
        )

    def test_summary_reports_sufficiency_calibration_buckets(self):
        budget_only = run.CaseResult(
            **{
                **case_result(
                    "budget-only",
                    sufficiency_status="insufficient",
                ).__dict__,
                "omitted": 7,
            }
        )
        complete_with_gaps = run.CaseResult(
            **{
                **case_result(
                    "complete-with-gaps",
                    sufficiency_status="insufficient",
                ).__dict__,
                "sufficiency_blocking_gaps": 2,
                "omitted": 3,
            }
        )
        incomplete_gapless = case_result(
            "incomplete-gapless",
            missed_paths=["src/a.rs"],
            sufficiency_status="insufficient",
        )

        summary = run.summarize([budget_only, complete_with_gaps, incomplete_gapless])
        calibration = summary["diagnostics"]["sufficiencyCalibration"]

        self.assertEqual(calibration["caseCount"], 3)
        self.assertEqual(calibration["pathCompleteCount"], 2)
        self.assertEqual(calibration["pathIncompleteCount"], 1)
        self.assertEqual(calibration["completeStatusCounts"], {"insufficient": 2})
        self.assertEqual(calibration["incompleteStatusCounts"], {"insufficient": 1})
        self.assertEqual(calibration["completeInsufficientCount"], 2)
        self.assertEqual(calibration["completeInsufficientWithBlockingGapsCount"], 1)
        self.assertEqual(calibration["completeInsufficientBudgetOnlyCount"], 1)
        self.assertEqual(calibration["incompleteWithoutBlockingGapsCount"], 1)
        self.assertEqual(
            calibration["completeInsufficientBudgetOnlyCases"][0]["id"],
            "budget-only",
        )
        self.assertEqual(
            calibration["incompleteWithoutBlockingGapsCases"][0]["id"],
            "incomplete-gapless",
        )

    def test_summary_reports_slowest_cases(self):
        fast = case_result("fast")
        slow = run.CaseResult(
            **{
                **case_result("slow", source_group="external").__dict__,
                "latency_ms": 42.0,
                "token_estimate": 1200,
                "omitted": 17,
                "candidate_present_missed_paths": ["src/missed.rs"],
            }
        )

        summary = run.summarize([fast, slow])

        self.assertEqual(summary["slowCases"][0]["id"], "slow")
        self.assertEqual(summary["slowCases"][0]["sourceGroup"], "external")
        self.assertEqual(summary["slowCases"][0]["latencyMs"], 42.0)
        self.assertEqual(summary["slowCases"][0]["tokenEstimate"], 1200)
        self.assertEqual(summary["slowCases"][0]["omitted"], 17)
        self.assertEqual(summary["slowCases"][0]["candidatePresentMissCount"], 1)

    def test_ranked_miss_causes_separate_late_selected_and_absent_paths(self):
        late_paths = [f"src/noise_{index}.rs" for index in range(25)] + [
            "src/late.rs"
        ]
        late = run.CaseResult(
            **{
                **case_result("late").__dict__,
                "expected_paths": ["src/late.rs"],
                "candidate_expected_paths": ["src/late.rs"],
                "ranked_retrieved_paths": late_paths,
                "retrieved_paths": late_paths,
                "recall_at_25": 0.0,
            }
        )
        absent = run.CaseResult(
            **{
                **case_result("absent").__dict__,
                "expected_paths": ["src/absent.rs"],
                "candidate_expected_paths": ["src/absent.rs"],
                "ranked_retrieved_paths": ["src/other.rs"],
                "retrieved_paths": ["src/other.rs"],
                "candidate_missed_paths": ["src/absent.rs"],
                "recall_at_25": 0.0,
            }
        )

        causes = run.ranked_miss_causes([late, absent])

        self.assertEqual(causes["top25MissedPathCount"], 2)
        self.assertEqual(causes["selectedAfter25Count"], 1)
        self.assertEqual(causes["selectedAfter25Cases"][0]["path"], "src/late.rs")
        self.assertEqual(causes["selectedAfter25Cases"][0]["rank"], 26)
        self.assertEqual(causes["candidatePresentOmittedCount"], 0)
        self.assertEqual(causes["candidateAbsentCount"], 1)
        self.assertEqual(causes["candidateAbsentCases"][0]["path"], "src/absent.rs")

    def test_summary_performance_block_reports_wall_clock_throughput(self):
        summary = run.summarize([case_result("a"), case_result("b")])

        run.attach_performance(summary, elapsed_ms=4000.0, jobs=2)

        self.assertEqual(summary["performance"]["wallClockMs"], 4000.0)
        self.assertEqual(summary["performance"]["meanWallClockMsPerCase"], 2000.0)
        self.assertEqual(summary["performance"]["casesPerSecond"], 0.5)
        self.assertEqual(summary["performance"]["jobs"], 2)
        self.assertEqual(summary["performance"]["phaseTotals"]["scoringMs"], 0.0)

    def test_repo_cache_root_is_per_materialized_checkout(self):
        with tempfile.TemporaryDirectory() as tmp:
            original_corpus_cache = run.CORPUS_CACHE
            try:
                run.CORPUS_CACHE = Path(tmp)
                first = run.cache_root_for_repo(Path(tmp) / ("a" * 12))
                second = run.cache_root_for_repo(Path(tmp) / ("b" * 12))
            finally:
                run.CORPUS_CACHE = original_corpus_cache

        self.assertNotEqual(first, second)
        self.assertEqual(first.name, "a" * 12)
        self.assertEqual(second.name, "b" * 12)

    def test_materialize_repo_uses_origin_mirror_and_commit_worktree(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            origin = tmp_path / "origin"
            origin.mkdir()
            corpus = tmp_path / "corpus"
            source = {
                "kind": "git",
                "origin": str(origin),
                "commit": "a" * 40,
            }
            calls = []
            original_corpus_cache = run.CORPUS_CACHE
            original_subprocess_run = run.subprocess.run

            def fake_run(command, **_kwargs):
                calls.append(command)
                if command[:3] == ["git", "clone", "--quiet"]:
                    Path(command[-1]).mkdir(parents=True)
                if "worktree" in command and "add" in command:
                    target = Path(command[-2])
                    target.mkdir(parents=True)
                    (target / ".git").write_text("gitdir: mirror/worktrees/a\n")
                return subprocess.CompletedProcess(command, 0)

            try:
                run.CORPUS_CACHE = corpus
                run.subprocess.run = fake_run
                target = run.materialize_repo(source)
                second = run.materialize_repo(
                    {**source, "commit": "b" * 40}
                )
            finally:
                run.CORPUS_CACHE = original_corpus_cache
                run.subprocess.run = original_subprocess_run

            clone_calls = [call for call in calls if call[:4] == ["git", "clone", "--quiet", "--mirror"]]
            fetch_calls = [call for call in calls if "fetch" in call]
            worktree_calls = [call for call in calls if "worktree" in call and "add" in call]

            self.assertEqual(len(clone_calls), 1)
            self.assertEqual(len(fetch_calls), 1)
            self.assertEqual(len(worktree_calls), 2)
            self.assertEqual(target, corpus / ("a" * 12))
            self.assertEqual(second, corpus / ("b" * 12))

    def test_selected_tail_details_join_scores_to_evidence(self):
        result = {
            "selectedCandidates": [
                {"evidenceId": "a", "score": 0.9, "rankIndex": 0},
                {"evidenceId": "b", "score": 0.4, "rankIndex": 20},
            ],
            "relationships": [
                {
                    "from": "a",
                    "to": "b",
                    "kind": "tests",
                    "confidence": 0.8,
                    "reason": "test path -> implementation path",
                }
            ],
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
                    "graphPaths": [
                        {
                            "kind": "tests",
                            "confidence": 0.8,
                            "path": "test path -> implementation path",
                        }
                    ],
                }
            ],
        )

    def test_selected_evictable_details_reports_low_score_full_content(self):
        result = {
            "selectedCandidates": [
                {"evidenceId": "changed", "score": -0.1, "rankIndex": 0},
                {"evidenceId": "skeleton", "score": 0.1, "rankIndex": 1},
                {"evidenceId": "high", "score": 0.6, "rankIndex": 2},
                {"evidenceId": "low", "score": 0.2, "rankIndex": 3},
            ]
        }
        evidence = [
            {
                "id": "changed",
                "kind": "symbol",
                "path": "src/changed.rs",
                "tokenEstimate": 10,
                "representation": "full_content",
                "isChangedSpan": True,
            },
            {
                "id": "skeleton",
                "kind": "file_span",
                "path": "src/skeleton.rs",
                "tokenEstimate": 20,
                "representation": "skeleton",
            },
            {
                "id": "high",
                "kind": "file_span",
                "path": "src/high.rs",
                "tokenEstimate": 30,
                "representation": "full_content",
            },
            {
                "id": "low",
                "kind": "file_span",
                "path": "src/low.rs",
                "tokenEstimate": 40,
                "representation": "full_content",
            },
        ]

        evictable = run.selected_evictable_details(result, evidence)

        self.assertEqual(
            evictable,
            [
                {
                    "evidenceId": "low",
                    "kind": "file_span",
                    "path": "src/low.rs",
                    "score": 0.2,
                    "rankIndex": 3,
                    "tokenEstimate": 40,
                    "representation": "full_content",
                }
            ],
        )

    def test_omitted_details_preserve_graph_paths(self):
        omitted = [
            {
                "evidenceId": "missed-1",
                "kind": "file_span",
                "path": "src/missed.rs",
                "signals": {"graphDistance": 1, "lexicalChangeScore": 0.5},
                "score": 0.5,
                "rankIndex": 12,
                "tokenEstimate": 100,
                "reason": "budget_exhausted",
                "budgetState": {
                    "remainingTokens": 20,
                    "fullContentRemainingTokens": 0,
                    "fullContentShortfallTokens": 100,
                },
                "graphPaths": [
                    {
                        "kind": "imports",
                        "confidence": 0.9,
                        "path": "src/changed.rs imports src/missed.rs",
                    }
                ],
            },
            {
                "evidenceId": "other-1",
                "kind": "file_span",
                "path": "src/other.rs",
                "score": 0.4,
                "rankIndex": 13,
                "tokenEstimate": 90,
                "reason": "budget_exhausted",
            },
        ]

        details = run.omitted_details_for_paths(omitted, ["src/missed.rs"])

        self.assertEqual(
            details,
            [
                {
                    "evidenceId": "missed-1",
                    "kind": "file_span",
                    "path": "src/missed.rs",
                    "signals": {"graphDistance": 1, "lexicalChangeScore": 0.5},
                    "score": 0.5,
                    "rankIndex": 12,
                    "tokenEstimate": 100,
                    "reason": "budget_exhausted",
                    "budgetState": {
                        "remainingTokens": 20,
                        "fullContentRemainingTokens": 0,
                        "fullContentShortfallTokens": 100,
                    },
                    "graphPaths": [
                        {
                            "kind": "imports",
                            "confidence": 0.9,
                            "path": "src/changed.rs imports src/missed.rs",
                        }
                    ],
                }
            ],
        )

    def test_graph_debug_diagnostic_reports_raw_graph_coverage(self):
        export = {
            "schemaVersion": run.GRAPH_DEBUG_SCHEMA_VERSION,
            "nodeCount": 5,
            "edgeCount": 7,
            "changedAnchors": ["file:src/changed.rs"],
            "candidates": [
                {"path": "src/found.rs"},
                {"path": "src/found.rs"},
                {"path": "src/other.rs"},
            ],
            "truncatedCandidates": 2,
            "omitted": [
                {
                    "node": "file:src/missed.rs",
                    "path": "src/missed.rs",
                    "anchor": "file:src/changed.rs",
                    "reason": "depth_limit",
                }
            ],
            "truncatedOmissions": 1,
            "omittedCountsByReason": {"depth_limit": 1, "budget_exceeded": 3},
            "edgeConfidenceByKind": {
                "imports": {"count": 2, "min": 0.5, "max": 1.0, "mean": 0.75}
            },
        }

        diagnostic = run.graph_debug_diagnostic(
            export,
            {"src/found.rs", "src/missed.rs"},
            latency_ms=12.5,
        )

        self.assertEqual(diagnostic["acceptedPathRecall"], 0.5)
        self.assertEqual(diagnostic["reachablePathRecall"], 1.0)
        self.assertEqual(diagnostic["candidateCount"], 5)
        self.assertEqual(diagnostic["omittedCount"], 2)
        self.assertEqual(diagnostic["candidatePathCount"], 2)
        self.assertEqual(diagnostic["acceptedFoundPaths"], ["src/found.rs"])
        self.assertEqual(diagnostic["acceptedMissedPaths"], ["src/missed.rs"])
        self.assertEqual(
            diagnostic["reachableFoundPaths"],
            ["src/found.rs", "src/missed.rs"],
        )
        self.assertEqual(diagnostic["reachableMissedPaths"], [])
        self.assertEqual(diagnostic["omittedOnlyExpectedPaths"], ["src/missed.rs"])
        self.assertEqual(
            diagnostic["omittedExpectedPaths"],
            [
                {
                    "node": "file:src/missed.rs",
                    "path": "src/missed.rs",
                    "anchor": "file:src/changed.rs",
                    "reason": "depth_limit",
                }
            ],
        )
        self.assertEqual(diagnostic["omittedExpectedPathCount"], 1)
        self.assertEqual(diagnostic["omittedExpectedPathsTruncated"], 0)
        self.assertEqual(diagnostic["omittedExpectedCountsByReason"], {"depth_limit": 1})
        self.assertEqual(diagnostic["edgeKindCounts"], {"imports": 2})

    def test_summary_reports_graph_coverage_when_enabled(self):
        graph_debug = {
            "latencyMs": 12.0,
            "expectedPathCount": 2,
            "candidatePathCount": 3,
            "acceptedFoundPathCount": 1,
            "acceptedMissedPathCount": 1,
            "acceptedPathRecall": 0.5,
            "reachableFoundPathCount": 2,
            "reachableMissedPathCount": 0,
            "reachablePathRecall": 1.0,
            "acceptedFoundPaths": ["src/found.rs"],
            "acceptedMissedPaths": ["src/missed.rs"],
            "reachableFoundPaths": ["src/found.rs", "src/missed.rs"],
            "reachableMissedPaths": [],
            "omittedOnlyExpectedPaths": ["src/missed.rs"],
            "omittedExpectedPathCount": 1,
            "omittedExpectedPaths": [
                {"path": "src/missed.rs", "reason": "depth_limit"}
            ],
            "omittedExpectedPathsTruncated": 0,
            "omittedExpectedCountsByReason": {"depth_limit": 1},
            "omittedCountsByReason": {"depth_limit": 2},
            "edgeKindCounts": {"imports": 4},
        }
        result = case_result("graph-miss", graph_debug=graph_debug)

        summary = run.summarize([result])
        coverage = summary["diagnostics"]["graphCoverage"]

        self.assertTrue(coverage["enabled"])
        self.assertEqual(coverage["caseCount"], 1)
        self.assertEqual(coverage["acceptedPathRecall"], 0.5)
        self.assertEqual(coverage["reachablePathRecall"], 1.0)
        self.assertEqual(coverage["acceptedMissedPathCount"], 1)
        self.assertEqual(coverage["reachableMissedPathCount"], 0)
        self.assertEqual(coverage["omittedExpectedPathCount"], 1)
        self.assertEqual(coverage["edgeKindObservationCounts"], {"imports": 4})
        self.assertEqual(coverage["omittedCountsByReason"], {"depth_limit": 2})
        self.assertEqual(
            coverage["omittedExpectedCountsByReason"], {"depth_limit": 1}
        )
        self.assertEqual(coverage["weakCases"][0]["id"], "graph-miss")
        self.assertEqual(
            summary["weakCases"][0]["graphDebug"]["acceptedMissedPaths"],
            ["src/missed.rs"],
        )
        self.assertEqual(
            summary["weakCases"][0]["graphDebug"]["reachableMissedPaths"],
            [],
        )

    def test_false_sufficient_is_a_failure(self):
        result = run.CaseResult(
            **{
                **case_result(
                    "false-sufficient",
                    missed_paths=["src/a.rs"],
                    sufficiency_status="sufficient",
                    false_sufficient=True,
                ).__dict__,
                "candidate_missed_paths": ["src/a.rs"],
                "candidate_present_missed_paths": ["src/b.rs"],
                "candidate_present_missed_omissions": [
                    {"path": "src/b.rs", "reason": "budget_exhausted"}
                ],
            }
        )
        summary = run.summarize([result])
        self.assertFalse(summary["ok"])
        self.assertEqual(summary["failures"], ["false-sufficient"])
        self.assertEqual(summary["metrics"]["sufficiencyFalseSufficientCount"], 1)
        diagnostic = summary["diagnostics"]["falseSufficientCases"][0]
        self.assertEqual(diagnostic["id"], "false-sufficient")
        self.assertEqual(diagnostic["missedPaths"], ["src/a.rs"])
        self.assertEqual(diagnostic["candidateMissedPaths"], ["src/a.rs"])
        self.assertEqual(diagnostic["candidatePresentMissedPaths"], ["src/b.rs"])
        self.assertEqual(
            diagnostic["candidatePresentMissedOmissions"][0]["reason"],
            "budget_exhausted",
        )


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
    def test_explicit_muzen_binary_skips_cargo_build(self):
        explicit = Path("/tmp/custom-muzen")

        self.assertEqual(run.resolve_muzen_bin(explicit), explicit)

    def test_default_muzen_binary_builds_once(self):
        original_run = run.subprocess.run
        calls = []

        def fake_run(command, **kwargs):
            calls.append((command, kwargs))
            return subprocess.CompletedProcess(command, 0)

        try:
            run.subprocess.run = fake_run
            resolved = run.resolve_muzen_bin(None)
        finally:
            run.subprocess.run = original_run

        self.assertEqual(resolved, run.default_muzen_binary())
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0], ["cargo", "build", "--quiet", "--bin", "muzen"])
        self.assertEqual(calls[0][1]["cwd"], run.ROOT)
        self.assertTrue(calls[0][1]["check"])

    def test_release_muzen_binary_builds_once(self):
        original_run = run.subprocess.run
        calls = []

        def fake_run(command, **kwargs):
            calls.append((command, kwargs))
            return subprocess.CompletedProcess(command, 0)

        try:
            run.subprocess.run = fake_run
            resolved = run.resolve_muzen_bin(None, release=True)
        finally:
            run.subprocess.run = original_run

        self.assertEqual(resolved, run.default_muzen_binary(release=True))
        self.assertEqual(len(calls), 1)
        self.assertEqual(
            calls[0][0], ["cargo", "build", "--quiet", "--bin", "muzen", "--release"]
        )
        self.assertEqual(calls[0][1]["cwd"], run.ROOT)
        self.assertTrue(calls[0][1]["check"])

    def test_default_muzen_binary_rejects_stale_local_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "target" / "debug" / "muzen"
            source = root / "src" / "lib.rs"
            binary.parent.mkdir(parents=True)
            source.parent.mkdir(parents=True)
            binary.write_text("bin")
            source.write_text("src")
            run.os.utime(binary, (10, 10))
            run.os.utime(source, (20, 20))
            original_default = run.default_muzen_binary
            original_inputs = run.muzen_build_input_paths

            try:
                run.default_muzen_binary = lambda release=False: binary
                run.muzen_build_input_paths = lambda: [source]
                with self.assertRaises(SystemExit) as raised:
                    run.validate_muzen_binary_freshness(binary)
            finally:
                run.default_muzen_binary = original_default
                run.muzen_build_input_paths = original_inputs

        self.assertIn("older than", str(raised.exception))

    def test_default_muzen_binary_accepts_fresh_local_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "target" / "debug" / "muzen"
            source = root / "src" / "lib.rs"
            binary.parent.mkdir(parents=True)
            source.parent.mkdir(parents=True)
            binary.write_text("bin")
            source.write_text("src")
            run.os.utime(source, (10, 10))
            run.os.utime(binary, (20, 20))
            original_default = run.default_muzen_binary
            original_inputs = run.muzen_build_input_paths

            try:
                run.default_muzen_binary = lambda release=False: binary
                run.muzen_build_input_paths = lambda: [source]
                run.validate_muzen_binary_freshness(binary)
            finally:
                run.default_muzen_binary = original_default
                run.muzen_build_input_paths = original_inputs

    def test_custom_muzen_binary_skips_local_freshness_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            default_binary = root / "target" / "debug" / "muzen"
            custom_binary = root / "custom-muzen"
            source = root / "src" / "lib.rs"
            default_binary.parent.mkdir(parents=True)
            source.parent.mkdir(parents=True)
            default_binary.write_text("default")
            custom_binary.write_text("custom")
            source.write_text("src")
            run.os.utime(custom_binary, (10, 10))
            run.os.utime(source, (20, 20))
            original_default = run.default_muzen_binary
            original_inputs = run.muzen_build_input_paths

            try:
                run.default_muzen_binary = lambda release=False: default_binary
                run.muzen_build_input_paths = lambda: [source]
                run.validate_muzen_binary_freshness(custom_binary)
            finally:
                run.default_muzen_binary = original_default
                run.muzen_build_input_paths = original_inputs

    def test_eval_run_metadata_records_binary_and_git_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "target" / "debug" / "muzen"
            binary.parent.mkdir(parents=True)
            binary.write_text("bin")
            run.os.utime(binary, (20, 20))
            args = type(
                "Args",
                (),
                {
                    "muzen_bin": binary,
                    "hosted_semantic": True,
                    "hosted_embedding_model": "text-embedding-3-small",
                    "hosted_embedding_base_url": "https://embeddings.example/v1",
                    "hosted_max_embedding_inputs": 4096,
                    "local_onnx_model_dir": None,
                    "rerank_base_url": "https://rerank.example",
                    "rerank_model": "rerank-v1",
                    "ablate_context_signal": ["pack-repair"],
                },
            )()
            original_default = run.default_muzen_binary
            original_git_output = run.git_output

            def fake_git_output(command):
                if command == ["rev-parse", "HEAD"]:
                    return "abc123"
                if command == ["status", "--porcelain"]:
                    return " M src/lib.rs"
                return None

            try:
                run.default_muzen_binary = lambda release=False: binary
                run.git_output = fake_git_output
                metadata = run.eval_run_metadata(args)
            finally:
                run.default_muzen_binary = original_default
                run.git_output = original_git_output

        self.assertEqual(metadata["muzenBin"], str(binary))
        self.assertEqual(metadata["muzenBinMtimeUnixMs"], 20_000)
        self.assertTrue(metadata["defaultBinaryFreshnessChecked"])
        self.assertEqual(metadata["buildProfile"], "debug")
        self.assertEqual(metadata["gitHead"], "abc123")
        self.assertTrue(metadata["gitDirty"])
        self.assertEqual(metadata["semantic"]["forcedTier"], "hosted")
        self.assertEqual(
            metadata["semantic"]["hostedEmbeddingModel"], "text-embedding-3-small"
        )
        self.assertEqual(
            metadata["semantic"]["hostedEmbeddingBaseUrl"],
            "https://embeddings.example/v1",
        )
        self.assertEqual(metadata["semantic"]["hostedMaxEmbeddingInputs"], 4096)
        self.assertIsNone(metadata["semantic"]["localOnnxModelDir"])
        self.assertTrue(metadata["rerank"]["enabled"])
        self.assertEqual(metadata["rerank"]["baseUrl"], "https://rerank.example")
        self.assertEqual(metadata["rerank"]["model"], "rerank-v1")
        self.assertEqual(metadata["ablateContextSignals"], ["pack-repair"])

    def test_eval_run_metadata_records_local_onnx_tier(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "muzen"
            binary.write_text("bin")
            model_dir = Path(tmp) / "model"
            model_dir.mkdir()
            args = type(
                "Args",
                (),
                {
                    "muzen_bin": binary,
                    "hosted_semantic": False,
                    "hosted_embedding_model": "text-embedding-3-small",
                    "hosted_embedding_base_url": None,
                    "hosted_max_embedding_inputs": 512,
                    "local_onnx_model_dir": model_dir,
                    "rerank_base_url": None,
                    "rerank_model": None,
                    "ablate_context_signal": [],
                },
            )()

            metadata = run.eval_run_metadata(args)

        self.assertEqual(metadata["semantic"]["forcedTier"], "local_onnx")
        self.assertEqual(metadata["semantic"]["localOnnxModelDir"], str(model_dir))
        self.assertFalse(metadata["rerank"]["enabled"])

    def test_graph_debug_runs_public_cli_with_common_context(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            cache_root = root / "cache"
            cache_root.mkdir()
            binary = root / "muzen"
            binary.write_text("bin")
            args = type(
                "Args",
                (),
                {
                    "muzen_bin": binary,
                    "local_onnx_model_dir": None,
                    "hosted_semantic": False,
                    "hosted_embedding_model": "text-embedding-3-small",
                    "hosted_embedding_base_url": None,
                    "hosted_max_embedding_inputs": 512,
                    "rerank_base_url": None,
                    "rerank_model": None,
                    "rerank_credential_ref": None,
                    "ablate_context_signal": ["graph"],
                },
            )()
            case_file = {
                "repoSource": {"kind": "fixture", "path": "unused"},
                "changedFiles": ["src/changed.rs"],
                "hostMetadata": {"ticket": "parent"},
            }
            case = {
                "id": "graph-case",
                "command": "pack",
                "hostInstructions": [
                    {"kind": "ticket", "text": "child", "trusted": True}
                ],
            }
            original_materialize_repo = run.materialize_repo
            original_materialize_diff = run.materialize_diff
            original_cache_root = run.cache_root_for_repo
            original_run = run.subprocess.run
            calls = []
            seen_host_metadata = None
            seen_host_instructions = None

            def fake_run(command, **kwargs):
                nonlocal seen_host_metadata, seen_host_instructions
                calls.append((command, kwargs))
                metadata_path = Path(command[command.index("--host-metadata-json") + 1])
                instructions_path = Path(
                    command[command.index("--host-instruction-json") + 1]
                )
                seen_host_metadata = json.loads(metadata_path.read_text())
                seen_host_instructions = json.loads(instructions_path.read_text())
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout=json.dumps(
                        {
                            "schemaVersion": run.GRAPH_DEBUG_SCHEMA_VERSION,
                            "nodeCount": 0,
                            "edgeCount": 0,
                            "changedAnchors": [],
                            "candidates": [],
                            "truncatedCandidates": 0,
                            "omitted": [],
                            "truncatedOmissions": 0,
                            "omittedCountsByReason": {},
                            "edgeConfidenceByKind": {},
                        }
                    ),
                    stderr="",
                )

            try:
                run.materialize_repo = lambda _source: repo
                run.materialize_diff = lambda _source: None
                run.cache_root_for_repo = lambda _repo: cache_root
                run.subprocess.run = fake_run
                run.run_context_graph_debug(case_file, case, args)
            finally:
                run.materialize_repo = original_materialize_repo
                run.materialize_diff = original_materialize_diff
                run.cache_root_for_repo = original_cache_root
                run.subprocess.run = original_run

        command, kwargs = calls[0]
        self.assertEqual(command[:3], [str(binary), "context", "graph-debug"])
        self.assertIn("--repo", command)
        self.assertEqual(command[command.index("--repo") + 1], str(repo))
        self.assertIn("--derived-cache-root", command)
        self.assertEqual(
            command[command.index("--derived-cache-root") + 1], str(cache_root)
        )
        self.assertIn("--changed-file", command)
        self.assertEqual(command[command.index("--changed-file") + 1], "src/changed.rs")
        self.assertIn("--ablate-context-signal", command)
        self.assertEqual(command[command.index("--ablate-context-signal") + 1], "graph")
        self.assertNotIn("--purpose", command)
        self.assertEqual(kwargs["cwd"], run.ROOT)
        self.assertEqual(seen_host_metadata, {"ticket": "parent"})
        self.assertEqual(
            seen_host_instructions,
            [{"kind": "ticket", "text": "child", "trusted": True}],
        )

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
        original_score_group = run.score_case_group
        prepared = []

        def fake_prepare(files):
            prepared.extend(file["repoSource"]["path"] for file in files)

        def fake_score_group(case_file, cases, args):
            case_ids = [case["id"] for case in cases]
            if "slow" in case_ids:
                time.sleep(0.03)
            elif "middle" in case_ids:
                time.sleep(0.01)
            return [case_result(case_id) for case_id in case_ids]

        try:
            run.prepare_corpus = fake_prepare
            run.score_case_group = fake_score_group
            summary = run.run_suite(case_files, args)
        finally:
            run.prepare_corpus = original_prepare
            run.score_case_group = original_score_group

        self.assertEqual(prepared, ["fixtures/a", "fixtures/b"])
        self.assertEqual(
            [case["id"] for case in summary["cases"]],
            ["slow", "fast", "middle"],
        )

    def test_jobs_must_be_positive(self):
        self.assertEqual(run.positive_int("2"), 2)
        with self.assertRaises(argparse.ArgumentTypeError):
            run.positive_int("0")

    def test_default_eval_jobs_uses_bounded_cpu_parallelism(self):
        original_cpu_count = run.os.cpu_count
        try:
            run.os.cpu_count = lambda: 12
            self.assertEqual(run.default_eval_jobs(), 24)
            run.os.cpu_count = lambda: 20
            self.assertEqual(run.default_eval_jobs(), 32)
            run.os.cpu_count = lambda: 2
            self.assertEqual(run.default_eval_jobs(), 4)
            run.os.cpu_count = lambda: None
            self.assertEqual(run.default_eval_jobs(), 1)
        finally:
            run.os.cpu_count = original_cpu_count


class AblationReportTest(unittest.TestCase):
    def test_context_ablation_args_pass_through_to_cli(self):
        args = type(
            "Args",
            (),
            {"ablate_context_signal": ["graph", "pack-repair"]},
        )()

        self.assertEqual(
            run.context_ablation_command_args(args),
            [
                "--ablate-context-signal",
                "graph",
                "--ablate-context-signal",
                "pack-repair",
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
