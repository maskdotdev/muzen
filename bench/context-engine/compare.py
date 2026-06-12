#!/usr/bin/env python3
"""Compare two context-engine summary artifacts.

Usage:
  python3 bench/context-engine/compare.py baseline.json candidate.json
"""

from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_METRICS = [
    "meanRecallAt5",
    "meanRecallAt10",
    "meanNdcgAt10",
    "meanRecallAt25",
    "meanCandidateRecall",
    "candidatePresentMissRate",
    "candidatePresentMissCaseRate",
    "meanCandidatePresentMissRate",
    "firstRelevantRate",
    "meanTokensToFirstRelevant",
    "meanPrecision",
    "meanUsefulEvidencePer1kTokens",
    "meanLatencyMs",
]

CASE_FIELDS = [
    "recall_at_10",
    "recall_at_25",
    "ndcg_at_10",
    "first_relevant_rank",
    "tokens_to_first_relevant",
]
CASE_LOWER_IS_BETTER_FIELDS = {
    "first_relevant_rank",
    "tokens_to_first_relevant",
    "candidate_present_miss_delta",
}

COHORT_GROUPS = ["byKind", "bySourceGroup", "byTruthSource"]

LOWER_IS_BETTER_PREFIXES = (
    "candidatePresentMiss",
    "meanCandidatePresentMiss",
    "meanTokensToFirstRelevant",
    "meanLatencyMs",
    "maxLatencyMs",
    "totalOmittedCandidates",
    "sufficiencyFalseSufficientCount",
)


@dataclass(frozen=True)
class MetricDelta:
    name: str
    baseline: float | int | None
    candidate: float | int | None

    @property
    def delta(self) -> float | int | None:
        if self.baseline is None or self.candidate is None:
            return None
        return self.candidate - self.baseline

    @property
    def status(self) -> str:
        return delta_status(self.name, self.delta)


@dataclass(frozen=True)
class CaseDelta:
    case_id: str
    source_group: str | None
    truth_source: str | None
    deltas: dict[str, float | int | None]
    candidate_present_miss_delta: int

    @property
    def status(self) -> str:
        statuses = [
            case_delta_status(field, delta)
            for field, delta in self.deltas.items()
            if delta is not None and delta != 0
        ]
        if self.candidate_present_miss_delta:
            statuses.append(
                case_delta_status(
                    "candidate_present_miss_delta",
                    self.candidate_present_miss_delta,
                )
            )
        improved = statuses.count("improved")
        regressed = statuses.count("regressed")
        if improved and regressed:
            return f"mixed(+{improved}/-{regressed})"
        if improved:
            return "improved"
        if regressed:
            return "regressed"
        return "flat"


@dataclass(frozen=True)
class CohortDelta:
    group: str
    cohort: str
    metric: str
    baseline: float | int | None
    candidate: float | int | None

    @property
    def delta(self) -> float | int | None:
        if self.baseline is None or self.candidate is None:
            return None
        return self.candidate - self.baseline

    @property
    def status(self) -> str:
        return delta_status(self.metric, self.delta)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument(
        "--case-limit",
        type=int,
        default=40,
        help="Maximum changed cases to print.",
    )
    parser.add_argument(
        "--kind",
        help="Restrict case deltas to one case kind, e.g. pack.",
    )
    return parser.parse_args()


def load_summary(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def metric_deltas(
    baseline: dict[str, Any], candidate: dict[str, Any], metrics: list[str] | None = None
) -> list[MetricDelta]:
    names = metrics or DEFAULT_METRICS
    baseline_metrics = baseline.get("metrics") or {}
    candidate_metrics = candidate.get("metrics") or {}
    return [
        MetricDelta(name, baseline_metrics.get(name), candidate_metrics.get(name))
        for name in names
        if name in baseline_metrics or name in candidate_metrics
    ]


def case_deltas(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
    *,
    kind: str | None = None,
) -> list[CaseDelta]:
    baseline_cases = {
        case["id"]: case
        for case in baseline.get("cases", [])
        if kind is None or case.get("kind") == kind
    }
    candidate_cases = {
        case["id"]: case
        for case in candidate.get("cases", [])
        if kind is None or case.get("kind") == kind
    }
    rows = []
    for case_id in sorted(set(baseline_cases) & set(candidate_cases)):
        base_case = baseline_cases[case_id]
        candidate_case = candidate_cases[case_id]
        deltas = {
            field: value_delta(base_case.get(field), candidate_case.get(field))
            for field in CASE_FIELDS
        }
        miss_delta = len(candidate_case.get("candidate_present_missed_paths", [])) - len(
            base_case.get("candidate_present_missed_paths", [])
        )
        if any(delta is not None and delta != 0 for delta in deltas.values()) or miss_delta:
            rows.append(
                CaseDelta(
                    case_id=case_id,
                    source_group=base_case.get("source_group"),
                    truth_source=base_case.get("truth_source"),
                    deltas=deltas,
                    candidate_present_miss_delta=miss_delta,
                )
            )
    return rows


def cohort_metric_deltas(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
    metrics: list[str] | None = None,
    groups: list[str] | None = None,
) -> list[CohortDelta]:
    names = metrics or DEFAULT_METRICS
    group_names = groups or COHORT_GROUPS
    baseline_cohorts = baseline.get("cohorts") or {}
    candidate_cohorts = candidate.get("cohorts") or {}
    rows: list[CohortDelta] = []
    for group in group_names:
        baseline_group = baseline_cohorts.get(group) or {}
        candidate_group = candidate_cohorts.get(group) or {}
        for cohort in sorted(set(baseline_group) & set(candidate_group)):
            baseline_metrics = baseline_group[cohort].get("metrics") or {}
            candidate_metrics = candidate_group[cohort].get("metrics") or {}
            for metric in names:
                if metric not in baseline_metrics and metric not in candidate_metrics:
                    continue
                row = CohortDelta(
                    group=group,
                    cohort=cohort,
                    metric=metric,
                    baseline=baseline_metrics.get(metric),
                    candidate=candidate_metrics.get(metric),
                )
                if row.delta not in (None, 0):
                    rows.append(row)
    rows.sort(key=lambda row: (row.group, row.cohort, row.metric))
    return rows


def value_delta(base: Any, candidate: Any) -> float | int | None:
    if base is None or candidate is None:
        if base == candidate:
            return 0
        return None
    if isinstance(base, (int, float)) and isinstance(candidate, (int, float)):
        return candidate - base
    return 0 if base == candidate else None


def lower_is_better(metric: str) -> bool:
    return metric.startswith(LOWER_IS_BETTER_PREFIXES)


def delta_status(metric: str, delta: float | int | None) -> str:
    if delta is None:
        return "unknown"
    if delta == 0:
        return "flat"
    improved = delta < 0 if lower_is_better(metric) else delta > 0
    return "improved" if improved else "regressed"


def case_delta_status(field: str, delta: float | int | None) -> str:
    if delta is None:
        return "unknown"
    if delta == 0:
        return "flat"
    improved = delta < 0 if field in CASE_LOWER_IS_BETTER_FIELDS else delta > 0
    return "improved" if improved else "regressed"


def format_number(value: Any) -> str:
    if value is None:
        return "None"
    if isinstance(value, float):
        return f"{value:.4f}"
    return str(value)


def print_metric_deltas(rows: list[MetricDelta]) -> None:
    print("Metric deltas")
    print(
        f"{'metric':36} {'baseline':>12} {'candidate':>12} "
        f"{'delta':>12} {'status':>10}"
    )
    for row in rows:
        print(
            f"{row.name:36} "
            f"{format_number(row.baseline):>12} "
            f"{format_number(row.candidate):>12} "
            f"{format_number(row.delta):>12} "
            f"{row.status:>10}"
        )


def print_scope_warning(baseline: dict[str, Any], candidate: dict[str, Any]) -> None:
    baseline_count = baseline.get("caseCount")
    candidate_count = candidate.get("caseCount")
    if baseline_count != candidate_count:
        print(
            "warning: summary metric deltas compare artifacts with different "
            f"case counts ({baseline_count} vs {candidate_count}); use case "
            "rows for scoped diagnostics."
        )
        print()


def print_case_deltas(rows: list[CaseDelta], limit: int) -> None:
    print()
    print("Changed cases")
    print(
        f"{'case':36} {'group':>9} {'truth':>15} "
        f"{'r10':>8} {'r25':>8} {'ndcg10':>8} {'rank':>8} {'tokens':>8} "
        f"{'miss':>6} {'status':>14}"
    )
    for row in rows[:limit]:
        print(
            f"{row.case_id:36} "
            f"{str(row.source_group):>9} "
            f"{str(row.truth_source):>15} "
            f"{format_number(row.deltas['recall_at_10']):>8} "
            f"{format_number(row.deltas['recall_at_25']):>8} "
            f"{format_number(row.deltas['ndcg_at_10']):>8} "
            f"{format_number(row.deltas['first_relevant_rank']):>8} "
            f"{format_number(row.deltas['tokens_to_first_relevant']):>8} "
            f"{row.candidate_present_miss_delta:>6} "
            f"{row.status:>14}"
        )
    if len(rows) > limit:
        print(f"... {len(rows) - limit} more changed cases")


def print_cohort_deltas(rows: list[CohortDelta]) -> None:
    if not rows:
        return
    print()
    print("Cohort metric deltas")
    print(
        f"{'group':14} {'cohort':16} {'metric':36} "
        f"{'baseline':>12} {'candidate':>12} {'delta':>12} {'status':>10}"
    )
    for row in rows:
        print(
            f"{row.group:14} "
            f"{row.cohort:16} "
            f"{row.metric:36} "
            f"{format_number(row.baseline):>12} "
            f"{format_number(row.candidate):>12} "
            f"{format_number(row.delta):>12} "
            f"{row.status:>10}"
        )


def main() -> int:
    args = parse_args()
    baseline = load_summary(args.baseline)
    candidate = load_summary(args.candidate)
    print_scope_warning(baseline, candidate)
    print_metric_deltas(metric_deltas(baseline, candidate))
    print_cohort_deltas(cohort_metric_deltas(baseline, candidate))
    print_case_deltas(case_deltas(baseline, candidate, kind=args.kind), args.case_limit)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
