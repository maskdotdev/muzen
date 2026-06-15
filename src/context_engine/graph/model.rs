//! Context Graph model: nodes, typed edges, provenance.
//!
//! Edges store facts; traversal computes value. Every edge has a kind, a
//! confidence (how certain the relationship fact is -- not relevance), a
//! reason, and provenance. One canonical direction per edge kind: the
//! reverse view is a traversal direction via `edges_to`, never a second
//! edge kind. Node identity is snapshot-scoped; there is no cross-snapshot
//! identity promise.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::reviewer_kernel::kernel_types::{stable_id, RepoPath, SnapshotId};

use super::super::ContextRange;

/// Stable node identity within one snapshot. Chunk nodes are identified
/// by what chunking produces -- `(path, range)` -- never by evidence id:
/// the build pipeline is parsed artifacts -> ContextGraph -> evidence
/// projection, so the graph cannot depend on the evidence layer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum ContextNodeId {
    Repo {
        snapshot_id: SnapshotId,
    },
    File {
        path: RepoPath,
    },
    Chunk {
        path: RepoPath,
        range: ContextRange,
    },
    Symbol {
        path: RepoPath,
        name: String,
        range: ContextRange,
    },
}

impl ContextNodeId {
    pub fn path(&self) -> Option<&RepoPath> {
        match self {
            Self::Repo { .. } => None,
            Self::File { path } | Self::Chunk { path, .. } | Self::Symbol { path, .. } => {
                Some(path)
            }
        }
    }

    pub fn range(&self) -> Option<ContextRange> {
        match self {
            Self::Chunk { range, .. } | Self::Symbol { range, .. } => Some(*range),
            _ => None,
        }
    }

    /// Canonical text form: the basis for deterministic edge ids and
    /// debug output.
    pub fn key(&self) -> String {
        match self {
            Self::Repo { snapshot_id } => format!("repo:{}", snapshot_id.0),
            Self::File { path } => format!("file:{}", path.display()),
            Self::Chunk { path, range } => format!(
                "chunk:{}:{}-{}",
                path.display(),
                range.start_line,
                range.end_line
            ),
            Self::Symbol { path, name, range } => format!(
                "symbol:{}:{}:{}-{}",
                path.display(),
                name,
                range.start_line,
                range.end_line
            ),
        }
    }
}

/// Tests and external contracts are chunk or file nodes plus node-kind
/// metadata, not separate id variants.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextNodeKind {
    Repo,
    File,
    Chunk,
    Symbol,
    Test,
    Config,
    RepositoryRule,
    ExternalContract,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextGraphSource {
    SnapshotManifest,
    DiffHunk,
    SyntaxTree,
    ImportResolver,
    IdentifierScan,
    DocumentLink,
    TestConvention,
    GitHistory,
    HostMetadata,
    CrossRepoProvider,
}

/// Explainability is a property of the graph, not a post-hoc string
/// attached by the pack compiler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextGraphProvenance {
    pub source: ContextGraphSource,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<SnapshotId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextNode {
    pub id: ContextNodeId,
    pub kind: ContextNodeKind,
    pub path: Option<RepoPath>,
    pub range: Option<ContextRange>,
    pub label: String,
    pub provenance: ContextGraphProvenance,
}

/// One canonical direction per kind. Reverse kinds (`ImportedBy`,
/// `ReferencedBy`, `TestedBy`) are deliberately absent: storing both
/// directions doubles the surface where construction can go inconsistent
/// and adds no traversal power.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextEdgeKind {
    /// repo -> file, file -> chunk, chunk -> symbol (range overlap).
    Contains,
    /// changed chunk -> repo (the diff lives at snapshot scope).
    EnclosesHunk,
    /// importer file -> defining file.
    Imports,
    /// file -> symbol it defines.
    Defines,
    /// chunk/file -> symbol; identifier-level.
    References,
    /// test chunk/file -> tested file/symbol.
    Tests,
    /// file <-> file; symmetric, stored once with ordered endpoints.
    CoChanged,
    /// file <-> file; symmetric, stored once with ordered endpoints.
    SameModule,
    /// declarative convention rule edge.
    Convention,
    /// config file -> affected target.
    Configures,
    /// manifest-level dependency.
    DependsOn,
    /// doc -> documented artifact.
    Documents,
    /// artifact -> cross-repo contract.
    ExternalContract,
    /// generated artifact -> source.
    GeneratedFrom,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextEdgeId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEdge {
    pub id: ContextEdgeId,
    pub from: ContextNodeId,
    pub to: ContextNodeId,
    pub kind: ContextEdgeKind,
    /// How certain the relationship fact is (a resolved import is ~0.9;
    /// a bare-name fallback match is ~0.5). Not relevance: traversal
    /// value is computed from `(kind, confidence, purpose)` at query
    /// time.
    pub confidence: f32,
    pub reason: String,
    pub provenance: ContextGraphProvenance,
}

impl ContextEdge {
    pub fn from_path(&self) -> Option<&RepoPath> {
        self.from.path()
    }

    pub fn to_path(&self) -> Option<&RepoPath> {
        self.to.path()
    }
}

/// Deterministic edge id from the canonical fact tuple.
pub(crate) fn edge_id(
    from: &ContextNodeId,
    to: &ContextNodeId,
    kind: ContextEdgeKind,
    detail: &str,
) -> ContextEdgeId {
    ContextEdgeId(stable_id(&[
        &from.key(),
        &to.key(),
        &format!("{kind:?}"),
        detail,
    ]))
}

/// Co-change statistics for one file relative to the review's changed
/// files: raw commit count plus recency-decayed weight.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoChangeStat {
    pub count: u32,
    pub weight: f32,
}

/// A deterministic, bounded, explainable graph of review-relevant
/// relationships between repository artifacts, built per snapshot.
#[derive(Debug, Clone)]
pub struct ContextGraph {
    pub snapshot_id: SnapshotId,
    pub(crate) nodes: BTreeMap<ContextNodeId, ContextNode>,
    pub(crate) edges: Vec<ContextEdge>,
    pub(crate) edges_from: BTreeMap<ContextNodeId, Vec<usize>>,
    pub(crate) edges_to: BTreeMap<ContextNodeId, Vec<usize>>,
    /// Cross-file edges indexed by the path each endpoint belongs to,
    /// for file-granularity queries (callers, dependencies, tests).
    pub(crate) file_in_edges: BTreeMap<RepoPath, Vec<usize>>,
    pub(crate) file_out_edges: BTreeMap<RepoPath, Vec<usize>>,
    /// Diff anchors: changed chunk nodes, or the file node when no
    /// chunk covers a changed file.
    pub(crate) changed_anchors: Vec<ContextNodeId>,
    /// All indexed file paths, for same-module sibling lookups.
    pub(crate) file_paths: BTreeSet<RepoPath>,
    /// Aggregate co-change relative to the whole changed set, for rank
    /// signals. Pairwise facts live on `CoChanged` edges.
    pub co_change: BTreeMap<RepoPath, CoChangeStat>,
}

impl ContextGraph {
    pub(crate) fn empty(snapshot_id: SnapshotId) -> Self {
        Self {
            snapshot_id,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            edges_from: BTreeMap::new(),
            edges_to: BTreeMap::new(),
            file_in_edges: BTreeMap::new(),
            file_out_edges: BTreeMap::new(),
            changed_anchors: Vec::new(),
            file_paths: BTreeSet::new(),
            co_change: BTreeMap::new(),
        }
    }

    pub fn node(&self, id: &ContextNodeId) -> Option<&ContextNode> {
        self.nodes.get(id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &ContextNode> {
        self.nodes.values()
    }

    pub fn edges(&self) -> impl Iterator<Item = &ContextEdge> {
        self.edges.iter()
    }

    pub fn edges_from(&self, id: &ContextNodeId) -> impl Iterator<Item = &ContextEdge> {
        self.edges_from
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
    }

    pub fn edges_to(&self, id: &ContextNodeId) -> impl Iterator<Item = &ContextEdge> {
        self.edges_to
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
    }

    /// Diff anchors: changed chunk nodes when chunks exist for the
    /// file, otherwise the changed file node.
    pub fn changed_anchors(&self) -> impl Iterator<Item = &ContextNodeId> {
        self.changed_anchors.iter()
    }

    /// Cross-file edges whose target belongs to `path` (callers, users,
    /// tests of the file or its symbols).
    pub fn file_referencers(&self, path: &RepoPath) -> impl Iterator<Item = &ContextEdge> {
        self.file_in_edges
            .get(path)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
    }

    /// Cross-file edges whose source belongs to `path` (dependencies of
    /// the file or its chunks).
    pub fn file_references(&self, path: &RepoPath) -> impl Iterator<Item = &ContextEdge> {
        self.file_out_edges
            .get(path)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
    }

    pub(crate) fn add_node(&mut self, node: ContextNode) {
        if let ContextNodeId::File { path } = &node.id {
            self.file_paths.insert(path.clone());
        }
        self.nodes.entry(node.id.clone()).or_insert(node);
    }

    pub(crate) fn add_edge(&mut self, edge: ContextEdge) {
        let index = self.edges.len();
        self.edges_from
            .entry(edge.from.clone())
            .or_default()
            .push(index);
        self.edges_to
            .entry(edge.to.clone())
            .or_default()
            .push(index);
        let from_path = edge.from.path().cloned();
        let to_path = edge.to.path().cloned();
        if let (Some(from_path), Some(to_path)) = (&from_path, &to_path) {
            if from_path != to_path {
                self.file_out_edges
                    .entry(from_path.clone())
                    .or_default()
                    .push(index);
                self.file_in_edges
                    .entry(to_path.clone())
                    .or_default()
                    .push(index);
            }
        }
        self.edges.push(edge);
    }
}
