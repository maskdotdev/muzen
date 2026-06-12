//! Opt-in Context Graph debug export (G7).
//!
//! Bench and debug runs only -- never part of the default
//! `ContextIndexReport.artifacts`. The export is deterministic (stable
//! node and edge ordering) and bounded (hard caps with truncation
//! counters), so a bench failure can attribute a missed artifact to a
//! missing edge, a traversal omission, or a ranking drop -- and a
//! future UI can render graph paths without reverse-engineering pack
//! data.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::ContextRange;
use super::expand::{ContextGraphExpansion, ContextGraphOmissionReason};
use super::model::{ContextEdgeKind, ContextGraph, ContextGraphSource, ContextNodeKind};

pub const GRAPH_DEBUG_SCHEMA_VERSION: &str = "muzen.context-graph-debug.v1";

/// Hard caps independent of graph size: a pathological repository
/// cannot make the export unbounded. Truncation is visible in the
/// `truncated*` counters; aggregate counts always cover the full graph.
#[derive(Debug, Clone, Copy)]
pub struct ContextGraphDebugLimits {
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_candidates: usize,
    pub max_omissions: usize,
}

impl Default for ContextGraphDebugLimits {
    fn default() -> Self {
        Self {
            max_nodes: 100_000,
            max_edges: 200_000,
            max_candidates: 10_000,
            max_omissions: 20_000,
        }
    }
}

/// Node identity is exported as the canonical node key
/// (`file:path`, `chunk:path:start-end`, `symbol:path:name:start-end`):
/// compact, deterministic, and directly joinable against edge endpoints
/// and expansion results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphDebugNode {
    pub key: String,
    pub kind: ContextNodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ContextRange>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphDebugEdge {
    pub from: String,
    pub to: String,
    pub kind: ContextEdgeKind,
    pub confidence: f32,
    pub reason: String,
    pub source: ContextGraphSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphDebugPathStep {
    pub kind: ContextEdgeKind,
    pub forward: bool,
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// One expansion candidate in expansion (score-ranked) order, with the
/// graph path that justifies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphDebugCandidate {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub anchor: String,
    pub score: f32,
    pub hop_count: u8,
    pub steps: Vec<ContextGraphDebugPathStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphDebugOmission {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    pub reason: ContextGraphOmissionReason,
}

/// Confidence distribution for one edge kind, computed over every edge
/// in the graph (never the truncated export subset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphConfidenceSummary {
    pub count: usize,
    pub min: f32,
    pub max: f32,
    pub mean: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphDebugExport {
    pub schema_version: String,
    pub snapshot_id: String,
    /// Full-graph counts, independent of export truncation.
    pub node_count: usize,
    pub edge_count: usize,
    pub changed_anchors: Vec<String>,
    pub nodes: Vec<ContextGraphDebugNode>,
    pub truncated_nodes: usize,
    pub edges: Vec<ContextGraphDebugEdge>,
    pub truncated_edges: usize,
    pub candidates: Vec<ContextGraphDebugCandidate>,
    pub truncated_candidates: usize,
    pub omitted: Vec<ContextGraphDebugOmission>,
    pub truncated_omissions: usize,
    pub omitted_counts_by_reason: BTreeMap<ContextGraphOmissionReason, usize>,
    pub edge_confidence_by_kind: BTreeMap<ContextEdgeKind, ContextGraphConfidenceSummary>,
}

impl ContextGraphDebugExport {
    pub fn collect(graph: &ContextGraph, expansion: &ContextGraphExpansion) -> Self {
        Self::collect_bounded(graph, expansion, ContextGraphDebugLimits::default())
    }

    pub fn collect_bounded(
        graph: &ContextGraph,
        expansion: &ContextGraphExpansion,
        limits: ContextGraphDebugLimits,
    ) -> Self {
        // Nodes iterate in BTreeMap (node id) order: deterministic.
        let nodes: Vec<ContextGraphDebugNode> = graph
            .nodes()
            .take(limits.max_nodes)
            .map(|node| ContextGraphDebugNode {
                key: node.id.key(),
                kind: node.kind,
                path: node.path.as_ref().map(|path| path.display()),
                range: node.range,
                label: node.label.clone(),
            })
            .collect();

        // Edges sort by the canonical fact tuple so the export does not
        // depend on construction insertion order.
        let mut all_edges: Vec<&super::model::ContextEdge> = graph.edges().collect();
        all_edges.sort_by(|left, right| {
            left.from
                .key()
                .cmp(&right.from.key())
                .then_with(|| left.to.key().cmp(&right.to.key()))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.id.cmp(&right.id))
        });
        let edges: Vec<ContextGraphDebugEdge> = all_edges
            .iter()
            .take(limits.max_edges)
            .map(|edge| ContextGraphDebugEdge {
                from: edge.from.key(),
                to: edge.to.key(),
                kind: edge.kind,
                confidence: edge.confidence,
                reason: edge.reason.clone(),
                source: edge.provenance.source,
            })
            .collect();

        // Candidates keep expansion order: that order is the graph's
        // own ranking and is deterministic by construction.
        let candidates: Vec<ContextGraphDebugCandidate> = expansion
            .candidates
            .iter()
            .take(limits.max_candidates)
            .map(|candidate| ContextGraphDebugCandidate {
                node: candidate.node_id.key(),
                path: candidate.repo_path().map(|path| path.display()),
                anchor: candidate.anchor.key(),
                score: candidate.score,
                hop_count: candidate.hop_count,
                steps: candidate
                    .path
                    .steps
                    .iter()
                    .map(|step| ContextGraphDebugPathStep {
                        kind: step.kind,
                        forward: step.forward,
                        from: step.from.key(),
                        to: step.to.key(),
                        reason: step.reason.clone(),
                    })
                    .collect(),
            })
            .collect();

        let omitted: Vec<ContextGraphDebugOmission> = expansion
            .omitted
            .iter()
            .take(limits.max_omissions)
            .map(|omission| ContextGraphDebugOmission {
                node: omission.node_id.key(),
                path: omission.node_id.path().map(|path| path.display()),
                anchor: omission.anchor.as_ref().map(|anchor| anchor.key()),
                reason: omission.reason,
            })
            .collect();

        // Aggregates always cover the full graph and expansion, never
        // the truncated subsets.
        let mut accumulators: BTreeMap<ContextEdgeKind, (usize, f32, f32, f64)> = BTreeMap::new();
        for edge in graph.edges() {
            let (count, min, max, sum) =
                accumulators
                    .entry(edge.kind)
                    .or_insert((0usize, f32::MAX, f32::MIN, 0.0f64));
            *count += 1;
            *min = min.min(edge.confidence);
            *max = max.max(edge.confidence);
            *sum += f64::from(edge.confidence);
        }
        let edge_confidence_by_kind: BTreeMap<ContextEdgeKind, ContextGraphConfidenceSummary> =
            accumulators
                .into_iter()
                .map(|(kind, (count, min, max, sum))| {
                    let mean = (sum / count as f64) as f32;
                    (
                        kind,
                        ContextGraphConfidenceSummary {
                            count,
                            min,
                            max,
                            // f32 rounding must not push the mean outside
                            // the observed range.
                            mean: mean.clamp(min, max),
                        },
                    )
                })
                .collect();

        let node_count = graph.nodes().count();
        let edge_count = graph.edges().count();
        Self {
            schema_version: GRAPH_DEBUG_SCHEMA_VERSION.to_string(),
            snapshot_id: graph.snapshot_id.0.clone(),
            node_count,
            edge_count,
            changed_anchors: graph
                .changed_anchors()
                .map(|anchor| anchor.key())
                .collect(),
            truncated_nodes: node_count.saturating_sub(nodes.len()),
            nodes,
            truncated_edges: edge_count.saturating_sub(edges.len()),
            edges,
            truncated_candidates: expansion.candidates.len().saturating_sub(candidates.len()),
            candidates,
            truncated_omissions: expansion.omitted.len().saturating_sub(omitted.len()),
            omitted,
            omitted_counts_by_reason: expansion.omitted_counts(),
            edge_confidence_by_kind,
        }
    }
}
