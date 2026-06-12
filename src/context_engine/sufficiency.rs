//! Per-hunk structural sufficiency (R6).
//!
//! A context set is sufficient for a review when, for every diff hunk:
//! the enclosing definition chunk is present, at least one caller/usage
//! is present (or the definition is verifiably unreferenced), and related
//! tests are present (or the absence of related tests is recorded
//! explicitly). Gaps carry a ready-to-run `context.*` query so an agentic
//! loop can fill them. The pack compiler and the `sufficiency_check`
//! query share this logic, so the two can never disagree.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::runtime::contracts::RepoPath;

use super::chunking::range_overlaps;
use super::graph::ContextEdgeKind;
use super::{
    ContextEvidence, ContextEvidenceKind, ContextIndex, ContextRange, ContextSufficiency,
    ContextSufficiencyStatus,
};

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextCoverageGapKind {
    EnclosingDefinition,
    Callers,
    Tests,
    /// The repository has no tests related to this hunk. Recorded
    /// explicitly; does not count against sufficiency.
    NoRelatedTests,
}

impl ContextCoverageGapKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnclosingDefinition => "enclosing_definition",
            Self::Callers => "callers",
            Self::Tests => "tests",
            Self::NoRelatedTests => "no_related_tests",
        }
    }

    /// Whether this gap counts against sufficiency (informational gaps
    /// like `no_related_tests` do not).
    fn blocks_sufficiency(self) -> bool {
        !matches!(self, Self::NoRelatedTests)
    }
}

/// One coverage gap for one diff hunk, with the query that would fill it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextSufficiencyGap {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub missing: Vec<ContextCoverageGapKind>,
    /// Ready-to-run `context.*` query (kind + arguments) for the most
    /// fundamental missing requirement.
    pub suggested_query: serde_json::Value,
}

/// Evaluate per-hunk coverage of `evidence` against the index.
///
/// `budget_exhausted` downgrades blocking gaps to `probably_sufficient`:
/// the compiler could not do better within budget, and the unresolved
/// gaps stay recorded.
pub(crate) fn evaluate_sufficiency(
    index: &ContextIndex,
    evidence: &[ContextEvidence],
    budget_exhausted: bool,
) -> ContextSufficiency {
    // Skeleton-representation evidence (R7) elides bodies: it cannot
    // satisfy enclosing-definition, caller, or test coverage. Coverage
    // counts full content only; the budget_exhausted downgrade already
    // accounts for packs that could not fit the full content.
    let evidence: Vec<&ContextEvidence> = evidence
        .iter()
        .filter(|item| item.representation == super::ContextEvidenceRepresentation::FullContent)
        .collect();
    let mut gaps: Vec<ContextSufficiencyGap> = Vec::new();
    let mut checked_hunks = 0usize;
    for (path_text, hunks) in &index.hunk_ranges {
        let Ok(path) = RepoPath::parse(path_text) else {
            continue;
        };
        // Caller coverage comes from the same Context Graph paths
        // retrieval uses: cross-file relationship edges into the changed
        // file or its symbols.
        let referencer_paths: BTreeSet<&RepoPath> = index
            .graph
            .file_referencers(&path)
            .filter(|edge| {
                matches!(
                    edge.kind,
                    ContextEdgeKind::Imports | ContextEdgeKind::References | ContextEdgeKind::Tests
                )
            })
            .filter_map(|edge| edge.from_path())
            .collect();
        let related_test_paths = related_test_paths(index, &path);
        for hunk in hunks {
            checked_hunks += 1;
            let mut missing: Vec<ContextCoverageGapKind> = Vec::new();
            let enclosing_present = evidence.iter().any(|item| {
                item.path.as_ref() == Some(&path)
                    && item
                        .range
                        .map(|range| range_overlaps(&range, hunk))
                        // Whole-file evidence without a range encloses
                        // every hunk in the file.
                        .unwrap_or(true)
            });
            if !enclosing_present {
                missing.push(ContextCoverageGapKind::EnclosingDefinition);
            }
            if !referencer_paths.is_empty() {
                let caller_present = evidence.iter().any(|item| {
                    item.path
                        .as_ref()
                        .map(|item_path| referencer_paths.contains(item_path))
                        .unwrap_or(false)
                });
                if !caller_present {
                    missing.push(ContextCoverageGapKind::Callers);
                }
            }
            if related_test_paths.is_empty() {
                missing.push(ContextCoverageGapKind::NoRelatedTests);
            } else {
                let test_present = evidence.iter().any(|item| {
                    item.kind == ContextEvidenceKind::Test
                        && item
                            .path
                            .as_ref()
                            .map(|item_path| related_test_paths.contains(item_path))
                            .unwrap_or(false)
                });
                if !test_present {
                    missing.push(ContextCoverageGapKind::Tests);
                }
            }
            if !missing.is_empty() {
                let suggested_query = suggested_query(&path, hunk, missing[0]);
                gaps.push(ContextSufficiencyGap {
                    path: path_text.clone(),
                    start_line: hunk.start_line,
                    end_line: hunk.end_line,
                    missing,
                    suggested_query,
                });
            }
        }
    }
    let blocking = gaps
        .iter()
        .any(|gap| gap.missing.iter().any(|kind| kind.blocks_sufficiency()));
    let status = if blocking {
        if budget_exhausted {
            ContextSufficiencyStatus::ProbablySufficient
        } else {
            ContextSufficiencyStatus::Insufficient
        }
    } else if checked_hunks > 0 {
        ContextSufficiencyStatus::Sufficient
    } else {
        // No hunks to check: nothing to verify structurally.
        ContextSufficiencyStatus::ProbablySufficient
    };
    let missing = gaps
        .iter()
        .filter(|gap| gap.missing.iter().any(|kind| kind.blocks_sufficiency()))
        .map(|gap| {
            format!(
                "{}:{}-{} missing {}",
                gap.path,
                gap.start_line,
                gap.end_line,
                gap.missing
                    .iter()
                    .filter(|kind| kind.blocks_sufficiency())
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    ContextSufficiency {
        status,
        missing,
        gaps,
    }
}

/// Test files connected to `path` through `Tests` edges: import-resolved
/// test linkage plus stem-convention edges. The graph owns both sources,
/// so sufficiency never recreates path-stem or test-convention logic.
fn related_test_paths(index: &ContextIndex, path: &RepoPath) -> BTreeSet<RepoPath> {
    index
        .graph
        .file_referencers(path)
        .filter(|edge| edge.kind == ContextEdgeKind::Tests)
        .filter_map(|edge| edge.from_path().cloned())
        .collect()
}

fn suggested_query(
    path: &RepoPath,
    hunk: &ContextRange,
    kind: ContextCoverageGapKind,
) -> serde_json::Value {
    match kind {
        ContextCoverageGapKind::EnclosingDefinition => serde_json::json!({
            "kind": "read_span",
            "arguments": {
                "path": path.display(),
                "startLine": hunk.start_line,
                "endLine": hunk.end_line,
            },
        }),
        ContextCoverageGapKind::Callers => serde_json::json!({
            "kind": "related_symbols",
            "arguments": { "path": path.display() },
        }),
        ContextCoverageGapKind::Tests | ContextCoverageGapKind::NoRelatedTests => {
            serde_json::json!({
                "kind": "related_tests",
                "arguments": { "path": path.display() },
            })
        }
    }
}
