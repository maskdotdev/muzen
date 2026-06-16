//! Context Graph: the Muzen primitive for review-relevant relationships.
//!
//! A `ContextGraph` is a deterministic, bounded, explainable graph of
//! review-relevant relationships between repository artifacts, built per
//! snapshot. The diff is the retrieval anchor: the highest-precision
//! review context is the blast radius of the change.
//!
//! The graph is a candidate generator for review context, not a truth
//! oracle: ranking, budgets, and trust filters discard false edges
//! cheaply, but no downstream stage can recover a missing edge. The
//! quality bar is edge recall with explainable provenance, not
//! compiler-grade edge precision.

mod build;
mod debug;
mod expand;
mod model;

#[cfg(test)]
mod tests;

pub(crate) use build::ContextGraphBuildInput;
pub(crate) use debug::ContextGraphDebugExport;
#[cfg(test)]
pub(crate) use debug::{ContextGraphDebugLimits, GRAPH_DEBUG_SCHEMA_VERSION};
pub(crate) use expand::{
    ContextGraphCandidate, ContextGraphExpansion, ContextGraphExpansionPurpose,
    ContextGraphExpansionRequest, ContextGraphOmission, ContextGraphOmissionReason,
};
pub(crate) use model::{ContextEdgeKind, ContextGraph, ContextNodeId, ContextNodeKind};
#[cfg(test)]
pub(crate) use model::{ContextEdge, ContextEdgeId, ContextGraphSource};
