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

/// Confidence for `mod name;` containment edges: a declared route, but
/// containment rather than usage -- ranked below resolved imports so
/// module parents do not outrank true dependencies in expansion.
const MOD_DECLARATION_CONFIDENCE: f32 = 0.55;

/// Cap on bare-name fallback fan-out: a common name defined in many files
/// would otherwise create noise edges.
const NAME_FALLBACK_MAX_DEFINERS: usize = 4;

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
    /// Cross-repo contracts declared by host metadata, keyed by the
    /// metadata key; values may name the repository paths they cover.
    pub external_contracts: &'a BTreeMap<String, serde_json::Value>,
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

        // ---- Rust `mod name;` edges for modules nothing else reaches.
        build_mod_declaration_edges(&mut graph, &input, &provenance);

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
        // changed files, so sufficiency never recreates stem logic.
        build_test_convention_edges(&mut graph, &input, &provenance);

        // ---- Configures edges: manifest/config files point at the
        // targets they declare, so config sufficiency is graph-derived.
        build_configures_edges(&mut graph, &input, &provenance);

        // ---- External contract edges from host metadata declarations.
        build_external_contract_edges(&mut graph, &input, &provenance);

        graph
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

/// Stem-convention test edges for changed files: `foo.test.ts`,
/// `foo_test.rs`, `test_foo.py` style names test `foo.*`. Matching is
/// exact on the normalized stem -- substring matching would flood a
/// repository full of `index.ts` or `route.ts` files with false test
/// edges. A match anywhere in the tree is accepted only when unique;
/// otherwise the test must live near the changed file.
fn build_test_convention_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    let test_paths: Vec<RepoPath> = graph
        .file_paths
        .iter()
        .filter(|path| is_test_path(&path.display()))
        .cloned()
        .collect();
    for changed in input.changed_paths {
        if !graph.file_paths.contains(changed) || is_test_path(&changed.display()) {
            continue;
        }
        let stem = path_stem(&changed.display());
        if stem.is_empty() {
            continue;
        }
        let matches: Vec<&RepoPath> = test_paths
            .iter()
            .filter(|test_path| {
                *test_path != changed && normalized_test_stem(&test_path.display()) == stem
            })
            .collect();
        let unique = matches.len() == 1;
        for test_path in matches {
            if !unique && !is_near(test_path, changed) {
                continue;
            }
            let from = ContextNodeId::File {
                path: test_path.clone(),
            };
            let to = ContextNodeId::File {
                path: changed.clone(),
            };
            let already_tested = graph
                .file_referencers(changed)
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
                    changed.display()
                ),
                provenance: provenance(ContextGraphSource::TestConvention, detail),
            });
        }
    }
}

/// Rust `mod name;` edges, emitted only for child modules that resolver
/// edges left orphaned (no cross-file Imports/References/Tests
/// in-edges). A module parent declares every submodule, so emitting the
/// edge unconditionally turns `lib.rs`/`mod.rs` into hop-1 hubs that
/// outrank true dependencies; the declaration only adds recall when
/// nothing else connects the child.
fn build_mod_declaration_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    let paths = graph.file_paths.clone();
    for (importer, parsed) in input.parsed_by_file {
        if parsed.mod_declarations.is_empty() || !importer.display().ends_with(".rs") {
            continue;
        }
        for name in &parsed.mod_declarations {
            let Some(target) = resolve_mod_declaration(importer, name, &paths) else {
                continue;
            };
            if target == *importer {
                continue;
            }
            let connected = graph.file_referencers(&target).any(|edge| {
                matches!(
                    edge.kind,
                    ContextEdgeKind::Imports | ContextEdgeKind::References | ContextEdgeKind::Tests
                )
            });
            if connected {
                continue;
            }
            let from = ContextNodeId::File {
                path: importer.clone(),
            };
            let to = ContextNodeId::File {
                path: target.clone(),
            };
            let detail = format!("mod declaration '{name}'");
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::Imports, &detail),
                from,
                to,
                kind: ContextEdgeKind::Imports,
                confidence: MOD_DECLARATION_CONFIDENCE,
                reason: format!(
                    "{} declares module {name} ({})",
                    importer.display(),
                    target.display()
                ),
                provenance: provenance(ContextGraphSource::ImportResolver, detail),
            });
        }
    }
}

/// Cap on `Configures` targets per config file: a manifest declaring
/// more routes than this contributes its first declarations only.
const MAX_CONFIGURES_TARGETS_PER_CONFIG: usize = 16;

/// `Configures` edges from manifest/config files to the snapshot files
/// they declare: package.json entry points, exports, and bins; tsconfig
/// extends chains and `files`; Cargo.toml lib/bin paths, workspace
/// members, and conventional crate roots.
fn build_configures_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    let paths = graph.file_paths.clone();
    for (config_path, text) in input.file_contents {
        if !paths.contains(config_path) {
            continue;
        }
        let config_text = config_path.display();
        let (dir, file_name) = match config_text.rsplit_once('/') {
            Some((dir, file_name)) => (dir, file_name),
            None => ("", config_text.as_str()),
        };
        let targets: Vec<(RepoPath, String)> = match file_name {
            "package.json" => parse_jsonc(text)
                .map(|value| package_configure_targets(dir, &value, &paths))
                .unwrap_or_default(),
            "tsconfig.json" | "jsconfig.json" => parse_jsonc(text)
                .map(|value| tsconfig_configure_targets(dir, &value, &paths))
                .unwrap_or_default(),
            "Cargo.toml" => cargo_configure_targets(dir, text, &paths),
            _ => continue,
        };
        // One edge per (config, target): the first declared route wins.
        let mut seen: BTreeSet<RepoPath> = BTreeSet::new();
        let from = ContextNodeId::File {
            path: config_path.clone(),
        };
        for (target, route) in targets {
            if target == *config_path || !seen.insert(target.clone()) {
                continue;
            }
            if seen.len() > MAX_CONFIGURES_TARGETS_PER_CONFIG {
                break;
            }
            let to = ContextNodeId::File {
                path: target.clone(),
            };
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &to, ContextEdgeKind::Configures, &route),
                from: from.clone(),
                to,
                kind: ContextEdgeKind::Configures,
                confidence: DECLARED_RESOLUTION_CONFIDENCE,
                reason: format!(
                    "{} configures {} ({route})",
                    config_path.display(),
                    target.display()
                ),
                provenance: provenance(ContextGraphSource::SnapshotManifest, route),
            });
        }
    }
}

/// Declared targets in a `package.json`: entry-point fields, bin
/// entries, and `exports` map string leaves.
fn package_configure_targets(
    dir: &str,
    value: &serde_json::Value,
    paths: &BTreeSet<RepoPath>,
) -> Vec<(RepoPath, String)> {
    let mut targets = Vec::new();
    let add = |spec: &str, route: String, targets: &mut Vec<(RepoPath, String)>| {
        if let Some(joined) = normalize_join(dir, spec) {
            if let Some(found) = try_paths(ts_file_candidates(&joined), paths) {
                targets.push((found, route));
            }
        }
    };
    for key in ["main", "module", "types", "typings", "browser"] {
        if let Some(spec) = value.get(key).and_then(|spec| spec.as_str()) {
            add(spec, format!("{key} '{spec}'"), &mut targets);
        }
    }
    match value.get("bin") {
        Some(serde_json::Value::String(spec)) => add(spec, format!("bin '{spec}'"), &mut targets),
        Some(serde_json::Value::Object(map)) => {
            for (name, spec) in map {
                if let Some(spec) = spec.as_str() {
                    add(spec, format!("bin '{name}' -> '{spec}'"), &mut targets);
                }
            }
        }
        _ => {}
    }
    if let Some(exports) = value.get("exports") {
        let mut specs = Vec::new();
        collect_export_path_strings(exports, &mut specs);
        for spec in specs {
            add(&spec, format!("exports '{spec}'"), &mut targets);
        }
    }
    targets
}

/// String leaves of an `exports` map that name concrete relative paths
/// (wildcard patterns configure a family, not a file; skip them).
fn collect_export_path_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(spec) => {
            if spec.starts_with("./") && !spec.contains('*') {
                out.push(spec.clone());
            }
        }
        serde_json::Value::Object(map) => {
            for nested in map.values() {
                collect_export_path_strings(nested, out);
            }
        }
        _ => {}
    }
}

/// Declared targets in a tsconfig/jsconfig: the `extends` chain and the
/// explicit `files` list.
fn tsconfig_configure_targets(
    dir: &str,
    value: &serde_json::Value,
    paths: &BTreeSet<RepoPath>,
) -> Vec<(RepoPath, String)> {
    let mut targets = Vec::new();
    let extend_specs: Vec<&str> = match value.get("extends") {
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
        let Some(joined) = normalize_join(dir, spec) else {
            continue;
        };
        let candidates = if joined.ends_with(".json") {
            vec![joined.clone()]
        } else {
            vec![format!("{joined}.json"), joined]
        };
        if let Some(found) = try_paths(candidates, paths) {
            targets.push((found, format!("extends '{spec}'")));
        }
    }
    if let Some(files) = value.get("files").and_then(|files| files.as_array()) {
        for spec in files.iter().filter_map(|spec| spec.as_str()) {
            if let Some(joined) = normalize_join(dir, spec) {
                if let Some(found) = try_paths([joined], paths) {
                    targets.push((found, format!("files '{spec}'")));
                }
            }
        }
    }
    targets
}

/// Declared targets in a Cargo manifest: `[lib]`/`[[bin]]` path keys,
/// workspace members (their manifests), and conventional crate roots.
fn cargo_configure_targets(
    dir: &str,
    text: &str,
    paths: &BTreeSet<RepoPath>,
) -> Vec<(RepoPath, String)> {
    let mut targets = Vec::new();
    let mut section = String::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.trim_matches(['[', ']']).to_string();
            continue;
        }
        match section.as_str() {
            "lib" | "bin" => {
                if let Some(spec) = toml_string_value(trimmed, "path") {
                    if let Some(joined) = normalize_join(dir, &spec) {
                        if let Some(found) = try_paths([joined], paths) {
                            targets.push((found, format!("[{section}] path '{spec}'")));
                        }
                    }
                }
            }
            "workspace" => {
                if trimmed.starts_with("members") {
                    let mut buffer = trimmed.to_string();
                    while !buffer.contains(']') {
                        match lines.next() {
                            Some(next) => buffer.push_str(next.trim()),
                            None => break,
                        }
                    }
                    for member in quoted_strings(&buffer) {
                        if member.contains('*') {
                            continue;
                        }
                        if let Some(joined) = normalize_join(dir, &format!("{member}/Cargo.toml")) {
                            if let Some(found) = try_paths([joined], paths) {
                                targets.push((found, format!("workspace member '{member}'")));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for conventional in ["src/lib.rs", "src/main.rs"] {
        if let Some(joined) = normalize_join(dir, conventional) {
            if let Some(found) = try_paths([joined], paths) {
                targets.push((found, format!("crate root '{conventional}'")));
            }
        }
    }
    targets
}

/// `key = "value"` on one TOML line; enough for the manifest keys the
/// graph reads, without a TOML dependency.
fn toml_string_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    rest.split_once('"').map(|(value, _)| value.to_string())
}

fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else {
            break;
        };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

/// Cap on `ExternalContract` edges per contract when the declaration
/// names no paths and falls back to the review's changed files.
const MAX_CONTRACT_SCOPE_FILES: usize = 32;

/// `ExternalContract` edges from covered file nodes to the repo node
/// (which stands in for the host-scoped contract until contracts get
/// richer structure). Paths the declaration names resolve at full
/// confidence; a declaration without paths scopes to the changed files
/// at reduced confidence so coverage checks still see it.
fn build_external_contract_edges(
    graph: &mut ContextGraph,
    input: &ContextGraphBuildInput,
    provenance: &impl Fn(ContextGraphSource, String) -> ContextGraphProvenance,
) {
    if input.external_contracts.is_empty() {
        return;
    }
    let repo_id = ContextNodeId::Repo {
        snapshot_id: input.snapshot_id.clone(),
    };
    for (key, value) in input.external_contracts {
        let mut covered: Vec<(RepoPath, f32, String)> = Vec::new();
        for spec in contract_declared_paths(value) {
            let Ok(path) = RepoPath::parse(&spec) else {
                continue;
            };
            if graph.file_paths.contains(&path) {
                covered.push((
                    path,
                    DECLARED_RESOLUTION_CONFIDENCE,
                    format!("declared path '{spec}'"),
                ));
            }
        }
        if covered.is_empty() {
            covered.extend(
                input
                    .changed_paths
                    .iter()
                    .filter(|path| graph.file_paths.contains(*path))
                    .take(MAX_CONTRACT_SCOPE_FILES)
                    .map(|path| {
                        (
                            path.clone(),
                            0.5,
                            "no declared paths; scoped to changed files".to_string(),
                        )
                    }),
            );
        }
        for (path, confidence, detail) in covered {
            let from = ContextNodeId::File { path: path.clone() };
            let detail = format!("{key}: {detail}");
            graph.add_edge(ContextEdge {
                id: edge_id(&from, &repo_id, ContextEdgeKind::ExternalContract, &detail),
                from,
                to: repo_id.clone(),
                kind: ContextEdgeKind::ExternalContract,
                confidence,
                reason: format!(
                    "{} is covered by external contract '{key}'",
                    path.display()
                ),
                provenance: provenance(ContextGraphSource::HostMetadata, detail),
            });
        }
    }
}

/// Paths a contract declaration names: `paths`/`files` arrays or a
/// single `path` string in the metadata value.
fn contract_declared_paths(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(map) = value.as_object() else {
        return out;
    };
    for key in ["paths", "files"] {
        if let Some(entries) = map.get(key).and_then(|entries| entries.as_array()) {
            out.extend(
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string)),
            );
        }
    }
    if let Some(path) = map.get("path").and_then(|path| path.as_str()) {
        out.push(path.to_string());
    }
    out
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

/// Bound on re-export chain depth: barrels of barrels resolve, but a
/// pathological chain cannot recurse unbounded.
const REEXPORT_MAX_DEPTH: usize = 3;

/// Hops through barrel/re-export files: statements in `target` that
/// bind `name` (named re-export) or bind nothing (`export * from`,
/// wildcard `use`) resolve to the files that actually provide the name,
/// following chained barrels up to `REEXPORT_MAX_DEPTH`.
fn barrel_hops(
    target: &RepoPath,
    target_parsed: &ParsedSymbols,
    name: &str,
    parsed_by_file: &BTreeMap<RepoPath, ParsedSymbols>,
    paths: &BTreeSet<RepoPath>,
    resolver: &ModuleResolver,
) -> Vec<RepoPath> {
    let mut hops = Vec::new();
    let mut visited = BTreeSet::new();
    visited.insert(target.clone());
    collect_barrel_hops(
        target,
        target_parsed,
        name,
        parsed_by_file,
        paths,
        resolver,
        0,
        &mut visited,
        &mut hops,
    );
    hops.sort();
    hops.dedup();
    hops
}

#[allow(clippy::too_many_arguments)]
fn collect_barrel_hops(
    target: &RepoPath,
    target_parsed: &ParsedSymbols,
    name: &str,
    parsed_by_file: &BTreeMap<RepoPath, ParsedSymbols>,
    paths: &BTreeSet<RepoPath>,
    resolver: &ModuleResolver,
    depth: usize,
    visited: &mut BTreeSet<RepoPath>,
    hops: &mut Vec<RepoPath>,
) {
    // `mod name;` declarations are routes too: `use util::helpers`
    // resolves to `util.rs`, whose `mod helpers;` names the child file.
    if target_parsed
        .mod_declarations
        .iter()
        .any(|declared| declared == name)
    {
        if let Some(child) = resolve_mod_declaration(target, name, paths) {
            if child != *target {
                hops.push(child);
            }
        }
    }
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
        let resolved_parsed = parsed_by_file.get(&resolved.path);
        let defines = resolved_parsed
            .is_some_and(|parsed| parsed.definitions.iter().any(|definition| definition == name));
        let may_recurse = depth + 1 < REEXPORT_MAX_DEPTH
            && !defines
            && !visited.contains(&resolved.path)
            && resolved_parsed.is_some();
        if wildcard {
            // A wildcard re-export justifies the hop only when the
            // resolved file defines the name; otherwise it may be a
            // deeper barrel that re-exports it.
            if defines {
                hops.push(resolved.path);
            } else if may_recurse {
                visited.insert(resolved.path.clone());
                collect_barrel_hops(
                    &resolved.path,
                    resolved_parsed.unwrap(),
                    name,
                    parsed_by_file,
                    paths,
                    resolver,
                    depth + 1,
                    visited,
                    hops,
                );
            }
            continue;
        }
        if defines || !may_recurse {
            hops.push(resolved.path);
            continue;
        }
        // A named re-export of a name the resolved file does not define:
        // chase the chain, but keep the direct hop if nothing deeper
        // provides the name (the existing one-hop behavior).
        let before = hops.len();
        visited.insert(resolved.path.clone());
        collect_barrel_hops(
            &resolved.path,
            resolved_parsed.unwrap(),
            name,
            parsed_by_file,
            paths,
            resolver,
            depth + 1,
            visited,
            hops,
        );
        if hops.len() == before {
            hops.push(resolved.path);
        }
    }
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
/// aliases, workspace package exports, and Rust workspace crates.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModuleResolver {
    ts: TsProjectConfig,
    packages: PackageExports,
    rust: RustWorkspace,
}

impl ModuleResolver {
    pub(crate) fn from_files(file_contents: &BTreeMap<RepoPath, String>) -> Self {
        Self {
            ts: TsProjectConfig::from_files(file_contents),
            packages: PackageExports::from_files(file_contents),
            rust: RustWorkspace::from_files(file_contents),
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
    let importer_text = importer.display();
    // A single-segment module from a Rust importer (`use other_crate::X`
    // records module `other_crate`) is a rust path only when it names a
    // workspace crate: arbitrary single segments resolve better through
    // the bare-name fallback's definer scan.
    if module.contains("::")
        || module == "crate"
        || module == "self"
        || module == "super"
        || (importer_text.ends_with(".rs") && resolver.rust.knows_crate(module))
    {
        return resolve_rust_path(importer, module, paths, &resolver.rust).map(
            |(path, crate_root_fallback)| ResolvedModule {
                path,
                via: format!("rust path '{module}'"),
                // A crate-root file catching leftover item segments is a
                // layout guess, not a declared module file.
                confidence: if crate_root_fallback {
                    LAYOUT_RESOLUTION_CONFIDENCE
                } else {
                    DECLARED_RESOLUTION_CONFIDENCE
                },
            },
        );
    }
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
/// `mod.rs`), `super::x` relative to the importer's parent module, and
/// workspace crate names (`other_crate::foo`) rooted at that crate's
/// source directory.
/// Returns the resolved file plus whether it was reached through the
/// crate-root (`lib.rs`/`main.rs`) layout fallback rather than a module
/// file named by the path.
fn resolve_rust_path(
    importer: &RepoPath,
    module: &str,
    paths: &BTreeSet<RepoPath>,
    workspace: &RustWorkspace,
) -> Option<(RepoPath, bool)> {
    let importer_text = importer.display();
    let importer_dir = importer_text
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let mut segments: Vec<String> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    // Source roots whose crate-root files (`lib.rs`, `main.rs`) should
    // catch paths whose segments are all items inside the crate root.
    let mut crate_root_dirs: Vec<String> = Vec::new();
    let mut raw = module.split("::").peekable();
    match raw.peek().copied() {
        Some("crate") => {
            raw.next();
            // The importer's own crate: its nearest enclosing `src`
            // directory in a workspace, with repo-level fallbacks. No
            // crate-root file fallback here: an own-crate item path that
            // names no module file resolves better through the bare-name
            // definer scan than by landing on `lib.rs`.
            if let Some(own_src) = nearest_src_root(importer_dir) {
                roots.push(own_src);
            }
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
        Some(first) => {
            if let Some(crate_src) = workspace.src_root(first) {
                // Workspace crate boundary: consume the crate-name
                // segment and root at that crate's source directory.
                raw.next();
                roots.push(crate_src.clone());
                crate_root_dirs.push(crate_src);
            } else {
                // External crate (`serde::...`) unless the first segment
                // is a local top-level module; try source roots anyway.
                roots.push("src".to_string());
                roots.push(String::new());
                roots.push(importer_dir.to_string());
            }
        }
        None => {}
    }
    segments.extend(raw.map(str::to_string));
    if segments.is_empty() && crate_root_dirs.is_empty() {
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
    if let Some(path) = try_paths(candidates, paths) {
        return Some((path, false));
    }
    // Crate-root files last: every module prefix was tried, so the
    // remaining segments must be items defined in the crate root.
    let mut root_candidates = Vec::new();
    for dir in &crate_root_dirs {
        root_candidates.push(format!("{dir}/lib.rs"));
        root_candidates.push(format!("{dir}/main.rs"));
    }
    try_paths(root_candidates, paths).map(|path| (path, true))
}

/// Nearest enclosing `src` directory of a module file, which is the
/// crate source root inside a Cargo workspace member.
fn nearest_src_root(importer_dir: &str) -> Option<String> {
    let mut dir = importer_dir.to_string();
    loop {
        if dir == "src" || dir.ends_with("/src") {
            return Some(dir);
        }
        match dir.rsplit_once('/') {
            Some((parent, _)) => dir = parent.to_string(),
            None => return None,
        }
    }
}

/// Resolve a Rust `mod name;` declaration to the child module file. A
/// crate-root or `mod.rs` parent loads siblings; any other parent file
/// loads from its own subdirectory (2018-style module layout).
fn resolve_mod_declaration(
    importer: &RepoPath,
    name: &str,
    paths: &BTreeSet<RepoPath>,
) -> Option<RepoPath> {
    let importer_text = importer.display();
    let (dir, file_name) = match importer_text.rsplit_once('/') {
        Some((dir, file_name)) => (dir, file_name),
        None => ("", importer_text.as_str()),
    };
    let stem = file_name.strip_suffix(".rs")?;
    let join = |rest: String| {
        if dir.is_empty() {
            rest
        } else {
            format!("{dir}/{rest}")
        }
    };
    let candidates = if matches!(stem, "mod" | "lib" | "main") {
        [join(format!("{name}.rs")), join(format!("{name}/mod.rs"))]
    } else {
        [
            join(format!("{stem}/{name}.rs")),
            join(format!("{stem}/{name}/mod.rs")),
        ]
    };
    try_paths(candidates, paths)
}

// ---------------------------------------------------------------------
// Cargo workspace crates
// ---------------------------------------------------------------------

/// Workspace crates discovered from `Cargo.toml` manifests: normalized
/// package name (dashes as underscores, as the name appears in `use`
/// paths) -> manifest directory.
#[derive(Debug, Clone, Default)]
pub(crate) struct RustWorkspace {
    crates: BTreeMap<String, String>,
}

impl RustWorkspace {
    fn from_files(file_contents: &BTreeMap<RepoPath, String>) -> Self {
        let mut crates = BTreeMap::new();
        for (path, text) in file_contents {
            let path_text = path.display();
            let file_name = path_text
                .rsplit_once('/')
                .map(|(_, name)| name)
                .unwrap_or(&path_text);
            if file_name != "Cargo.toml" {
                continue;
            }
            let Some(name) = toml_package_name(text) else {
                continue;
            };
            let dir = path_text
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or("")
                .to_string();
            // First manifest wins on duplicate names; BTreeMap iteration
            // keeps this deterministic.
            crates.entry(name.replace('-', "_")).or_insert(dir);
        }
        Self { crates }
    }

    /// Whether `name` is a workspace crate as it appears in `use` paths.
    fn knows_crate(&self, name: &str) -> bool {
        self.crates.contains_key(name)
    }

    /// Source root for a crate referenced by its `use`-path name.
    fn src_root(&self, name: &str) -> Option<String> {
        let dir = self.crates.get(name)?;
        Some(if dir.is_empty() {
            "src".to_string()
        } else {
            format!("{dir}/src")
        })
    }
}

/// `[package] name` from a Cargo manifest via a minimal line scan: full
/// TOML parsing buys nothing for one key and would add a dependency.
fn toml_package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(name) = toml_string_value(trimmed, "name") {
                return Some(name);
            }
        }
    }
    None
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
}

/// Workspace packages discovered from `package.json` manifests, for
/// resolving `@scope/pkg/subpath` specifiers through `exports` maps.
#[derive(Debug, Clone, Default)]
struct PackageExports {
    packages: BTreeMap<String, PackageManifest>,
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
                });
        }
        Self { packages }
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
