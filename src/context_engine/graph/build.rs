//! Context Graph construction.
//!
//! Deterministic from the snapshot, derived artifacts, and bounded git
//! history. Every resolver is an edge source: retrieval, ranking, and
//! sufficiency consume graph facts and contain no resolver logic.
//! Resolver failures only affect edges from that source; unresolvable
//! specifiers degrade to bounded low-confidence identifier-scan edges.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::runtime::contracts::{RepoPath, SnapshotId};

use super::super::chunking::{range_overlaps, FileChunk};
use super::super::syntax::ParsedSymbols;
use super::super::ContextRange;
use super::model::{
    edge_id, CoChangeStat, ContextEdge, ContextEdgeKind, ContextGraph, ContextGraphProvenance,
    ContextGraphSource, ContextNode, ContextNodeId, ContextNodeKind,
};

/// Cap on bare-name fallback fan-out: a common name defined in many files
/// would otherwise create noise edges.
const NAME_FALLBACK_MAX_DEFINERS: usize = 4;
const FEATURE_SLICE_MAX_SIBLINGS: usize = 48;
const DOCUMENT_LINK_CONFIDENCE: f32 = 0.85;
const DOCUMENT_LINK_MAX_TARGETS_PER_DOC: usize = 64;
const NEXT_APP_LAYOUT_CONFIDENCE: f32 = 0.75;
const NEXT_APP_MAX_LAYOUT_EDGES_PER_CHANGED: usize = 4;
const NEXT_APP_ROUTE_PARAM_CONFIDENCE: f32 = 0.62;
const NEXT_APP_MAX_ROUTE_PARAM_EDGES_PER_CHANGED: usize = 32;
const PACKAGE_DECLARATION_CONFIDENCE: f32 = 0.85;

/// Cap on chunk-level `References` edges per (importer, symbol): the
/// earliest referencing chunks carry the relationship; more add noise.
const MAX_REFERENCING_CHUNKS_PER_SYMBOL: usize = 4;

/// Everything the graph build consumes. All inputs are deterministic
/// snapshot artifacts; no network, no model calls.
pub struct ContextGraphBuildInput<'a> {
    pub snapshot_id: SnapshotId,
    /// Checkout root for the bounded git co-change walk.
    pub repo_root: &'a Path,
    pub parsed_by_file: &'a BTreeMap<RepoPath, ParsedSymbols>,
    /// Resolver configuration sources (`tsconfig.json`, `jsconfig.json`).
    pub file_contents: &'a BTreeMap<RepoPath, String>,
    pub chunks_by_file: BTreeMap<RepoPath, &'a [FileChunk]>,
    /// File node kinds (Test/Config/RepositoryRule); absent paths are
    /// plain `File` nodes.
    pub node_kind_by_file: BTreeMap<RepoPath, ContextNodeKind>,
    /// Diff hunk ranges by changed file path (new-side line spans).
    pub hunk_ranges: &'a BTreeMap<String, Vec<ContextRange>>,
    pub changed_paths: &'a BTreeSet<RepoPath>,
    pub co_change_commit_limit: usize,
}

impl ContextGraph {
    pub fn build(input: ContextGraphBuildInput) -> ContextGraph {
        let mut graph = ContextGraph::empty(input.snapshot_id.clone());
        let snapshot_id = input.snapshot_id.clone();
        let provenance = |source: ContextGraphSource, detail: String| ContextGraphProvenance {
            source,
            detail,
            snapshot_id: Some(snapshot_id.clone()),
        };

        let repo_id = ContextNodeId::Repo {
            snapshot_id: input.snapshot_id.clone(),
        };
        graph.add_node(ContextNode {
            id: repo_id.clone(),
            kind: ContextNodeKind::Repo,
            path: None,
            range: None,
            label: "repository".to_string(),
            provenance: provenance(ContextGraphSource::SnapshotManifest, String::new()),
        });

        // ---- File, chunk, and symbol nodes with structural edges.
        for (path, parsed) in input.parsed_by_file {
            let file_kind = input
                .node_kind_by_file
                .get(path)
                .copied()
                .unwrap_or(ContextNodeKind::File);
            let file_id = ContextNodeId::File { path: path.clone() };
            graph.add_node(ContextNode {
                id: file_id.clone(),
                kind: file_kind,
                path: Some(path.clone()),
                range: None,
                label: path.display(),
                provenance: provenance(ContextGraphSource::SnapshotManifest, String::new()),
            });
            graph.add_edge(ContextEdge {
                id: edge_id(&repo_id, &file_id, ContextEdgeKind::Contains, ""),
                from: repo_id.clone(),
                to: file_id.clone(),
                kind: ContextEdgeKind::Contains,
                confidence: 1.0,
                reason: format!("repository contains {}", path.display()),
                provenance: provenance(ContextGraphSource::SnapshotManifest, String::new()),
            });
            let chunks = input
                .chunks_by_file
                .get(path)
                .copied()
                .unwrap_or(&[] as &[FileChunk]);
            for chunk in chunks {
                let chunk_id = ContextNodeId::Chunk {
                    path: path.clone(),
                    range: chunk.range(),
                };
                graph.add_node(ContextNode {
                    id: chunk_id.clone(),
                    kind: ContextNodeKind::Chunk,
                    path: Some(path.clone()),
                    range: Some(chunk.range()),
                    label: chunk
                        .symbol_path
                        .clone()
                        .unwrap_or_else(|| format!("{}:{}", path.display(), chunk.start_line)),
                    provenance: provenance(ContextGraphSource::SyntaxTree, chunk.node_kind.clone()),
                });
                graph.add_edge(ContextEdge {
                    id: edge_id(&file_id, &chunk_id, ContextEdgeKind::Contains, ""),
                    from: file_id.clone(),
                    to: chunk_id.clone(),
                    kind: ContextEdgeKind::Contains,
                    confidence: 1.0,
                    reason: format!(
                        "{} contains lines {}-{}",
                        path.display(),
                        chunk.start_line,
                        chunk.end_line
                    ),
                    provenance: provenance(ContextGraphSource::SyntaxTree, String::new()),
                });
            }
            for (name, range) in &parsed.definition_ranges {
                let symbol_id = ContextNodeId::Symbol {
                    path: path.clone(),
                    name: name.clone(),
                    range: *range,
                };
                graph.add_node(ContextNode {
                    id: symbol_id.clone(),
                    kind: ContextNodeKind::Symbol,
                    path: Some(path.clone()),
                    range: Some(*range),
                    label: name.clone(),
                    provenance: provenance(ContextGraphSource::SyntaxTree, String::new()),
                });
                graph.add_edge(ContextEdge {
                    id: edge_id(&file_id, &symbol_id, ContextEdgeKind::Defines, ""),
                    from: file_id.clone(),
                    to: symbol_id.clone(),
                    kind: ContextEdgeKind::Defines,
                    confidence: 1.0,
                    reason: format!("{} defines {}", path.display(), name),
                    provenance: provenance(ContextGraphSource::SyntaxTree, String::new()),
                });
                for chunk in chunks {
                    if range_overlaps(&chunk.range(), range) {
                        let chunk_id = ContextNodeId::Chunk {
                            path: path.clone(),
                            range: chunk.range(),
                        };
                        graph.add_edge(ContextEdge {
                            id: edge_id(&chunk_id, &symbol_id, ContextEdgeKind::Contains, ""),
                            from: chunk_id,
                            to: symbol_id.clone(),
                            kind: ContextEdgeKind::Contains,
                            confidence: 1.0,
                            reason: format!("chunk encloses definition of {name}"),
                            provenance: provenance(ContextGraphSource::SyntaxTree, String::new()),
                        });
                    }
                }
            }
        }

        // ---- Changed anchors: chunk nodes overlapping diff hunks, or
        // the file node when no chunk covers the file.
        for path in input.changed_paths {
            if !input.parsed_by_file.contains_key(path) {
                continue;
            }
            let hunks = input
                .hunk_ranges
                .get(&path.display())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let chunks = input
                .chunks_by_file
                .get(path)
                .copied()
                .unwrap_or(&[] as &[FileChunk]);
            let mut anchored = false;
            for chunk in chunks {
                let touches_hunk = hunks
                    .iter()
                    .any(|hunk| range_overlaps(&chunk.range(), hunk));
                if !touches_hunk {
                    continue;
                }
                let chunk_id = ContextNodeId::Chunk {
                    path: path.clone(),
                    range: chunk.range(),
                };
                graph.add_edge(ContextEdge {
                    id: edge_id(
                        &chunk_id,
                        &repo_id,
                        ContextEdgeKind::EnclosesHunk,
                        &format!("{}-{}", chunk.start_line, chunk.end_line),
                    ),
                    from: chunk_id.clone(),
                    to: repo_id.clone(),
                    kind: ContextEdgeKind::EnclosesHunk,
                    confidence: 1.0,
                    reason: format!(
                        "chunk {}:{}-{} encloses changed lines",
                        path.display(),
                        chunk.start_line,
                        chunk.end_line
                    ),
                    provenance: provenance(
                        ContextGraphSource::DiffHunk,
                        format!("{}:{}-{}", path.display(), chunk.start_line, chunk.end_line),
                    ),
                });
                graph.changed_anchors.push(chunk_id);
                anchored = true;
            }
            if !anchored {
                graph
                    .changed_anchors
                    .push(ContextNodeId::File { path: path.clone() });
            }
        }

        // ---- Import, reference, and test edges from resolvers.
        build_reference_edges(&mut graph, &input, &provenance);
        build_package_artifact_edges(&mut graph, &input, &provenance);
        build_document_link_edges(&mut graph, &input, &provenance);
        build_next_app_layout_edges(&mut graph, &input, &provenance);
        build_next_app_route_param_edges(&mut graph, &input, &provenance);

        // ---- Same-module siblings of changed files, stored once with
        // ordered endpoints.
        let mut same_module_pairs: BTreeSet<(RepoPath, RepoPath)> = BTreeSet::new();
        for path in input.changed_paths {
            if !graph.file_paths.contains(path) {
                continue;
            }
            for sibling in same_module_siblings(&graph.file_paths, path) {
                if input.changed_paths.contains(&sibling) {
                    continue;
                }
                let pair = if *path < sibling {
                    (path.clone(), sibling)
                } else {
                    (sibling, path.clone())
                };
                same_module_pairs.insert(pair);
            }
        }
        for (left, right) in same_module_pairs {
            let dir = left
                .display()
                .rsplit_once('/')
                .map(|(dir, _)| dir.to_string())
                .unwrap_or_default();
            let from = ContextNodeId::File { path: left.clone() };
            let to = ContextNodeId::File {
                path: right.clone(),
            };
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::SameModule, &dir),
                from,
                to,
                kind: ContextEdgeKind::SameModule,
                confidence: 0.4,
                reason: format!("same module ({})", if dir.is_empty() { "." } else { &dir }),
                provenance: provenance(ContextGraphSource::SnapshotManifest, dir.clone()),
            });
        }
        build_feature_slice_edges(&mut graph, &input, &provenance);

        // ---- Co-change facts from bounded git history.
        let (aggregate, pairs) = co_change_facts(
            input.repo_root,
            input.changed_paths,
            input.co_change_commit_limit,
        );
        graph.co_change = aggregate;
        for ((changed, other), stat) in pairs {
            let (left, right) = if changed < other {
                (changed, other)
            } else {
                (other, changed)
            };
            let from = ContextNodeId::File { path: left };
            let to = ContextNodeId::File { path: right };
            let detail = format!("count={} weight={:.3}", stat.count, stat.weight);
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::CoChanged, &detail),
                from,
                to,
                kind: ContextEdgeKind::CoChanged,
                confidence: (0.3 + stat.weight / 10.0).min(0.8),
                reason: format!("co-changed in {} past commits", stat.count),
                provenance: provenance(ContextGraphSource::GitHistory, detail),
            });
        }

        // ---- Test-convention edges: stem-matching test files for
        // source files, so retrieval and sufficiency never recreate stem logic.
        build_test_convention_edges(&mut graph, &provenance);

        graph
    }
}

fn build_package_artifact_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    let packages = PackageExports::from_files(input.file_contents);
    for artifact in packages.declaration_artifacts(&graph.file_paths) {
        if artifact.declaration == artifact.runtime {
            continue;
        }
        let from = ContextNodeId::File {
            path: artifact.declaration.clone(),
        };
        let to = ContextNodeId::File {
            path: artifact.runtime.clone(),
        };
        graph.add_edge(ContextEdge {
            id: edge_id(&from, &to, ContextEdgeKind::GeneratedFrom, &artifact.detail),
            from,
            to,
            kind: ContextEdgeKind::GeneratedFrom,
            confidence: PACKAGE_DECLARATION_CONFIDENCE,
            reason: format!(
                "{} declares types generated from {}",
                artifact.declaration.display(),
                artifact.runtime.display()
            ),
            provenance: provenance(ContextGraphSource::SnapshotManifest, artifact.detail),
        });
    }
}

fn build_document_link_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    for (doc_path, content) in input.file_contents {
        if !is_document_path(&doc_path.display()) || !graph.file_paths.contains(doc_path) {
            continue;
        }
        let from = ContextNodeId::File {
            path: doc_path.clone(),
        };
        for (index, target) in document_link_targets(doc_path, content, &graph.file_paths)
            .into_iter()
            .take(DOCUMENT_LINK_MAX_TARGETS_PER_DOC)
            .enumerate()
        {
            if target == *doc_path {
                continue;
            }
            let to = ContextNodeId::File {
                path: target.clone(),
            };
            let detail = format!("{} link {}", doc_path.display(), index + 1);
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::Documents, &detail),
                from: from.clone(),
                to,
                kind: ContextEdgeKind::Documents,
                confidence: DOCUMENT_LINK_CONFIDENCE,
                reason: format!("{} links to {}", doc_path.display(), target.display()),
                provenance: provenance(ContextGraphSource::DocumentLink, detail),
            });
        }
    }
}

/// Resolve every import statement to typed edges: `Imports`/`Tests` at
/// file level, `References` at chunk-to-symbol level, and bounded
/// low-confidence `References` for bare-name fallback.
fn build_reference_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    let paths: BTreeSet<RepoPath> = input.parsed_by_file.keys().cloned().collect();
    let resolver = ModuleResolver::from_files(input.file_contents);
    let mut definers_by_name: BTreeMap<&str, BTreeSet<&RepoPath>> = BTreeMap::new();
    for (path, parsed) in input.parsed_by_file {
        for definition in &parsed.definitions {
            definers_by_name
                .entry(definition.as_str())
                .or_default()
                .insert(path);
        }
    }

    for (importer, parsed) in input.parsed_by_file {
        let importer_is_test = is_test_path(&importer.display());
        // (target, via-detail) -> (route confidence, imported names), so
        // one file edge per resolved file pair per resolution route.
        let mut resolved_groups: BTreeMap<(RepoPath, String), (f32, BTreeSet<String>)> =
            BTreeMap::new();
        // (definer, name) for bare-name fallback.
        let mut fallback: BTreeSet<(RepoPath, String)> = BTreeSet::new();
        for statement in &parsed.import_statements {
            let Some(module) = statement.module.as_deref() else {
                continue;
            };
            match resolve_module(importer, module, &paths, &resolver) {
                Some(resolved) if resolved.path != *importer => {
                    for declaration in resolver.packages.resolve_declarations(module, &paths) {
                        if declaration.path != *importer && declaration.path != resolved.path {
                            let entry = resolved_groups
                                .entry((declaration.path, declaration.via))
                                .or_insert((0.0, BTreeSet::new()));
                            entry.0 = entry.0.max(declaration.confidence);
                            entry.1.extend(statement.names.iter().cloned());
                        }
                    }
                    // Barrel hop: names the target re-exports rather than
                    // defines connect the importer one hop further to the
                    // defining file, so barrels do not dead-end.
                    if let Some(target_parsed) = input.parsed_by_file.get(&resolved.path) {
                        for name in &statement.names {
                            if target_parsed.definitions.contains(name) {
                                continue;
                            }
                            for hop in barrel_hops(
                                &resolved.path,
                                target_parsed,
                                name,
                                input.parsed_by_file,
                                &paths,
                                &resolver,
                            ) {
                                if hop != *importer && hop != resolved.path {
                                    let entry = resolved_groups
                                        .entry((
                                            hop,
                                            format!(
                                                "re-export via {} ({module})",
                                                resolved.path.display()
                                            ),
                                        ))
                                        .or_insert((0.0, BTreeSet::new()));
                                    entry.0 = entry.0.max(resolved.confidence);
                                    entry.1.insert(name.clone());
                                }
                            }
                        }
                    }
                    let entry = resolved_groups
                        .entry((resolved.path, resolved.via))
                        .or_insert((0.0, BTreeSet::new()));
                    entry.0 = entry.0.max(resolved.confidence);
                    entry.1.extend(statement.names.iter().cloned());
                }
                Some(_) => {}
                None => {
                    for name in &statement.names {
                        let Some(definers) = definers_by_name.get(name.as_str()) else {
                            continue;
                        };
                        if definers.len() > NAME_FALLBACK_MAX_DEFINERS {
                            continue;
                        }
                        for definer in definers {
                            if *definer != importer {
                                fallback.insert(((*definer).clone(), name.clone()));
                            }
                        }
                    }
                }
            }
        }

        let importer_id = ContextNodeId::File {
            path: importer.clone(),
        };
        let importer_chunks = input
            .chunks_by_file
            .get(importer)
            .copied()
            .unwrap_or(&[] as &[FileChunk]);
        for ((target, via), (route_confidence, names)) in resolved_groups {
            let target_id = ContextNodeId::File {
                path: target.clone(),
            };
            let kind = if importer_is_test {
                ContextEdgeKind::Tests
            } else {
                ContextEdgeKind::Imports
            };
            let names_text = names.iter().cloned().collect::<Vec<_>>().join(", ");
            graph.add_edge(ContextEdge {
                id: edge_id(&importer_id, &target_id, kind, &via),
                from: importer_id.clone(),
                to: target_id.clone(),
                kind,
                confidence: route_confidence,
                reason: format!(
                    "{} imports {} from {} ({via})",
                    importer.display(),
                    if names_text.is_empty() {
                        "module".to_string()
                    } else {
                        names_text.clone()
                    },
                    target.display()
                ),
                provenance: provenance(ContextGraphSource::ImportResolver, via.clone()),
            });
            // Identifier-level references: the chunks of the importer
            // that mention the symbol, narrowed so a large file
            // contributes its referencing chunk rather than all chunks.
            let Some(target_parsed) = input.parsed_by_file.get(&target) else {
                continue;
            };
            for name in &names {
                let Some(range) = target_parsed.definition_ranges.get(name) else {
                    continue;
                };
                let symbol_id = ContextNodeId::Symbol {
                    path: target.clone(),
                    name: name.clone(),
                    range: *range,
                };
                emit_reference_edges(
                    graph,
                    &importer_id,
                    importer,
                    importer_chunks,
                    &symbol_id,
                    name,
                    &target,
                    route_confidence,
                    provenance(ContextGraphSource::IdentifierScan, via.clone()),
                );
            }
        }

        for (definer, name) in fallback {
            let Some(definer_parsed) = input.parsed_by_file.get(&definer) else {
                continue;
            };
            let detail = format!("bare-name match '{name}'");
            match definer_parsed.definition_ranges.get(&name) {
                Some(range) => {
                    let symbol_id = ContextNodeId::Symbol {
                        path: definer.clone(),
                        name: name.clone(),
                        range: *range,
                    };
                    emit_reference_edges(
                        graph,
                        &importer_id,
                        importer,
                        importer_chunks,
                        &symbol_id,
                        &name,
                        &definer,
                        0.5,
                        provenance(ContextGraphSource::IdentifierScan, detail),
                    );
                }
                None => {
                    let definer_id = ContextNodeId::File {
                        path: definer.clone(),
                    };
                    graph.add_edge(ContextEdge {
                        id: edge_id(
                            &importer_id,
                            &definer_id,
                            ContextEdgeKind::References,
                            &detail,
                        ),
                        from: importer_id.clone(),
                        to: definer_id,
                        kind: ContextEdgeKind::References,
                        confidence: 0.5,
                        reason: format!(
                            "{} references {name} defined in {}",
                            importer.display(),
                            definer.display()
                        ),
                        provenance: provenance(ContextGraphSource::IdentifierScan, detail),
                    });
                }
            }
        }
    }
}

/// `References` edges from the importer's mentioning chunks (or its file
/// node when unchunked) to the defining symbol.
#[allow(clippy::too_many_arguments)]
fn emit_reference_edges(
    graph: &mut ContextGraph,
    importer_id: &ContextNodeId,
    importer: &RepoPath,
    importer_chunks: &[FileChunk],
    symbol_id: &ContextNodeId,
    name: &str,
    target: &RepoPath,
    confidence: f32,
    provenance: ContextGraphProvenance,
) {
    let mut emitted = 0usize;
    for chunk in importer_chunks {
        if emitted >= MAX_REFERENCING_CHUNKS_PER_SYMBOL {
            break;
        }
        if !text_word_contains(&chunk.text, name) {
            continue;
        }
        let chunk_id = ContextNodeId::Chunk {
            path: importer.clone(),
            range: chunk.range(),
        };
        graph.add_edge(ContextEdge {
            id: edge_id(
                &chunk_id,
                symbol_id,
                ContextEdgeKind::References,
                &provenance.detail,
            ),
            from: chunk_id,
            to: symbol_id.clone(),
            kind: ContextEdgeKind::References,
            confidence,
            reason: format!(
                "{}:{}-{} references {name} defined in {}",
                importer.display(),
                chunk.start_line,
                chunk.end_line,
                target.display()
            ),
            provenance: provenance.clone(),
        });
        emitted += 1;
    }
    if emitted == 0 {
        graph.add_edge(ContextEdge {
            id: edge_id(
                importer_id,
                symbol_id,
                ContextEdgeKind::References,
                &provenance.detail,
            ),
            from: importer_id.clone(),
            to: symbol_id.clone(),
            kind: ContextEdgeKind::References,
            confidence,
            reason: format!(
                "{} references {name} defined in {}",
                importer.display(),
                target.display()
            ),
            provenance,
        });
    }
}

/// Stem-convention test edges: `foo.test.ts`, `foo_test.rs`,
/// `test_foo.py` style names test `foo.*`. Matching is exact on the
/// normalized stem -- substring matching would flood a repository full
/// of `index.ts` or `route.ts` files with false test edges. A match
/// anywhere in the tree is accepted only when unique; otherwise the test
/// must live near the source file.
fn build_test_convention_edges(
    graph: &mut ContextGraph,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    let mut tests_by_stem: BTreeMap<String, Vec<RepoPath>> = BTreeMap::new();
    for path in graph
        .file_paths
        .iter()
        .filter(|path| is_test_path(&path.display()))
    {
        let stem = normalized_test_stem(&path.display());
        if !stem.is_empty() {
            tests_by_stem.entry(stem).or_default().push(path.clone());
        }
    }
    let source_paths: Vec<RepoPath> = graph.file_paths.iter().cloned().collect();
    for source in source_paths {
        if is_test_path(&source.display()) {
            continue;
        }
        let stem = path_stem(&source.display());
        if stem.is_empty() {
            continue;
        }
        let Some(matches) = tests_by_stem.get(&stem) else {
            continue;
        };
        let unique = matches.len() == 1;
        for test_path in matches {
            if !unique && !is_near(test_path, &source) {
                continue;
            }
            let from = ContextNodeId::File {
                path: test_path.clone(),
            };
            let to = ContextNodeId::File {
                path: source.clone(),
            };
            let already_tested = graph
                .file_referencers(&source)
                .any(|edge| edge.kind == ContextEdgeKind::Tests && edge.from == from);
            if already_tested {
                continue;
            }
            let detail = format!("stem '{stem}'");
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::Tests, &detail),
                from,
                to,
                kind: ContextEdgeKind::Tests,
                confidence: 0.5,
                reason: format!(
                    "{} matches test naming convention for {}",
                    test_path.display(),
                    source.display()
                ),
                provenance: provenance(ContextGraphSource::TestConvention, detail),
            });
        }
    }
}

fn path_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_string()
}

/// Strip test naming affixes: `foo.test` / `foo.spec` / `foo_test` /
/// `foo-test` / `test_foo` -> `foo`. A bare `foo` stays `foo`.
fn normalized_test_stem(path: &str) -> String {
    let mut stem = path_stem(path);
    // `foo.test.ts` -> file_stem `foo.test`.
    for suffix in [".test", ".spec", "_test", "-test", "_spec", "-spec"] {
        if let Some(stripped) = stem.strip_suffix(suffix) {
            stem = stripped.to_string();
            break;
        }
    }
    for prefix in ["test_", "spec_"] {
        if let Some(stripped) = stem.strip_prefix(prefix) {
            stem = stripped.to_string();
            break;
        }
    }
    stem
}

/// Same directory, or the test sits in a test directory directly under
/// the changed file's directory (`__tests__/`, `tests/`, `test/`).
fn is_near(test_path: &RepoPath, changed: &RepoPath) -> bool {
    let dir_of = |path: &RepoPath| {
        path.display()
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default()
    };
    let test_dir = dir_of(test_path);
    let changed_dir = dir_of(changed);
    if test_dir == changed_dir {
        return true;
    }
    for nested in ["__tests__", "tests", "test"] {
        let candidate = if changed_dir.is_empty() {
            nested.to_string()
        } else {
            format!("{changed_dir}/{nested}")
        };
        if test_dir == candidate {
            return true;
        }
    }
    false
}

/// One hop through a barrel/re-export file: statements in `target` that
/// bind `name` (named re-export) or bind nothing (`export * from`,
/// wildcard `use`) resolve to the files that actually provide the name.
fn barrel_hops(
    target: &RepoPath,
    target_parsed: &ParsedSymbols,
    name: &str,
    parsed_by_file: &BTreeMap<RepoPath, ParsedSymbols>,
    paths: &BTreeSet<RepoPath>,
    resolver: &ModuleResolver,
) -> Vec<RepoPath> {
    let mut hops = Vec::new();
    for statement in &target_parsed.import_statements {
        let binds_name = statement.names.iter().any(|bound| bound == name);
        let wildcard = statement.names.is_empty();
        if !binds_name && !wildcard {
            continue;
        }
        let Some(module) = statement.module.as_deref() else {
            continue;
        };
        let Some(resolved) = resolve_module(target, module, paths, resolver) else {
            continue;
        };
        if wildcard {
            // A wildcard re-export only justifies the hop when the
            // resolved file actually defines the name.
            let defines = parsed_by_file
                .get(&resolved.path)
                .is_some_and(|parsed| parsed.definitions.iter().any(|d| d == name));
            if !defines {
                continue;
            }
        }
        hops.push(resolved.path);
    }
    hops.sort();
    hops.dedup();
    hops
}

/// Identifier occurrence with word boundaries: `getDb` matches `getDb(`
/// but not `forgetDbCache`.
fn text_word_contains(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let mut start = 0usize;
    while let Some(found) = text[start..].find(name) {
        let begin = start + found;
        let end = begin + name.len();
        let before_ok = begin == 0 || !is_identifier_byte(bytes[begin - 1]);
        let after_ok = end >= bytes.len() || !is_identifier_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        start = begin + name.len();
    }
    false
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

pub(crate) fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("test") || lower.contains("spec") || lower.contains("__tests__")
}

fn is_document_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx") || lower.ends_with(".rst")
}

fn document_link_targets(
    doc_path: &RepoPath,
    content: &str,
    paths: &BTreeSet<RepoPath>,
) -> Vec<RepoPath> {
    let mut targets = BTreeSet::new();
    let mut raw_targets = Vec::new();
    collect_markdown_inline_targets(content, &mut raw_targets);
    collect_markdown_reference_targets(content, &mut raw_targets);
    collect_angle_targets(content, &mut raw_targets);
    collect_rst_hyperlink_targets(content, &mut raw_targets);
    for raw in raw_targets {
        if let Some(target) = document_link_target(&raw)
            .and_then(|target| resolve_document_link(doc_path, &target, paths))
        {
            targets.insert(target);
        }
    }
    targets.into_iter().collect()
}

fn collect_markdown_inline_targets(content: &str, raw_targets: &mut Vec<String>) {
    let mut rest = content;
    while let Some(start) = rest.find("](") {
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find(')') else {
            break;
        };
        raw_targets.push(after_open[..end].to_string());
        rest = &after_open[end + 1..];
    }
}

fn collect_markdown_reference_targets(content: &str, raw_targets: &mut Vec<String>) {
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('[') {
            continue;
        }
        let Some((_label, target)) = trimmed.split_once("]:") else {
            continue;
        };
        raw_targets.push(target.trim().to_string());
    }
}

fn collect_angle_targets(content: &str, raw_targets: &mut Vec<String>) {
    let mut rest = content;
    while let Some(start) = rest.find('<') {
        let after_open = &rest[start + 1..];
        let Some(end) = after_open.find('>') else {
            break;
        };
        raw_targets.push(after_open[..end].to_string());
        rest = &after_open[end + 1..];
    }
}

fn collect_rst_hyperlink_targets(content: &str, raw_targets: &mut Vec<String>) {
    for line in content.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix(".. ") else {
            continue;
        };
        if !rest.starts_with('_') {
            continue;
        }
        let Some((_label, target)) = rest.split_once(':') else {
            continue;
        };
        raw_targets.push(target.trim().to_string());
    }
}

fn document_link_target(raw: &str) -> Option<String> {
    let token = raw.trim().split_whitespace().next()?;
    let target = token
        .trim_matches('<')
        .trim_matches('>')
        .trim_matches('"')
        .trim_matches('\'');
    let lower = target.to_ascii_lowercase();
    if target.is_empty()
        || target.starts_with('#')
        || target.starts_with("//")
        || lower.contains("://")
        || lower.starts_with("mailto:")
    {
        return None;
    }
    let target = target
        .split_once('#')
        .map(|(path, _)| path)
        .unwrap_or(target)
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(target);
    let target = strip_line_suffix(target);
    (!target.is_empty()).then(|| target.to_string())
}

fn strip_line_suffix(path: &str) -> &str {
    let Some((prefix, suffix)) = path.rsplit_once(':') else {
        return path;
    };
    if suffix.chars().all(|char| char.is_ascii_digit()) {
        prefix
    } else {
        path
    }
}

fn resolve_document_link(
    doc_path: &RepoPath,
    target: &str,
    paths: &BTreeSet<RepoPath>,
) -> Option<RepoPath> {
    if target.starts_with('/') {
        return resolve_absolute_or_suffix_link(target, paths);
    }
    let doc_text = doc_path.display();
    let base_dir = doc_text.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let joined = normalize_join(base_dir, target)?;
    try_paths([joined], paths)
}

fn resolve_absolute_or_suffix_link(target: &str, paths: &BTreeSet<RepoPath>) -> Option<RepoPath> {
    let trimmed = target.trim_start_matches('/');
    if let Some(path) = try_paths([trimmed.to_string()], paths) {
        return Some(path);
    }
    let target = target.replace('\\', "/");
    let mut matches = paths
        .iter()
        .filter(|path| {
            let display = path.display();
            target == display || target.ends_with(&format!("/{display}"))
        })
        .cloned()
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

fn build_next_app_layout_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    for changed in input.changed_paths {
        if !is_next_app_leaf_path(changed) {
            continue;
        }
        let mut emitted = 0usize;
        for layout in next_app_ancestor_layouts(changed, &graph.file_paths) {
            if emitted >= NEXT_APP_MAX_LAYOUT_EDGES_PER_CHANGED {
                break;
            }
            if layout == *changed {
                continue;
            }
            let from = ContextNodeId::File {
                path: layout.clone(),
            };
            let to = ContextNodeId::File {
                path: changed.clone(),
            };
            let detail = format!("next app layout for {}", changed.display());
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::Configures, &detail),
                from,
                to,
                kind: ContextEdgeKind::Configures,
                confidence: NEXT_APP_LAYOUT_CONFIDENCE,
                reason: format!(
                    "{} configures app route {}",
                    layout.display(),
                    changed.display()
                ),
                provenance: provenance(ContextGraphSource::SnapshotManifest, detail),
            });
            emitted += 1;
        }
    }
}

fn build_next_app_route_param_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    for changed in input.changed_paths {
        let route_tokens = next_app_route_param_tokens(changed);
        if route_tokens.is_empty() {
            continue;
        }
        let mut targets = graph
            .file_paths
            .iter()
            .filter(|path| {
                *path != changed
                    && !input.changed_paths.contains(*path)
                    && feature_slice_root(path).is_some()
                    && !path_tokens(path).is_disjoint(&route_tokens)
            })
            .cloned()
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| {
            route_param_target_score(&route_tokens, right)
                .total_cmp(&route_param_target_score(&route_tokens, left))
                .then_with(|| left.display().cmp(&right.display()))
        });
        targets.truncate(NEXT_APP_MAX_ROUTE_PARAM_EDGES_PER_CHANGED);
        for target in targets {
            let from = ContextNodeId::File {
                path: target.clone(),
            };
            let to = ContextNodeId::File {
                path: changed.clone(),
            };
            let detail = format!(
                "next app route params [{}] for {}",
                route_tokens.iter().cloned().collect::<Vec<_>>().join(", "),
                changed.display()
            );
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::Convention, &detail),
                from,
                to,
                kind: ContextEdgeKind::Convention,
                confidence: NEXT_APP_ROUTE_PARAM_CONFIDENCE,
                reason: format!(
                    "{} shares route parameter domain tokens with {}",
                    target.display(),
                    changed.display()
                ),
                provenance: provenance(ContextGraphSource::SnapshotManifest, detail),
            });
        }
    }
}

fn is_next_app_leaf_path(path: &RepoPath) -> bool {
    let text = path.display();
    let parts = text.split('/').collect::<Vec<_>>();
    if !parts.iter().any(|part| *part == "app") {
        return false;
    }
    let Some(file_name) = parts.last() else {
        return false;
    };
    next_app_leaf_stem(file_name).is_some()
}

fn next_app_leaf_stem(file_name: &str) -> Option<&str> {
    let stem = file_name.split('.').next()?;
    matches!(
        stem,
        "page" | "route" | "layout" | "template" | "loading" | "error" | "not-found" | "default"
    )
    .then_some(stem)
}

fn next_app_ancestor_layouts(changed: &RepoPath, file_paths: &BTreeSet<RepoPath>) -> Vec<RepoPath> {
    let text = changed.display();
    let Some((dir, _file_name)) = text.rsplit_once('/') else {
        return Vec::new();
    };
    let mut dirs = dir.split('/').collect::<Vec<_>>();
    let Some(app_index) = dirs.iter().position(|part| *part == "app") else {
        return Vec::new();
    };
    let mut layouts = Vec::new();
    while dirs.len() > app_index {
        let candidate_dir = dirs.join("/");
        for extension in ["tsx", "jsx", "ts", "js"] {
            let candidate = format!("{candidate_dir}/layout.{extension}");
            if let Ok(path) = RepoPath::parse(&candidate) {
                if file_paths.contains(&path) {
                    layouts.push(path);
                    break;
                }
            }
        }
        dirs.pop();
    }
    layouts
}

fn next_app_route_param_tokens(path: &RepoPath) -> BTreeSet<String> {
    let text = path.display();
    let parts = text.split('/').collect::<Vec<_>>();
    if !parts.iter().any(|part| *part == "app") {
        return BTreeSet::new();
    }
    let mut tokens = BTreeSet::new();
    for part in parts {
        let Some(raw) = part
            .strip_prefix('[')
            .and_then(|part| part.strip_suffix(']'))
        else {
            continue;
        };
        for token in split_domain_token(raw) {
            if !is_common_feature_path_token(&token) {
                tokens.insert(token);
            }
        }
    }
    tokens
}

fn route_param_target_score(route_tokens: &BTreeSet<String>, path: &RepoPath) -> f32 {
    let path_tokens = path_tokens(path);
    let overlap = route_tokens.intersection(&path_tokens).count() as f32;
    overlap / route_tokens.len().max(1) as f32
}

fn split_domain_token(raw: &str) -> Vec<String> {
    let mut normalized = String::new();
    let mut previous_lowercase = false;
    for character in raw.chars() {
        if character == '-' || character == '_' || character == '.' {
            normalized.push(' ');
            previous_lowercase = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_lowercase {
            normalized.push(' ');
        }
        previous_lowercase = character.is_ascii_lowercase() || character.is_ascii_digit();
        normalized.push(character.to_ascii_lowercase());
    }
    normalized
        .split_whitespace()
        .filter(|token| token.len() >= 3)
        .filter(|token| *token != "id")
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------
// Module resolution
// ---------------------------------------------------------------------

/// Confidence for resolutions backed by a declared route: a relative
/// path, a config alias, or an `exports` map entry.
const DECLARED_RESOLUTION_CONFIDENCE: f32 = 0.9;
/// Confidence for resolutions backed only by conventional package
/// layout (`<dir>/<rest>`, `<dir>/src/<rest>`): a guess, not a fact.
const LAYOUT_RESOLUTION_CONFIDENCE: f32 = 0.75;

/// A specifier resolved to a defining file, with the resolution route
/// preserved for provenance.
pub(crate) struct ResolvedModule {
    pub path: RepoPath,
    /// Human-readable route, e.g. `alias '@/*' -> './src/*'`.
    pub via: String,
    /// How trustworthy the route is; heuristic routes score lower so
    /// they rank behind declared ones rather than crowding them out.
    pub confidence: f32,
}

/// All resolver configuration discovered in the snapshot: tsconfig path
/// aliases and workspace package exports.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModuleResolver {
    ts: TsProjectConfig,
    packages: PackageExports,
}

impl ModuleResolver {
    pub(crate) fn from_files(file_contents: &BTreeMap<RepoPath, String>) -> Self {
        Self {
            ts: TsProjectConfig::from_files(file_contents),
            packages: PackageExports::from_files(file_contents),
        }
    }
}

/// Resolve a module specifier to the defining file within the indexed
/// path set. Returns `None` for unresolvable specifiers (external
/// packages, stdlib), which degrade to name matching.
pub(crate) fn resolve_module(
    importer: &RepoPath,
    module: &str,
    paths: &BTreeSet<RepoPath>,
    resolver: &ModuleResolver,
) -> Option<ResolvedModule> {
    if module.starts_with("./") || module.starts_with("../") {
        return resolve_relative_specifier(importer, module, paths).map(|path| ResolvedModule {
            path,
            via: format!("relative '{module}'"),
            confidence: DECLARED_RESOLUTION_CONFIDENCE,
        });
    }
    if module.contains("::") || module == "crate" || module == "self" || module == "super" {
        return resolve_rust_path(importer, module, paths).map(|path| ResolvedModule {
            path,
            via: format!("rust path '{module}'"),
            confidence: DECLARED_RESOLUTION_CONFIDENCE,
        });
    }
    let importer_text = importer.display();
    if importer_text.ends_with(".py") {
        if let Some(stripped) = module.strip_prefix('.') {
            return resolve_python_relative_module(importer, stripped, paths).map(|path| {
                ResolvedModule {
                    path,
                    via: format!("python relative '{module}'"),
                    confidence: DECLARED_RESOLUTION_CONFIDENCE,
                }
            });
        }
        return resolve_python_module(module, paths).map(|path| ResolvedModule {
            path,
            via: format!("python module '{module}'"),
            confidence: DECLARED_RESOLUTION_CONFIDENCE,
        });
    }
    if is_ts_like_path(&importer_text) {
        return resolver
            .ts
            .resolve(importer, module, paths)
            .or_else(|| resolver.packages.resolve(module, paths));
    }
    None
}

fn is_ts_like_path(path: &str) -> bool {
    [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

fn try_paths(
    candidates: impl IntoIterator<Item = String>,
    paths: &BTreeSet<RepoPath>,
) -> Option<RepoPath> {
    for candidate in candidates {
        if let Ok(path) = RepoPath::parse(&candidate) {
            if paths.contains(&path) {
                return Some(path);
            }
        }
    }
    None
}

/// TS/JS relative specifiers: `./api`, `../db/pool`, with implicit
/// extensions and directory indexes.
fn resolve_relative_specifier(
    importer: &RepoPath,
    specifier: &str,
    paths: &BTreeSet<RepoPath>,
) -> Option<RepoPath> {
    let importer_text = importer.display();
    let base_dir = match importer_text.rsplit_once('/') {
        Some((dir, _file)) => dir,
        None => "",
    };
    let joined = normalize_join(base_dir, specifier)?;
    try_paths(ts_file_candidates(&joined), paths)
}

/// Join a repo-relative directory with a (possibly `./`/`../`-prefixed)
/// relative path, normalizing `.` and `..` segments. Returns `None` when
/// `..` escapes the repository root.
fn normalize_join(base_dir: &str, relative: &str) -> Option<String> {
    let mut segments: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

/// File candidates for a TS/JS module path: the path itself, implicit
/// extensions, and directory index files.
fn ts_file_candidates(joined: &str) -> Vec<String> {
    let mut candidates = vec![joined.to_string()];
    for extension in [".ts", ".tsx", ".js", ".jsx", ".mts", ".mjs"] {
        candidates.push(format!("{joined}{extension}"));
    }
    for index in [
        "/index.ts",
        "/index.tsx",
        "/index.js",
        "/index.jsx",
        "/index.mts",
        "/index.mjs",
    ] {
        candidates.push(format!("{joined}{index}"));
    }
    candidates
}

/// Rust `use` paths: `crate::auth::token` -> `src/auth/token.rs` (or
/// `mod.rs`), `super::x` relative to the importer's parent module.
fn resolve_rust_path(
    importer: &RepoPath,
    module: &str,
    paths: &BTreeSet<RepoPath>,
) -> Option<RepoPath> {
    let importer_text = importer.display();
    let importer_dir = importer_text
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let mut segments: Vec<String> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    let mut raw = module.split("::").peekable();
    match raw.peek().copied() {
        Some("crate") => {
            raw.next();
            roots.push("src".to_string());
            roots.push(String::new());
        }
        Some("self") => {
            raw.next();
            roots.push(importer_dir.to_string());
        }
        Some("super") => {
            while raw.peek() == Some(&"super") {
                raw.next();
                // one `super` strips the file, further ones strip dirs
            }
            let parent = importer_dir
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or("");
            roots.push(importer_dir.to_string());
            roots.push(parent.to_string());
        }
        _ => {
            // External crate (`serde::...`) unless the first segment is a
            // local top-level module; try source roots anyway.
            roots.push("src".to_string());
            roots.push(String::new());
            roots.push(importer_dir.to_string());
        }
    }
    segments.extend(raw.map(str::to_string));
    if segments.is_empty() {
        return None;
    }
    let mut candidates = Vec::new();
    for root in &roots {
        // Full module path, then progressively shorter prefixes: a trailing
        // segment may be an item path inside the file, not a file.
        for take in (1..=segments.len()).rev() {
            let joined = if root.is_empty() {
                segments[..take].join("/")
            } else {
                format!("{root}/{}", segments[..take].join("/"))
            };
            candidates.push(format!("{joined}.rs"));
            candidates.push(format!("{joined}/mod.rs"));
        }
    }
    try_paths(candidates, paths)
}

/// Python dotted modules: `auth.tokens` -> `auth/tokens.py` or package
/// `__init__.py`.
fn resolve_python_module(module: &str, paths: &BTreeSet<RepoPath>) -> Option<RepoPath> {
    let joined = module.replace('.', "/");
    try_paths(
        [format!("{joined}.py"), format!("{joined}/__init__.py")],
        paths,
    )
}

/// Python relative imports: `from .tokens import x` resolves next to the
/// importer; each extra leading dot climbs one package level. The
/// leading dot is already stripped: `rest` is `tokens`, `.tokens` (one
/// extra level), or empty (`from . import x`).
fn resolve_python_relative_module(
    importer: &RepoPath,
    rest: &str,
    paths: &BTreeSet<RepoPath>,
) -> Option<RepoPath> {
    let importer_text = importer.display();
    let mut dir = importer_text
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("")
        .to_string();
    let mut remaining = rest;
    while let Some(stripped) = remaining.strip_prefix('.') {
        let (parent, _) = dir.rsplit_once('/').unwrap_or(("", ""));
        dir = parent.to_string();
        remaining = stripped;
    }
    let joined = if remaining.is_empty() {
        dir.clone()
    } else if dir.is_empty() {
        remaining.replace('.', "/")
    } else {
        format!("{dir}/{}", remaining.replace('.', "/"))
    };
    let candidates = if remaining.is_empty() {
        vec![format!("{joined}/__init__.py")]
    } else {
        vec![format!("{joined}.py"), format!("{joined}/__init__.py")]
    };
    try_paths(candidates, paths)
}

// ---------------------------------------------------------------------
// package.json workspace exports
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PackageManifest {
    /// Package directory, repo-relative; `""` is the repository root.
    dir: String,
    exports: Option<serde_json::Value>,
    main: Option<String>,
    types: Option<String>,
}

/// Workspace packages discovered from `package.json` manifests, for
/// resolving `@scope/pkg/subpath` specifiers through `exports` maps.
#[derive(Debug, Clone, Default)]
struct PackageExports {
    packages: BTreeMap<String, PackageManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PackageDeclarationArtifact {
    declaration: RepoPath,
    runtime: RepoPath,
    detail: String,
}

impl PackageExports {
    fn from_files(file_contents: &BTreeMap<RepoPath, String>) -> Self {
        let mut packages = BTreeMap::new();
        for (path, text) in file_contents {
            let path_text = path.display();
            let file_name = path_text
                .rsplit_once('/')
                .map(|(_, name)| name)
                .unwrap_or(&path_text);
            if file_name != "package.json" {
                continue;
            }
            let Some(value) = parse_jsonc(text) else {
                continue;
            };
            let Some(name) = value.get("name").and_then(|name| name.as_str()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let dir = path_text
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or("")
                .to_string();
            // First manifest wins on duplicate names; BTreeMap iteration
            // keeps this deterministic.
            packages
                .entry(name.to_string())
                .or_insert_with(|| PackageManifest {
                    dir,
                    exports: value.get("exports").cloned(),
                    main: value
                        .get("main")
                        .and_then(|main| main.as_str())
                        .map(str::to_string),
                    types: value
                        .get("types")
                        .or_else(|| value.get("typings"))
                        .and_then(|types| types.as_str())
                        .map(str::to_string),
                });
        }
        Self { packages }
    }

    fn declaration_artifacts(
        &self,
        paths: &BTreeSet<RepoPath>,
    ) -> BTreeSet<PackageDeclarationArtifact> {
        let mut artifacts = BTreeSet::new();
        for (name, manifest) in &self.packages {
            if let (Some(types), Some(runtime)) = (
                manifest.types.as_deref(),
                manifest.main.as_deref().or(Some("index")),
            ) {
                self.add_declaration_artifact(
                    paths,
                    &mut artifacts,
                    name,
                    ".",
                    manifest,
                    types,
                    runtime,
                );
            }
            if let Some(exports) = &manifest.exports {
                for (subpath, value) in export_entries(exports) {
                    let Some(types) = condition_string(value, "types") else {
                        continue;
                    };
                    let Some(runtime) = first_runtime_condition(value) else {
                        continue;
                    };
                    self.add_declaration_artifact(
                        paths,
                        &mut artifacts,
                        name,
                        &subpath,
                        manifest,
                        &types,
                        &runtime,
                    );
                }
            }
        }
        artifacts
    }

    #[allow(clippy::too_many_arguments)]
    fn add_declaration_artifact(
        &self,
        paths: &BTreeSet<RepoPath>,
        artifacts: &mut BTreeSet<PackageDeclarationArtifact>,
        package_name: &str,
        subpath: &str,
        manifest: &PackageManifest,
        declaration_target: &str,
        runtime_target: &str,
    ) {
        let Some(declaration_joined) = normalize_join(&manifest.dir, declaration_target) else {
            return;
        };
        let Some(runtime_joined) = normalize_join(&manifest.dir, runtime_target) else {
            return;
        };
        let Some(declaration) = try_paths(declaration_file_candidates(&declaration_joined), paths)
        else {
            return;
        };
        let Some(runtime) = try_paths(ts_file_candidates(&runtime_joined), paths) else {
            return;
        };
        artifacts.insert(PackageDeclarationArtifact {
            declaration,
            runtime,
            detail: format!(
                "package '{package_name}' export '{subpath}' types '{declaration_target}' runtime '{runtime_target}'"
            ),
        });
    }

    fn resolve(&self, specifier: &str, paths: &BTreeSet<RepoPath>) -> Option<ResolvedModule> {
        if self.packages.is_empty() {
            return None;
        }
        let (name, manifest, subpath) = self.match_package(specifier)?;
        if let Some(exports) = &manifest.exports {
            if let Some(target) = resolve_exports_target(exports, &subpath) {
                if let Some(joined) = normalize_join(&manifest.dir, &target) {
                    if let Some(found) = try_paths(ts_file_candidates(&joined), paths) {
                        return Some(ResolvedModule {
                            path: found,
                            via: format!("package '{name}' exports '{subpath}' -> '{target}'"),
                            confidence: DECLARED_RESOLUTION_CONFIDENCE,
                        });
                    }
                }
            }
        }
        // `main` is a declared route for the package root.
        if subpath == "." {
            if let Some(main) = &manifest.main {
                if let Some(joined) = normalize_join(&manifest.dir, main) {
                    if let Some(found) = try_paths(ts_file_candidates(&joined), paths) {
                        return Some(ResolvedModule {
                            path: found,
                            via: format!("package '{name}' main '{main}'"),
                            confidence: DECLARED_RESOLUTION_CONFIDENCE,
                        });
                    }
                }
            }
        }
        // No (matching) exports entry: fall back to conventional package
        // layouts. Recall-oriented; false candidates are cheap, missing
        // edges are not — but they carry heuristic confidence.
        let mut candidates: Vec<String> = Vec::new();
        if subpath == "." {
            for layout in ["index", "src/index"] {
                if let Some(joined) = normalize_join(&manifest.dir, layout) {
                    candidates.extend(ts_file_candidates(&joined));
                }
            }
        } else {
            let rest = subpath.trim_start_matches("./");
            for layout in [rest.to_string(), format!("src/{rest}")] {
                if let Some(joined) = normalize_join(&manifest.dir, &layout) {
                    candidates.extend(ts_file_candidates(&joined));
                }
            }
        }
        try_paths(candidates, paths).map(|path| ResolvedModule {
            path,
            via: format!("package '{name}' layout '{subpath}'"),
            confidence: LAYOUT_RESOLUTION_CONFIDENCE,
        })
    }

    fn resolve_declarations(
        &self,
        specifier: &str,
        paths: &BTreeSet<RepoPath>,
    ) -> Vec<ResolvedModule> {
        let Some((name, manifest, subpath)) = self.match_package(specifier) else {
            return Vec::new();
        };
        let mut modules = Vec::new();
        if subpath == "." {
            if let Some(types) = &manifest.types {
                if let Some(path) = self.resolve_declaration_target(manifest, types, paths) {
                    modules.push(ResolvedModule {
                        path,
                        via: format!("package '{name}' types '{types}'"),
                        confidence: DECLARED_RESOLUTION_CONFIDENCE,
                    });
                }
            }
        }
        if let Some(exports) = &manifest.exports {
            if let Some(value) = export_entries(exports)
                .into_iter()
                .find_map(|(entry, value)| (entry == subpath).then_some(value))
            {
                if let Some(types) = condition_string(value, "types").or_else(|| {
                    first_string_condition(value)
                        .filter(|target| target.ends_with(".d.ts") || target.ends_with(".d.mts"))
                }) {
                    if let Some(path) = self.resolve_declaration_target(manifest, &types, paths) {
                        modules.push(ResolvedModule {
                            path,
                            via: format!("package '{name}' export '{subpath}' types '{types}'"),
                            confidence: DECLARED_RESOLUTION_CONFIDENCE,
                        });
                    }
                }
            }
        }
        modules.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.via.cmp(&right.via))
        });
        modules.dedup_by(|left, right| left.path == right.path && left.via == right.via);
        modules
    }

    fn resolve_declaration_target(
        &self,
        manifest: &PackageManifest,
        target: &str,
        paths: &BTreeSet<RepoPath>,
    ) -> Option<RepoPath> {
        let joined = normalize_join(&manifest.dir, target)?;
        try_paths(declaration_file_candidates(&joined), paths)
    }

    /// Longest package-name prefix: `@argus/contracts/review` matches
    /// package `@argus/contracts` with subpath `./review`.
    fn match_package<'spec>(
        &self,
        specifier: &'spec str,
    ) -> Option<(&'spec str, &PackageManifest, String)> {
        let mut name = specifier;
        loop {
            if let Some(manifest) = self.packages.get(name) {
                let subpath = if name.len() == specifier.len() {
                    ".".to_string()
                } else {
                    format!(".{}", &specifier[name.len()..])
                };
                return Some((name, manifest, subpath));
            }
            match name.rfind('/') {
                Some(position) if position > 0 => name = &name[..position],
                _ => return None,
            }
        }
    }
}

fn declaration_file_candidates(joined: &str) -> Vec<String> {
    vec![
        joined.to_string(),
        format!("{joined}.d.ts"),
        format!("{joined}/index.d.ts"),
    ]
}

fn export_entries(exports: &serde_json::Value) -> Vec<(String, &serde_json::Value)> {
    let mut entries = Vec::new();
    match exports {
        serde_json::Value::String(_) => entries.push((".".to_string(), exports)),
        serde_json::Value::Object(map) if map.keys().all(|key| !key.starts_with('.')) => {
            entries.push((".".to_string(), exports));
        }
        serde_json::Value::Object(map) => {
            for (subpath, value) in map {
                if subpath.starts_with('.') {
                    entries.push((subpath.clone(), value));
                }
            }
        }
        _ => {}
    }
    entries
}

fn condition_string(value: &serde_json::Value, condition: &str) -> Option<String> {
    match value {
        serde_json::Value::String(target) if condition == "default" => Some(target.clone()),
        serde_json::Value::Object(map) => map
            .get(condition)
            .and_then(|nested| first_string_condition(nested)),
        _ => None,
    }
}

fn first_runtime_condition(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(target) => Some(target.clone()),
        serde_json::Value::Object(map) => {
            for condition in ["import", "default", "require", "node", "browser"] {
                if let Some(nested) = map.get(condition) {
                    if let Some(target) = first_string_condition(nested) {
                        return Some(target);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Resolve a subpath through an `exports` value: exact entries first,
/// then wildcard patterns by longest prefix. Conditional objects take
/// the first import-ish condition.
fn resolve_exports_target(exports: &serde_json::Value, subpath: &str) -> Option<String> {
    match exports {
        serde_json::Value::String(target) if subpath == "." => Some(target.clone()),
        serde_json::Value::Object(map) => {
            // A conditions-only object (no "./" keys) describes ".".
            if map.keys().all(|key| !key.starts_with('.')) {
                if subpath == "." {
                    return first_string_condition(exports);
                }
                return None;
            }
            if let Some(value) = map.get(subpath) {
                return first_string_condition(value);
            }
            let mut best: Option<(usize, String)> = None;
            for (pattern, value) in map {
                let Some((prefix, suffix)) = pattern.split_once('*') else {
                    continue;
                };
                if subpath.len() >= prefix.len() + suffix.len()
                    && subpath.starts_with(prefix)
                    && subpath.ends_with(suffix)
                {
                    let captured = &subpath[prefix.len()..subpath.len() - suffix.len()];
                    if let Some(target) = first_string_condition(value) {
                        let resolved = target.replacen('*', captured, 1);
                        if best
                            .as_ref()
                            .is_none_or(|(best_len, _)| prefix.len() > *best_len)
                        {
                            best = Some((prefix.len(), resolved));
                        }
                    }
                }
            }
            best.map(|(_, target)| target)
        }
        _ => None,
    }
}

fn first_string_condition(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(target) => Some(target.clone()),
        serde_json::Value::Object(map) => {
            for condition in ["import", "default", "require", "node", "types"] {
                if let Some(nested) = map.get(condition) {
                    if let Some(target) = first_string_condition(nested) {
                        return Some(target);
                    }
                }
            }
            map.values().find_map(first_string_condition)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------
// tsconfig / jsconfig path aliases
// ---------------------------------------------------------------------

/// `compilerOptions.baseUrl` / `compilerOptions.paths` rules from one
/// `tsconfig.json` or `jsconfig.json`, with `extends` chains flattened.
/// Directories are repo-relative; `""` is the repository root.
#[derive(Debug, Clone, Default, PartialEq)]
struct TsPathRules {
    /// Base directory for `paths` substitution targets: the resolved
    /// `baseUrl` when declared, otherwise the directory of the config
    /// that declared `paths`.
    paths_base_dir: String,
    /// `(pattern, targets)` from `compilerOptions.paths`, declaration
    /// order preserved.
    paths: Vec<(String, Vec<String>)>,
    /// Resolved `baseUrl` directory for bare-specifier fallback; `None`
    /// when the config does not declare `baseUrl`.
    base_url_dir: Option<String>,
}

/// All path-alias rules discovered in the snapshot, keyed by the
/// directory of the declaring `tsconfig.json` / `jsconfig.json`. The
/// nearest enclosing config governs an importer; lookups walk upward so
/// monorepo root configs still apply when a package has none.
#[derive(Debug, Clone, Default)]
pub(crate) struct TsProjectConfig {
    rules_by_dir: BTreeMap<String, TsPathRules>,
}

/// Bound on `extends` chain depth: configs beyond this are ignored
/// rather than recursed into (defends against cycles).
const TSCONFIG_EXTENDS_MAX_DEPTH: usize = 8;

impl TsProjectConfig {
    pub(crate) fn from_files(file_contents: &BTreeMap<RepoPath, String>) -> Self {
        let mut rules_by_dir = BTreeMap::new();
        for path in file_contents.keys() {
            let text = path.display();
            let file_name = text.rsplit_once('/').map(|(_, name)| name).unwrap_or(&text);
            // `tsconfig.base.json` and friends participate only as
            // `extends` targets; they do not anchor a project directory.
            if file_name != "tsconfig.json" && file_name != "jsconfig.json" {
                continue;
            }
            let dir = text.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
            let Some(rules) = ts_rules_for_config(path, file_contents) else {
                continue;
            };
            if rules.paths.is_empty() && rules.base_url_dir.is_none() {
                continue;
            }
            // `tsconfig.json` wins over `jsconfig.json` in the same dir.
            let entry = rules_by_dir.entry(dir.to_string());
            match entry {
                std::collections::btree_map::Entry::Vacant(vacant) => {
                    vacant.insert(rules);
                }
                std::collections::btree_map::Entry::Occupied(mut occupied) => {
                    if file_name == "tsconfig.json" {
                        occupied.insert(rules);
                    }
                }
            }
        }
        Self { rules_by_dir }
    }

    /// Resolve a non-relative specifier for `importer` through the
    /// nearest enclosing config's `paths` aliases, then its `baseUrl`.
    /// Walks toward the root so an unmatched specifier can still hit a
    /// workspace-level config.
    fn resolve(
        &self,
        importer: &RepoPath,
        specifier: &str,
        paths: &BTreeSet<RepoPath>,
    ) -> Option<ResolvedModule> {
        if self.rules_by_dir.is_empty() {
            return None;
        }
        let importer_text = importer.display();
        let mut dir = importer_text
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("")
            .to_string();
        loop {
            if let Some(rules) = self.rules_by_dir.get(&dir) {
                if let Some(found) = rules.resolve(specifier, paths) {
                    return Some(found);
                }
            }
            if dir.is_empty() {
                return None;
            }
            dir = dir
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("")
                .to_string();
        }
    }
}

impl TsPathRules {
    fn resolve(&self, specifier: &str, paths: &BTreeSet<RepoPath>) -> Option<ResolvedModule> {
        // Exact patterns beat wildcard patterns; longer matched prefixes
        // beat shorter ones (TypeScript's pattern-priority rule).
        let mut matched: Vec<(usize, &str, &[String], Option<&str>)> = Vec::new();
        for (pattern, targets) in &self.paths {
            match pattern.split_once('*') {
                None => {
                    if pattern == specifier {
                        matched.push((usize::MAX, pattern, targets, None));
                    }
                }
                Some((prefix, suffix)) => {
                    if specifier.len() >= prefix.len() + suffix.len()
                        && specifier.starts_with(prefix)
                        && specifier.ends_with(suffix)
                    {
                        let captured = &specifier[prefix.len()..specifier.len() - suffix.len()];
                        matched.push((prefix.len(), pattern, targets, Some(captured)));
                    }
                }
            }
        }
        matched.sort_by(|left, right| right.0.cmp(&left.0));
        for (_, pattern, targets, captured) in matched {
            for target in targets {
                let substituted = match captured {
                    Some(captured) => target.replacen('*', captured, 1),
                    None => target.clone(),
                };
                let Some(joined) = normalize_join(&self.paths_base_dir, &substituted) else {
                    continue;
                };
                if let Some(found) = try_paths(ts_file_candidates(&joined), paths) {
                    return Some(ResolvedModule {
                        path: found,
                        via: format!("alias '{pattern}' -> '{target}'"),
                        confidence: DECLARED_RESOLUTION_CONFIDENCE,
                    });
                }
            }
        }
        let base_url_dir = self.base_url_dir.as_deref()?;
        let joined = normalize_join(base_url_dir, specifier)?;
        try_paths(ts_file_candidates(&joined), paths).map(|path| ResolvedModule {
            path,
            via: format!("baseUrl '{base_url_dir}'"),
            confidence: DECLARED_RESOLUTION_CONFIDENCE,
        })
    }
}

/// Compute effective `baseUrl` and `paths` for one config by walking its
/// `extends` chain. Each option keeps the directory of the config that
/// declared it: TypeScript resolves path-like options relative to the
/// file they originate in, and a child's `paths` replaces the parent's
/// entirely (shallow `compilerOptions` merge).
fn ts_rules_for_config(
    config_path: &RepoPath,
    file_contents: &BTreeMap<RepoPath, String>,
) -> Option<TsPathRules> {
    type DeclaredPaths = (String, Vec<(String, Vec<String>)>);
    let mut declared_paths: Option<DeclaredPaths> = None;
    let mut declared_base_url: Option<(String, String)> = None;
    let mut current = Some(config_path.clone());
    let mut depth = 0usize;
    while let Some(path) = current.take() {
        if depth >= TSCONFIG_EXTENDS_MAX_DEPTH {
            break;
        }
        depth += 1;
        let Some(text) = file_contents.get(&path) else {
            break;
        };
        let Some(value) = parse_jsonc(text) else {
            break;
        };
        let path_text = path.display();
        let dir = path_text
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("")
            .to_string();
        if let Some(options) = value.get("compilerOptions") {
            if declared_paths.is_none() {
                if let Some(map) = options.get("paths").and_then(|paths| paths.as_object()) {
                    let mut rules = Vec::new();
                    for (pattern, targets) in map {
                        let targets: Vec<String> = targets
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|target| target.as_str().map(str::to_string))
                            .collect();
                        if !targets.is_empty() {
                            rules.push((pattern.clone(), targets));
                        }
                    }
                    declared_paths = Some((dir.clone(), rules));
                }
            }
            if declared_base_url.is_none() {
                if let Some(base_url) = options.get("baseUrl").and_then(|base| base.as_str()) {
                    declared_base_url = Some((dir.clone(), base_url.to_string()));
                }
            }
        }
        if declared_paths.is_some() && declared_base_url.is_some() {
            break;
        }
        // Follow `extends` (string or first resolvable array entry);
        // non-relative extends (npm packages) are outside the snapshot.
        let extends = value.get("extends");
        let extend_specs: Vec<&str> = match extends {
            Some(serde_json::Value::String(spec)) => vec![spec.as_str()],
            Some(serde_json::Value::Array(specs)) => {
                specs.iter().filter_map(|spec| spec.as_str()).collect()
            }
            _ => Vec::new(),
        };
        for spec in extend_specs {
            if !spec.starts_with("./") && !spec.starts_with("../") {
                continue;
            }
            let Some(joined) = normalize_join(&dir, spec) else {
                continue;
            };
            let candidates = if joined.ends_with(".json") {
                vec![joined.clone()]
            } else {
                vec![format!("{joined}.json"), joined.clone()]
            };
            if let Some(found) = candidates
                .into_iter()
                .filter_map(|candidate| RepoPath::parse(&candidate).ok())
                .find(|candidate| file_contents.contains_key(candidate))
            {
                current = Some(found);
                break;
            }
        }
    }
    let (declared_paths_dir, paths) = match declared_paths {
        Some((dir, rules)) => (Some(dir), rules),
        None => (None, Vec::new()),
    };
    let base_url_dir =
        declared_base_url.and_then(|(dir, base_url)| normalize_join(&dir, &base_url));
    if paths.is_empty() && base_url_dir.is_none() {
        return None;
    }
    let paths_base_dir = base_url_dir
        .clone()
        .or(declared_paths_dir)
        .unwrap_or_default();
    Some(TsPathRules {
        paths_base_dir,
        paths,
        base_url_dir,
    })
}

/// Parse JSON with comments and trailing commas (the `tsconfig.json`
/// dialect). Returns `None` on malformed input; resolver config parse
/// failures degrade to no alias rules, never a build error.
fn parse_jsonc(text: &str) -> Option<serde_json::Value> {
    let stripped = strip_trailing_commas(&strip_jsonc_comments(text));
    serde_json::from_str(&stripped).ok()
}

fn strip_jsonc_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    let mut in_string = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            out.push(byte as char);
            match byte {
                b'\\' if index + 1 < bytes.len() => {
                    out.push(bytes[index + 1] as char);
                    index += 2;
                    continue;
                }
                b'"' => in_string = false,
                _ => {}
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                out.push('"');
                index += 1;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            _ => {
                out.push(byte as char);
                index += 1;
            }
        }
    }
    out
}

fn strip_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    let mut in_string = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            out.push(byte as char);
            match byte {
                b'\\' if index + 1 < bytes.len() => {
                    out.push(bytes[index + 1] as char);
                    index += 2;
                    continue;
                }
                b'"' => in_string = false,
                _ => {}
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            out.push('"');
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut peek = index + 1;
            while peek < bytes.len() && bytes[peek].is_ascii_whitespace() {
                peek += 1;
            }
            if peek < bytes.len() && (bytes[peek] == b'}' || bytes[peek] == b']') {
                index += 1;
                continue;
            }
        }
        out.push(byte as char);
        index += 1;
    }
    out
}

// ---------------------------------------------------------------------
// Same-module siblings and co-change history
// ---------------------------------------------------------------------

fn same_module_siblings(file_paths: &BTreeSet<RepoPath>, anchor: &RepoPath) -> Vec<RepoPath> {
    let anchor_text = anchor.display();
    let anchor_dir = anchor_text
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    file_paths
        .iter()
        .filter(|path| {
            let text = path.display();
            *path != anchor && text.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("") == anchor_dir
        })
        .cloned()
        .collect()
}

fn build_feature_slice_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    let mut pairs: BTreeSet<(RepoPath, RepoPath)> = BTreeSet::new();
    for changed in input.changed_paths {
        if !graph.file_paths.contains(changed) {
            continue;
        }
        for sibling in feature_slice_siblings(&graph.file_paths, changed) {
            if input.changed_paths.contains(&sibling) {
                continue;
            }
            let pair = if *changed < sibling {
                (changed.clone(), sibling)
            } else {
                (sibling, changed.clone())
            };
            pairs.insert(pair);
        }
    }
    for (left, right) in pairs {
        let Some(root) = feature_slice_root(&left) else {
            continue;
        };
        let Some(confidence) = feature_slice_confidence(&left, &right) else {
            continue;
        };
        let from = ContextNodeId::File { path: left.clone() };
        let to = ContextNodeId::File {
            path: right.clone(),
        };
        let detail = format!("feature slice '{root}'");
        graph.add_edge(ContextEdge {
            id: edge_id(&from, &to, ContextEdgeKind::SameModule, &detail),
            from,
            to,
            kind: ContextEdgeKind::SameModule,
            confidence,
            reason: format!("same feature slice ({root})"),
            provenance: provenance(ContextGraphSource::SnapshotManifest, detail),
        });
    }
}

fn feature_slice_siblings(file_paths: &BTreeSet<RepoPath>, anchor: &RepoPath) -> Vec<RepoPath> {
    let Some(root) = feature_slice_root(anchor) else {
        return Vec::new();
    };
    let anchor_dir = dir_of(anchor);
    let mut siblings = file_paths
        .iter()
        .filter(|path| {
            *path != anchor
                && feature_slice_root(path).as_deref() == Some(root.as_str())
                && dir_of(path) != anchor_dir
                && feature_slice_confidence(anchor, path).is_some()
        })
        .cloned()
        .collect::<Vec<_>>();
    siblings.sort_by(|left, right| {
        feature_slice_confidence(anchor, right)
            .unwrap_or(0.0)
            .total_cmp(&feature_slice_confidence(anchor, left).unwrap_or(0.0))
            .then_with(|| left.display().cmp(&right.display()))
    });
    siblings.truncate(FEATURE_SLICE_MAX_SIBLINGS);
    siblings
}

fn feature_slice_root(path: &RepoPath) -> Option<String> {
    let text = path.display();
    let parts = text.split('/').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        if *part == "features" && index + 1 < parts.len() {
            return Some(parts[..=index + 1].join("/"));
        }
    }
    None
}

fn feature_slice_confidence(left: &RepoPath, right: &RepoPath) -> Option<f32> {
    if !path_tokens(left).is_disjoint(&path_tokens(right)) {
        Some(0.55)
    } else {
        None
    }
}

fn path_tokens(path: &RepoPath) -> BTreeSet<String> {
    let mut after_feature_root = false;
    let mut skip_feature_name = false;
    let mut tokens = BTreeSet::new();
    for part in path.display().split('/') {
        if skip_feature_name {
            skip_feature_name = false;
            after_feature_root = true;
            continue;
        }
        if part == "features" {
            skip_feature_name = true;
            continue;
        }
        if !after_feature_root {
            continue;
        }
        tokens.extend(
            part.split(['-', '_', '.', '[', ']', '(', ')'])
                .filter(|part| part.len() >= 4)
                .filter(|part| !is_common_feature_path_token(part))
                .map(str::to_ascii_lowercase),
        );
    }
    tokens
}

fn is_common_feature_path_token(part: &str) -> bool {
    matches!(
        part,
        "api"
            | "app"
            | "apps"
            | "client"
            | "component"
            | "components"
            | "feature"
            | "features"
            | "hook"
            | "hooks"
            | "index"
            | "lib"
            | "model"
            | "models"
            | "page"
            | "route"
            | "routes"
            | "server"
            | "src"
            | "test"
            | "tests"
            | "type"
            | "types"
            | "util"
            | "utils"
    )
}

fn dir_of(path: &RepoPath) -> String {
    path.display()
        .rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default()
}

/// Recency decay per commit step back in history. With 500 commits the
/// oldest commit still contributes ~0.007.
const CO_CHANGE_DECAY: f32 = 0.99;

/// Walk the last `commit_limit` commits of the checkout at `repo_root`
/// and record co-change facts relative to the review's changed files:
/// an aggregate stat per co-changed file (for rank signals) and pairwise
/// stats per (changed file, other file) pair (for `CoChanged` edges).
///
/// Deterministic: pure function of the pinned history. A missing `.git`
/// or failing `git` binary degrades to empty maps.
pub(crate) fn co_change_facts(
    repo_root: &Path,
    changed: &BTreeSet<RepoPath>,
    commit_limit: usize,
) -> (
    BTreeMap<RepoPath, CoChangeStat>,
    BTreeMap<(RepoPath, RepoPath), CoChangeStat>,
) {
    let mut aggregate = BTreeMap::new();
    let mut pairs = BTreeMap::new();
    if changed.is_empty() || commit_limit == 0 {
        return (aggregate, pairs);
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("log")
        .arg("--name-only")
        .arg("--pretty=format:\u{1}")
        .arg("-n")
        .arg(commit_limit.to_string())
        .output();
    let Ok(output) = output else {
        return (aggregate, pairs);
    };
    if !output.status.success() {
        return (aggregate, pairs);
    }
    let log = String::from_utf8_lossy(&output.stdout);
    for (commit_index, block) in log
        .split('\u{1}')
        .filter(|b| !b.trim().is_empty())
        .enumerate()
    {
        let files: Vec<RepoPath> = block
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(|line| RepoPath::parse(line).ok())
            .collect();
        let commit_changed: Vec<&RepoPath> = files
            .iter()
            .filter(|file| changed.contains(*file))
            .collect();
        if commit_changed.is_empty() {
            continue;
        }
        let decay = CO_CHANGE_DECAY.powi(commit_index as i32);
        for file in &files {
            if changed.contains(file) {
                continue;
            }
            let entry = aggregate.entry(file.clone()).or_insert(CoChangeStat {
                count: 0,
                weight: 0.0,
            });
            entry.count += 1;
            entry.weight += decay;
            for changed_file in &commit_changed {
                let entry = pairs
                    .entry(((*changed_file).clone(), file.clone()))
                    .or_insert(CoChangeStat {
                        count: 0,
                        weight: 0.0,
                    });
                entry.count += 1;
                entry.weight += decay;
            }
        }
    }
    (aggregate, pairs)
}
