//! Bounded, deterministic Context Graph expansion.
//!
//! Traversal value is computed from `(edge kind, confidence, purpose)` at
//! query time -- edges store facts, traversal computes value. Expansion
//! returns a graph path for every candidate: the path is the durable
//! explanation ("changed chunk -> defines symbol -> imported by chunk ->
//! test chunk"). Ties break on stable node ids, so candidate order is
//! identical across runs of the same snapshot.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::reviewer_kernel::kernel_types::RepoPath;

use super::super::ContextRelationshipKind;
use super::model::{ContextEdge, ContextEdgeId, ContextEdgeKind, ContextGraph, ContextNodeId};

/// What the expansion is for. Sufficiency may value coverage edges
/// (tests, callers) differently from retrieval ranking without the graph
/// storing two drifting numbers.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextGraphExpansionPurpose {
    Retrieval,
    Sufficiency,
}

#[derive(Debug, Clone)]
pub struct ContextGraphExpansionRequest {
    pub max_hops: usize,
    pub max_candidates_per_anchor: usize,
    /// Edges below this confidence are not traversed; the nodes they
    /// would have reached are recorded as `BelowConfidenceFloor`.
    pub min_confidence: f32,
    pub purpose: ContextGraphExpansionPurpose,
}

/// One step of a traversal path. `forward` is true when the edge was
/// traversed in its canonical direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphPathStep {
    pub edge_id: ContextEdgeId,
    pub kind: ContextEdgeKind,
    pub forward: bool,
    pub from: ContextNodeId,
    pub to: ContextNodeId,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphPath {
    pub steps: Vec<ContextGraphPathStep>,
}

impl ContextGraphPath {
    /// Human-readable explanation from the relationship-bearing steps
    /// (structural `Contains`/`Defines` glue is elided).
    pub fn describe(&self) -> String {
        let significant: Vec<&str> = self
            .steps
            .iter()
            .filter(|step| !is_structural(step.kind))
            .map(|step| step.reason.as_str())
            .collect();
        if significant.is_empty() {
            self.steps
                .last()
                .map(|step| step.reason.clone())
                .unwrap_or_default()
        } else {
            significant.join("; ")
        }
    }
}

/// One expansion candidate rooted at a changed anchor, with the graph
/// path that justifies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphCandidate {
    pub node_id: ContextNodeId,
    pub anchor: ContextNodeId,
    pub score: f32,
    pub hop_count: u8,
    pub path: ContextGraphPath,
}

impl ContextGraphCandidate {
    pub fn repo_path(&self) -> Option<&RepoPath> {
        self.node_id.path()
    }

    pub fn anchor_path(&self) -> Option<&RepoPath> {
        self.anchor.path()
    }

    pub fn reason(&self) -> String {
        self.path.describe()
    }

    /// The evidence-level relationship this path projects to: the first
    /// relationship-bearing step decides.
    pub fn relationship_kind(&self) -> ContextRelationshipKind {
        for step in &self.path.steps {
            match step.kind {
                ContextEdgeKind::Imports | ContextEdgeKind::References => {
                    return if step.forward {
                        ContextRelationshipKind::Calls
                    } else {
                        ContextRelationshipKind::CalledBy
                    };
                }
                ContextEdgeKind::Tests => return ContextRelationshipKind::Tests,
                ContextEdgeKind::CoChanged => return ContextRelationshipKind::CoChanged,
                ContextEdgeKind::SameModule => return ContextRelationshipKind::SameModule,
                ContextEdgeKind::Configures => return ContextRelationshipKind::Configures,
                ContextEdgeKind::DependsOn => return ContextRelationshipKind::DependsOn,
                ContextEdgeKind::Documents => return ContextRelationshipKind::Documents,
                _ => continue,
            }
        }
        ContextRelationshipKind::CalledBy
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextGraphOmissionReason {
    BudgetExceeded,
    BelowConfidenceFloor,
    NoEvidenceProjection,
    DuplicateLowerScore,
}

impl ContextGraphOmissionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BudgetExceeded => "budget_exceeded",
            Self::BelowConfidenceFloor => "below_confidence_floor",
            Self::NoEvidenceProjection => "no_evidence_projection",
            Self::DuplicateLowerScore => "duplicate_lower_score",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphOmission {
    pub node_id: ContextNodeId,
    pub anchor: Option<ContextNodeId>,
    pub reason: ContextGraphOmissionReason,
}

#[derive(Debug, Clone, Default)]
pub struct ContextGraphExpansion {
    pub candidates: Vec<ContextGraphCandidate>,
    pub omitted: Vec<ContextGraphOmission>,
}

impl ContextGraphExpansion {
    pub fn omitted_counts(&self) -> BTreeMap<ContextGraphOmissionReason, usize> {
        let mut counts = BTreeMap::new();
        for omission in &self.omitted {
            *counts.entry(omission.reason).or_insert(0usize) += 1;
        }
        counts
    }
}

/// Hard cap on traversal states examined per anchor group, independent
/// of request limits: a pathological fan-out cannot make expansion
/// unbounded.
const EXPANSION_STATE_BUDGET: usize = 20_000;

/// Score decay for the second and later hops: nearer context is worth
/// more than transitively reachable context.
const HOP_DECAY: f32 = 0.5;

#[derive(Debug, Clone)]
struct TraversalState {
    score: f32,
    /// Stable node key, the deterministic tie-break.
    key: String,
    hops: u8,
    node: ContextNodeId,
    steps: Vec<ContextGraphPathStep>,
    /// Whether further relationship hops are allowed (lateral
    /// edges like CoChanged/SameModule are terminal).
    extendable: bool,
}

impl PartialEq for TraversalState {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for TraversalState {}

impl PartialOrd for TraversalState {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TraversalState {
    // Max-heap order: higher score first, then stable node key
    // (ascending), then fewer hops.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.key.cmp(&self.key))
            .then_with(|| other.hops.cmp(&self.hops))
    }
}

impl ContextGraph {
    /// Expand bounded candidate sets from the changed anchors. Anchors
    /// in the same file share one candidate budget, so a file with many
    /// changed chunks does not multiply its expansion.
    pub fn expand(&self, request: ContextGraphExpansionRequest) -> ContextGraphExpansion {
        let changed_paths: BTreeSet<&RepoPath> = self
            .changed_anchors
            .iter()
            .filter_map(|anchor| anchor.path())
            .collect();
        // Group anchors by file path, preserving anchor order.
        let mut anchor_groups: Vec<(&RepoPath, Vec<&ContextNodeId>)> = Vec::new();
        for anchor in &self.changed_anchors {
            let Some(path) = anchor.path() else {
                continue;
            };
            match anchor_groups.last_mut() {
                Some((group_path, group)) if *group_path == path => group.push(anchor),
                _ => anchor_groups.push((path, vec![anchor])),
            }
        }
        let mut expansion = ContextGraphExpansion::default();
        for (anchor_path, anchors) in anchor_groups {
            self.expand_anchor_group(
                anchor_path,
                &anchors,
                &changed_paths,
                &request,
                &mut expansion,
            );
        }
        expansion
    }

    fn expand_anchor_group(
        &self,
        anchor_path: &RepoPath,
        anchors: &[&ContextNodeId],
        changed_paths: &BTreeSet<&RepoPath>,
        request: &ContextGraphExpansionRequest,
        expansion: &mut ContextGraphExpansion,
    ) {
        if request.max_hops == 0 {
            return;
        }
        // Best candidate per reached path: (score, specificity, node).
        let mut best_by_path: BTreeMap<RepoPath, ContextGraphCandidate> = BTreeMap::new();
        let mut floor_omissions: BTreeSet<ContextNodeId> = BTreeSet::new();
        let mut states_examined = 0usize;
        for anchor in anchors {
            self.traverse(
                anchor,
                anchor_path,
                changed_paths,
                request,
                &mut best_by_path,
                &mut floor_omissions,
                &mut states_examined,
                expansion,
            );
        }
        for node_id in floor_omissions {
            expansion.omitted.push(ContextGraphOmission {
                node_id,
                anchor: anchors.first().map(|anchor| (*anchor).clone()),
                reason: ContextGraphOmissionReason::BelowConfidenceFloor,
            });
        }
        let mut group_candidates: Vec<ContextGraphCandidate> = best_by_path.into_values().collect();
        group_candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.node_id.key().cmp(&right.node_id.key()))
        });
        if group_candidates.len() > request.max_candidates_per_anchor {
            for dropped in group_candidates.split_off(request.max_candidates_per_anchor) {
                expansion.omitted.push(ContextGraphOmission {
                    node_id: dropped.node_id,
                    anchor: Some(dropped.anchor),
                    reason: ContextGraphOmissionReason::BudgetExceeded,
                });
            }
        }
        expansion.candidates.extend(group_candidates);
    }

    #[allow(clippy::too_many_arguments)]
    fn traverse(
        &self,
        anchor: &ContextNodeId,
        anchor_path: &RepoPath,
        changed_paths: &BTreeSet<&RepoPath>,
        request: &ContextGraphExpansionRequest,
        best_by_path: &mut BTreeMap<RepoPath, ContextGraphCandidate>,
        floor_omissions: &mut BTreeSet<ContextNodeId>,
        states_examined: &mut usize,
        expansion: &mut ContextGraphExpansion,
    ) {
        let mut frontier: std::collections::BinaryHeap<TraversalState> =
            std::collections::BinaryHeap::new();
        frontier.push(TraversalState {
            score: 1.0,
            key: anchor.key(),
            hops: 0,
            node: anchor.clone(),
            steps: Vec::new(),
            extendable: true,
        });
        let mut best_seen: BTreeMap<ContextNodeId, f32> = BTreeMap::new();
        best_seen.insert(anchor.clone(), 1.0);

        while let Some(state) = frontier.pop() {
            *states_examined += 1;
            if *states_examined > EXPANSION_STATE_BUDGET {
                return;
            }
            // Stale entry: a better path to this node was already taken.
            if best_seen
                .get(&state.node)
                .is_some_and(|best| *best > state.score)
            {
                continue;
            }
            // Candidate emission.
            if state.hops >= 1 {
                if let Some(candidate_path) = state.node.path() {
                    let eligible = matches!(
                        state.node,
                        ContextNodeId::File { .. } | ContextNodeId::Chunk { .. }
                    ) && !changed_paths.contains(candidate_path);
                    if eligible {
                        let candidate = ContextGraphCandidate {
                            node_id: state.node.clone(),
                            anchor: anchor.clone(),
                            score: state.score,
                            hop_count: state.hops,
                            path: ContextGraphPath {
                                steps: state.steps.clone(),
                            },
                        };
                        merge_candidate(best_by_path, candidate, expansion);
                    }
                }
            }
            if !state.extendable {
                self.emit_terminal_tests(
                    anchor,
                    anchor_path,
                    changed_paths,
                    request,
                    &state,
                    best_by_path,
                    floor_omissions,
                    expansion,
                );
                continue;
            }
            // Neighbor expansion.
            let outgoing = self.edges_from(&state.node).map(|edge| (edge, true));
            let incoming = self.edges_to(&state.node).map(|edge| (edge, false));
            for (edge, forward) in outgoing.chain(incoming) {
                let neighbor = if forward { &edge.to } else { &edge.from };
                if matches!(neighbor, ContextNodeId::Repo { .. }) {
                    continue;
                }
                if let Some(neighbor_path) = neighbor.path() {
                    // Other changed files are their own anchors; passing
                    // through them only duplicates their expansion.
                    if neighbor_path != anchor_path && changed_paths.contains(neighbor_path) {
                        continue;
                    }
                }
                let Some(step) = step_cost(edge, forward, state.hops, request.purpose) else {
                    continue;
                };
                if !is_structural(edge.kind) && edge.confidence < request.min_confidence {
                    floor_omissions.insert(neighbor.clone());
                    continue;
                }
                let new_hops = state.hops + step.hop_cost;
                if usize::from(new_hops) > request.max_hops {
                    continue;
                }
                let mut new_score = state.score * step.value;
                if step.hop_cost > 0 && state.hops >= 1 {
                    new_score *= HOP_DECAY;
                }
                if best_seen
                    .get(neighbor)
                    .is_some_and(|best| *best >= new_score)
                {
                    continue;
                }
                best_seen.insert(neighbor.clone(), new_score);
                let mut steps = state.steps.clone();
                steps.push(ContextGraphPathStep {
                    edge_id: edge.id.clone(),
                    kind: edge.kind,
                    forward,
                    from: state.node.clone(),
                    to: neighbor.clone(),
                    reason: edge.reason.clone(),
                });
                frontier.push(TraversalState {
                    score: new_score,
                    key: neighbor.key(),
                    hops: new_hops,
                    node: neighbor.clone(),
                    steps,
                    extendable: step.extendable,
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_terminal_tests(
        &self,
        anchor: &ContextNodeId,
        anchor_path: &RepoPath,
        changed_paths: &BTreeSet<&RepoPath>,
        request: &ContextGraphExpansionRequest,
        state: &TraversalState,
        best_by_path: &mut BTreeMap<RepoPath, ContextGraphCandidate>,
        floor_omissions: &mut BTreeSet<ContextNodeId>,
        expansion: &mut ContextGraphExpansion,
    ) {
        if usize::from(state.hops) >= request.max_hops {
            return;
        }
        let new_hops = state.hops + 1;
        for edge in self
            .edges_to(&state.node)
            .filter(|edge| edge.kind == ContextEdgeKind::Tests)
        {
            let neighbor = &edge.from;
            if matches!(neighbor, ContextNodeId::Repo { .. }) {
                continue;
            }
            if let Some(neighbor_path) = neighbor.path() {
                if neighbor_path != anchor_path && changed_paths.contains(neighbor_path) {
                    continue;
                }
            }
            if !is_structural(edge.kind) && edge.confidence < request.min_confidence {
                floor_omissions.insert(neighbor.clone());
                continue;
            }
            if usize::from(new_hops) > request.max_hops {
                continue;
            }
            let test_value = match request.purpose {
                ContextGraphExpansionPurpose::Retrieval => edge.confidence,
                ContextGraphExpansionPurpose::Sufficiency => edge.confidence.max(0.9),
            };
            let mut steps = state.steps.clone();
            steps.push(ContextGraphPathStep {
                edge_id: edge.id.clone(),
                kind: edge.kind,
                forward: false,
                from: state.node.clone(),
                to: neighbor.clone(),
                reason: edge.reason.clone(),
            });
            if let Some(candidate_path) = neighbor.path() {
                let eligible = matches!(
                    neighbor,
                    ContextNodeId::File { .. } | ContextNodeId::Chunk { .. }
                ) && !changed_paths.contains(candidate_path);
                if eligible {
                    let candidate = ContextGraphCandidate {
                        node_id: neighbor.clone(),
                        anchor: anchor.clone(),
                        score: state.score * test_value * HOP_DECAY,
                        hop_count: new_hops,
                        path: ContextGraphPath { steps },
                    };
                    merge_candidate(best_by_path, candidate, expansion);
                }
            }
        }
    }
}

/// Merge a candidate into the per-path best map. Specificity preference:
/// at equal score, a chunk node beats a file node ("the referencing
/// chunk, not all of its chunks"); remaining ties break on node key.
fn merge_candidate(
    best_by_path: &mut BTreeMap<RepoPath, ContextGraphCandidate>,
    candidate: ContextGraphCandidate,
    expansion: &mut ContextGraphExpansion,
) {
    let Some(path) = candidate.node_id.path().cloned() else {
        return;
    };
    match best_by_path.entry(path) {
        std::collections::btree_map::Entry::Vacant(vacant) => {
            vacant.insert(candidate);
        }
        std::collections::btree_map::Entry::Occupied(mut occupied) => {
            let current = occupied.get();
            let replace = match candidate.score.total_cmp(&current.score) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    let specificity = |node: &ContextNodeId| match node {
                        ContextNodeId::Chunk { .. } => 1u8,
                        _ => 0,
                    };
                    match specificity(&candidate.node_id).cmp(&specificity(&current.node_id)) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Less => false,
                        std::cmp::Ordering::Equal => {
                            candidate.node_id.key() < current.node_id.key()
                        }
                    }
                }
            };
            if replace {
                let dropped = occupied.insert(candidate);
                expansion.omitted.push(ContextGraphOmission {
                    node_id: dropped.node_id,
                    anchor: Some(dropped.anchor),
                    reason: ContextGraphOmissionReason::DuplicateLowerScore,
                });
            } else {
                expansion.omitted.push(ContextGraphOmission {
                    node_id: candidate.node_id,
                    anchor: Some(candidate.anchor),
                    reason: ContextGraphOmissionReason::DuplicateLowerScore,
                });
            }
        }
    }
}

pub(crate) fn is_structural(kind: ContextEdgeKind) -> bool {
    matches!(kind, ContextEdgeKind::Contains | ContextEdgeKind::Defines)
}

struct StepCost {
    value: f32,
    hop_cost: u8,
    /// Whether traversal may continue past the reached node.
    extendable: bool,
}

/// Traversal value from `(kind, direction, confidence, purpose)`.
/// Returns `None` when the edge is not traversable here.
fn step_cost(
    edge: &ContextEdge,
    forward: bool,
    hops_so_far: u8,
    purpose: ContextGraphExpansionPurpose,
) -> Option<StepCost> {
    match edge.kind {
        ContextEdgeKind::Contains | ContextEdgeKind::Defines => Some(StepCost {
            value: 1.0,
            hop_cost: 0,
            extendable: true,
        }),
        ContextEdgeKind::EnclosesHunk => None,
        ContextEdgeKind::Imports | ContextEdgeKind::References => Some(StepCost {
            // Callers of changed code (reverse) carry slightly more
            // review value than dependencies of changed code (forward).
            value: edge.confidence * if forward { 0.95 } else { 1.0 },
            hop_cost: 1,
            extendable: true,
        }),
        ContextEdgeKind::Tests => Some(StepCost {
            value: match purpose {
                ContextGraphExpansionPurpose::Retrieval => edge.confidence,
                // Coverage questions value test linkage at full strength
                // regardless of how the link was derived.
                ContextGraphExpansionPurpose::Sufficiency => edge.confidence.max(0.9),
            },
            hop_cost: 1,
            extendable: true,
        }),
        ContextEdgeKind::CoChanged | ContextEdgeKind::SameModule => {
            // Lateral neighborhood edges are first-hop only and
            // terminal: chaining them dilutes the change anchor.
            if hops_so_far > 0 {
                return None;
            }
            Some(StepCost {
                value: edge.confidence,
                hop_cost: 1,
                extendable: false,
            })
        }
        ContextEdgeKind::Convention
        | ContextEdgeKind::Configures
        | ContextEdgeKind::DependsOn
        | ContextEdgeKind::Documents
        | ContextEdgeKind::ExternalContract
        | ContextEdgeKind::GeneratedFrom => Some(StepCost {
            value: edge.confidence,
            hop_cost: 1,
            extendable: true,
        }),
    }
}
