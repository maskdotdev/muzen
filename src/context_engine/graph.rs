//! Change-rooted reference graph (R4).
//!
//! The diff is the retrieval anchor: the highest-precision review context
//! is the blast radius of the change. This module resolves imports to
//! defining files per language, builds a file-level reference graph,
//! computes git co-change frequency, and expands bounded candidate sets
//! from the changed files with typed relationships.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::runtime::contracts::RepoPath;

use super::syntax::ParsedSymbols;
use super::ContextRelationshipKind;

/// One import edge between files. `resolved` is true when the module
/// specifier resolved to the defining file; false when the edge fell back
/// to bare-name matching (lower confidence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceEdge {
    pub from: RepoPath,
    pub to: RepoPath,
    pub symbols: Vec<String>,
    pub resolved: bool,
}

/// Co-change statistics for one file relative to the review's changed
/// files: raw commit count plus recency-decayed weight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoChangeStat {
    pub count: u32,
    pub weight: f32,
}

#[derive(Debug, Clone, Default)]
pub struct ReferenceGraph {
    pub edges: Vec<ReferenceEdge>,
    /// importer file -> indexes into `edges`
    pub edges_from: BTreeMap<RepoPath, Vec<usize>>,
    /// defining file -> indexes into `edges`
    pub edges_to: BTreeMap<RepoPath, Vec<usize>>,
    /// All parsed file paths, for same-module sibling lookups.
    pub paths: BTreeSet<RepoPath>,
    pub co_change: BTreeMap<RepoPath, CoChangeStat>,
}

/// Cap on bare-name fallback fan-out: a common name defined in many files
/// would otherwise create noise edges.
const NAME_FALLBACK_MAX_DEFINERS: usize = 4;

impl ReferenceGraph {
    pub fn build(parsed_by_file: &BTreeMap<RepoPath, ParsedSymbols>) -> Self {
        let paths: BTreeSet<RepoPath> = parsed_by_file.keys().cloned().collect();
        let mut definers_by_name: BTreeMap<&str, BTreeSet<&RepoPath>> = BTreeMap::new();
        for (path, parsed) in parsed_by_file {
            for definition in &parsed.definitions {
                definers_by_name
                    .entry(definition.as_str())
                    .or_default()
                    .insert(path);
            }
        }

        let mut graph = Self {
            paths: paths.clone(),
            ..Self::default()
        };
        for (importer, parsed) in parsed_by_file {
            // (to, resolved) -> symbols, so one edge per file pair per kind
            let mut grouped: BTreeMap<(RepoPath, bool), BTreeSet<String>> = BTreeMap::new();
            for statement in &parsed.import_statements {
                let resolved_target = statement
                    .module
                    .as_deref()
                    .and_then(|module| resolve_module(importer, module, &paths));
                match resolved_target {
                    Some(target) if target != *importer => {
                        grouped
                            .entry((target, true))
                            .or_default()
                            .extend(statement.names.iter().cloned());
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
                                    grouped
                                        .entry(((*definer).clone(), false))
                                        .or_default()
                                        .insert(name.clone());
                                }
                            }
                        }
                    }
                }
            }
            for ((to, resolved), symbols) in grouped {
                let edge_index = graph.edges.len();
                graph
                    .edges_from
                    .entry(importer.clone())
                    .or_default()
                    .push(edge_index);
                graph.edges_to.entry(to.clone()).or_default().push(edge_index);
                graph.edges.push(ReferenceEdge {
                    from: importer.clone(),
                    to,
                    symbols: symbols.into_iter().collect(),
                    resolved,
                });
            }
        }
        graph
    }

    /// Files that import from `path` (callers / users).
    pub fn referencers(&self, path: &RepoPath) -> impl Iterator<Item = &ReferenceEdge> {
        self.edges_to
            .get(path)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
    }

    /// Files that `path` imports from (callees / dependencies).
    pub fn references(&self, path: &RepoPath) -> impl Iterator<Item = &ReferenceEdge> {
        self.edges_from
            .get(path)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
    }
}

/// Resolve a module specifier to the defining file within the indexed
/// path set. Returns `None` for unresolvable specifiers (external
/// packages, stdlib), which degrade to name matching.
fn resolve_module(importer: &RepoPath, module: &str, paths: &BTreeSet<RepoPath>) -> Option<RepoPath> {
    if module.starts_with("./") || module.starts_with("../") {
        return resolve_relative_specifier(importer, module, paths);
    }
    if module.contains("::") || module == "crate" || module == "self" || module == "super" {
        return resolve_rust_path(importer, module, paths);
    }
    if importer.display().ends_with(".py") {
        return resolve_python_module(module, paths);
    }
    None
}

fn try_paths<'a>(
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
    let mut segments: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in specifier.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    let mut candidates = vec![joined.clone()];
    for extension in [".ts", ".tsx", ".js", ".jsx"] {
        candidates.push(format!("{joined}{extension}"));
    }
    for index in ["/index.ts", "/index.tsx", "/index.js", "/index.jsx"] {
        candidates.push(format!("{joined}{index}"));
    }
    try_paths(candidates, paths)
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
        [
            format!("{joined}.py"),
            format!("{joined}/__init__.py"),
        ],
        paths,
    )
}

/// Recency decay per commit step back in history. With 500 commits the
/// oldest commit still contributes ~0.007.
const CO_CHANGE_DECAY: f32 = 0.99;

/// Walk the last `commit_limit` commits of the checkout at `repo_root` and
/// record, for every file that co-occurred in a commit with any of the
/// review's changed files, a count and recency-decayed weight.
///
/// Deterministic: pure function of the pinned history. A missing `.git`
/// or failing `git` binary degrades to an empty map.
pub fn co_change_stats(
    repo_root: &Path,
    changed: &BTreeSet<RepoPath>,
    commit_limit: usize,
) -> BTreeMap<RepoPath, CoChangeStat> {
    let mut stats = BTreeMap::new();
    if changed.is_empty() || commit_limit == 0 {
        return stats;
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
        return stats;
    };
    if !output.status.success() {
        return stats;
    }
    let log = String::from_utf8_lossy(&output.stdout);
    for (commit_index, block) in log.split('\u{1}').filter(|b| !b.trim().is_empty()).enumerate() {
        let files: Vec<&str> = block
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let touches_change = files.iter().any(|file| {
            RepoPath::parse(file)
                .map(|path| changed.contains(&path))
                .unwrap_or(false)
        });
        if !touches_change {
            continue;
        }
        let decay = CO_CHANGE_DECAY.powi(commit_index as i32);
        for file in files {
            let Ok(path) = RepoPath::parse(file) else {
                continue;
            };
            if changed.contains(&path) {
                continue;
            }
            let entry = stats.entry(path).or_insert(CoChangeStat {
                count: 0,
                weight: 0.0,
            });
            entry.count += 1;
            entry.weight += decay;
        }
    }
    stats
}

/// One expansion candidate rooted at a changed file.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphCandidate {
    pub anchor: RepoPath,
    pub path: RepoPath,
    pub kind: ContextRelationshipKind,
    pub confidence: f32,
    pub reason: String,
    pub hop: u8,
}

/// Expand bounded candidate sets from the changed files. Returns the
/// candidates plus the number dropped to respect budgets (for omission
/// records). Deterministic: anchors and edges iterate in `BTreeMap`
/// order, candidates dedupe to the highest-confidence entry.
pub fn expand_from_changes(
    graph: &ReferenceGraph,
    changed: &BTreeSet<RepoPath>,
    max_hops: usize,
    max_candidates_per_anchor: usize,
) -> (Vec<GraphCandidate>, usize) {
    let mut all: Vec<GraphCandidate> = Vec::new();
    let mut overflow = 0usize;
    for anchor in changed {
        let mut anchor_candidates: Vec<GraphCandidate> = Vec::new();
        if max_hops >= 1 {
            collect_neighbors(graph, anchor, anchor, changed, 1, &mut anchor_candidates);
            for sibling in same_module_siblings(graph, anchor) {
                if changed.contains(&sibling) {
                    continue;
                }
                anchor_candidates.push(GraphCandidate {
                    anchor: anchor.clone(),
                    path: sibling.clone(),
                    kind: ContextRelationshipKind::SameModule,
                    confidence: 0.4,
                    reason: format!("same module as {}", anchor.display()),
                    hop: 1,
                });
            }
            // Co-change is computed relative to the whole changed set; emit
            // it from the first anchor only so it is not duplicated.
            if anchor == changed.iter().next().expect("changed set non-empty") {
                for (path, stat) in &graph.co_change {
                    if changed.contains(path) {
                        continue;
                    }
                    anchor_candidates.push(GraphCandidate {
                        anchor: anchor.clone(),
                        path: path.clone(),
                        kind: ContextRelationshipKind::CoChanged,
                        confidence: (0.3 + stat.weight / 10.0).min(0.8),
                        reason: format!("co-changed in {} past commits", stat.count),
                        hop: 1,
                    });
                }
            }
        }
        if max_hops >= 2 {
            let hop_one_paths: BTreeSet<RepoPath> = anchor_candidates
                .iter()
                .filter(|candidate| {
                    matches!(
                        candidate.kind,
                        ContextRelationshipKind::Calls | ContextRelationshipKind::CalledBy
                    )
                })
                .map(|candidate| candidate.path.clone())
                .collect();
            for via in &hop_one_paths {
                collect_neighbors(graph, anchor, via, changed, 2, &mut anchor_candidates);
            }
        }
        // Dedupe (path, kind) keeping highest confidence / lowest hop.
        anchor_candidates.sort_by(|left, right| {
            (&left.path, left.kind)
                .cmp(&(&right.path, right.kind))
                .then(left.hop.cmp(&right.hop))
                .then(right.confidence.total_cmp(&left.confidence))
        });
        anchor_candidates.dedup_by(|next, kept| next.path == kept.path && next.kind == kept.kind);
        // Highest-confidence candidates within the per-anchor budget.
        anchor_candidates.sort_by(|left, right| {
            right
                .confidence
                .total_cmp(&left.confidence)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        if anchor_candidates.len() > max_candidates_per_anchor {
            overflow += anchor_candidates.len() - max_candidates_per_anchor;
            anchor_candidates.truncate(max_candidates_per_anchor);
        }
        all.extend(anchor_candidates);
    }
    (all, overflow)
}

fn collect_neighbors(
    graph: &ReferenceGraph,
    anchor: &RepoPath,
    from: &RepoPath,
    changed: &BTreeSet<RepoPath>,
    hop: u8,
    out: &mut Vec<GraphCandidate>,
) {
    let hop_scale = if hop == 1 { 1.0 } else { 0.5 };
    for edge in graph.referencers(from) {
        if changed.contains(&edge.from) {
            continue;
        }
        let base = if edge.resolved { 0.9 } else { 0.5 };
        let kind = if is_test_path(&edge.from.display()) {
            ContextRelationshipKind::Tests
        } else {
            ContextRelationshipKind::CalledBy
        };
        out.push(GraphCandidate {
            anchor: anchor.clone(),
            path: edge.from.clone(),
            kind,
            confidence: base * hop_scale,
            reason: format!(
                "references {} ({})",
                from.display(),
                edge.symbols.join(", ")
            ),
            hop,
        });
    }
    for edge in graph.references(from) {
        if changed.contains(&edge.to) {
            continue;
        }
        let base = if edge.resolved { 0.85 } else { 0.45 };
        out.push(GraphCandidate {
            anchor: anchor.clone(),
            path: edge.to.clone(),
            kind: ContextRelationshipKind::Calls,
            confidence: base * hop_scale,
            reason: format!(
                "{} imports from it ({})",
                from.display(),
                edge.symbols.join(", ")
            ),
            hop,
        });
    }
}

fn same_module_siblings(graph: &ReferenceGraph, anchor: &RepoPath) -> Vec<RepoPath> {
    let anchor_text = anchor.display();
    let anchor_dir = anchor_text
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    graph
        .paths
        .iter()
        .filter(|path| {
            let text = path.display();
            *path != anchor
                && text.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("") == anchor_dir
        })
        .cloned()
        .collect()
}

pub(crate) fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("test") || lower.contains("spec") || lower.contains("__tests__")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_engine::syntax::ImportStatement;

    fn repo_path(text: &str) -> RepoPath {
        RepoPath::parse(text).unwrap()
    }

    fn parsed(
        definitions: &[&str],
        statements: Vec<ImportStatement>,
    ) -> ParsedSymbols {
        ParsedSymbols {
            definitions: definitions.iter().map(|s| s.to_string()).collect(),
            definition_ranges: BTreeMap::new(),
            imports: Vec::new(),
            import_statements: statements,
        }
    }

    #[test]
    fn resolves_rust_use_paths_to_defining_files() {
        let mut files = BTreeMap::new();
        files.insert(
            repo_path("src/auth/token.rs"),
            parsed(&["authorize_request"], Vec::new()),
        );
        files.insert(
            repo_path("src/auth/routes.rs"),
            parsed(
                &["route"],
                vec![ImportStatement {
                    module: Some("crate::auth::token".to_string()),
                    names: vec!["authorize_request".to_string()],
                }],
            ),
        );
        let graph = ReferenceGraph::build(&files);
        let edges: Vec<_> = graph.referencers(&repo_path("src/auth/token.rs")).collect();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, repo_path("src/auth/routes.rs"));
        assert!(edges[0].resolved);
        assert_eq!(edges[0].symbols, vec!["authorize_request".to_string()]);
    }

    #[test]
    fn resolves_ts_relative_specifiers_without_bare_name_collisions() {
        let mut files = BTreeMap::new();
        // Two modules define the same name; only the imported one connects.
        files.insert(repo_path("src/a/load.ts"), parsed(&["loadUser"], Vec::new()));
        files.insert(repo_path("src/b/load.ts"), parsed(&["loadUser"], Vec::new()));
        files.insert(
            repo_path("src/app.ts"),
            parsed(
                &["main"],
                vec![ImportStatement {
                    module: Some("./a/load".to_string()),
                    names: vec!["loadUser".to_string()],
                }],
            ),
        );
        let graph = ReferenceGraph::build(&files);
        assert_eq!(
            graph.referencers(&repo_path("src/a/load.ts")).count(),
            1,
            "imported module gains the edge"
        );
        assert_eq!(
            graph.referencers(&repo_path("src/b/load.ts")).count(),
            0,
            "same-named module elsewhere gains no edge"
        );
    }

    #[test]
    fn unresolvable_module_degrades_to_name_match_with_low_confidence() {
        let mut files = BTreeMap::new();
        files.insert(
            repo_path("vendor/lib.rs"),
            parsed(&["helper_fn"], Vec::new()),
        );
        files.insert(
            repo_path("src/main.rs"),
            parsed(
                &["main"],
                vec![ImportStatement {
                    module: Some("external_pkg".to_string()),
                    names: vec!["helper_fn".to_string()],
                }],
            ),
        );
        let graph = ReferenceGraph::build(&files);
        let edges: Vec<_> = graph.referencers(&repo_path("vendor/lib.rs")).collect();
        assert_eq!(edges.len(), 1);
        assert!(!edges[0].resolved, "name-match fallback is low confidence");
    }

    #[test]
    fn resolves_python_modules() {
        let mut files = BTreeMap::new();
        files.insert(repo_path("auth/tokens.py"), parsed(&["Token"], Vec::new()));
        files.insert(
            repo_path("app/main.py"),
            parsed(
                &["main"],
                vec![ImportStatement {
                    module: Some("auth.tokens".to_string()),
                    names: vec!["Token".to_string()],
                }],
            ),
        );
        let graph = ReferenceGraph::build(&files);
        assert_eq!(graph.referencers(&repo_path("auth/tokens.py")).count(), 1);
    }

    #[test]
    fn expansion_respects_candidate_budget_and_reports_overflow() {
        let mut files = BTreeMap::new();
        files.insert(repo_path("src/core.rs"), parsed(&["core_fn"], Vec::new()));
        for index in 0..6 {
            files.insert(
                repo_path(&format!("src/user{index}.rs")),
                parsed(
                    &["use_it"],
                    vec![ImportStatement {
                        module: Some("crate::core".to_string()),
                        names: vec!["core_fn".to_string()],
                    }],
                ),
            );
        }
        let graph = ReferenceGraph::build(&files);
        let changed: BTreeSet<RepoPath> = [repo_path("src/core.rs")].into_iter().collect();
        let (bounded, overflow) = expand_from_changes(&graph, &changed, 2, 3);
        assert_eq!(bounded.len(), 3);
        assert!(overflow > 0, "dropped candidates land in overflow");
        let (all, _) = expand_from_changes(&graph, &changed, 2, 64);
        assert!(all.len() > 3);
        assert!(all
            .iter()
            .any(|candidate| candidate.kind == ContextRelationshipKind::CalledBy));
        assert!(all
            .iter()
            .any(|candidate| candidate.kind == ContextRelationshipKind::SameModule));
    }

    #[test]
    fn test_importers_surface_as_tests_relationship() {
        let mut files = BTreeMap::new();
        files.insert(repo_path("src/core.rs"), parsed(&["core_fn"], Vec::new()));
        files.insert(
            repo_path("tests/core_test.rs"),
            parsed(
                &["core_works"],
                vec![ImportStatement {
                    module: Some("crate::core".to_string()),
                    names: vec!["core_fn".to_string()],
                }],
            ),
        );
        let graph = ReferenceGraph::build(&files);
        let changed: BTreeSet<RepoPath> = [repo_path("src/core.rs")].into_iter().collect();
        let (candidates, _) = expand_from_changes(&graph, &changed, 2, 16);
        assert!(candidates.iter().any(|candidate| {
            candidate.kind == ContextRelationshipKind::Tests
                && candidate.path == repo_path("tests/core_test.rs")
        }));
    }

    #[test]
    fn co_change_counts_files_that_changed_together() {
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        for index in 0..5 {
            std::fs::write(repo.path().join("a.rs"), format!("a{index}")).unwrap();
            if index % 2 == 0 {
                // a.rs and b.rs co-change in commits 0, 2, 4
                std::fs::write(repo.path().join("b.rs"), format!("b{index}")).unwrap();
            } else {
                std::fs::write(repo.path().join("c.rs"), format!("c{index}")).unwrap();
            }
            run(&["add", "."]);
            run(&["commit", "-q", "-m", "step", "--no-gpg-sign"]);
        }
        let changed: BTreeSet<RepoPath> = [repo_path("a.rs")].into_iter().collect();
        let stats = co_change_stats(repo.path(), &changed, 500);
        assert_eq!(stats.get(&repo_path("b.rs")).map(|s| s.count), Some(3));
        assert!(stats.get(&repo_path("b.rs")).unwrap().weight > 0.0);
        // c.rs co-changed in the other 2 commits
        assert_eq!(stats.get(&repo_path("c.rs")).map(|s| s.count), Some(2));
    }

    #[test]
    fn missing_git_history_degrades_to_empty_co_change() {
        let dir = tempfile::tempdir().unwrap();
        let changed: BTreeSet<RepoPath> = [repo_path("a.rs")].into_iter().collect();
        let stats = co_change_stats(dir.path(), &changed, 500);
        assert!(stats.is_empty());
    }
}
