use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::context_engine::chunking::FileChunk;
use crate::context_engine::syntax::{parse_symbols, ImportStatement, ParsedSymbols};
use crate::context_engine::{ContextRange, ContextRelationshipKind};
use crate::runtime::contracts::{RepoPath, SnapshotId};

use super::build::co_change_facts;
use super::*;

fn repo_path(text: &str) -> RepoPath {
    RepoPath::parse(text).unwrap()
}

fn parsed(definitions: &[&str], statements: Vec<ImportStatement>) -> ParsedSymbols {
    ParsedSymbols {
        definitions: definitions.iter().map(|s| s.to_string()).collect(),
        definition_ranges: BTreeMap::new(),
        imports: Vec::new(),
        import_statements: statements,
    }
}

fn parsed_with_ranges(
    definitions: &[(&str, u32, u32)],
    statements: Vec<ImportStatement>,
) -> ParsedSymbols {
    ParsedSymbols {
        definitions: definitions
            .iter()
            .map(|(name, _, _)| name.to_string())
            .collect(),
        definition_ranges: definitions
            .iter()
            .map(|(name, start, end)| {
                (
                    name.to_string(),
                    ContextRange {
                        start_line: *start,
                        end_line: *end,
                    },
                )
            })
            .collect(),
        imports: Vec::new(),
        import_statements: statements,
    }
}

fn import(module: &str, names: &[&str]) -> ImportStatement {
    ImportStatement {
        module: Some(module.to_string()),
        names: names.iter().map(|s| s.to_string()).collect(),
    }
}

fn contents(entries: &[(&str, &str)]) -> BTreeMap<RepoPath, String> {
    entries
        .iter()
        .map(|(path, text)| (repo_path(path), text.to_string()))
        .collect()
}

fn chunk(start: u32, end: u32, text: &str) -> FileChunk {
    FileChunk {
        start_line: start,
        end_line: end,
        text: text.to_string(),
        symbol_path: None,
        node_kind: "test".to_string(),
    }
}

struct GraphSpec<'a> {
    files: &'a BTreeMap<RepoPath, ParsedSymbols>,
    contents: BTreeMap<RepoPath, String>,
    chunks: BTreeMap<RepoPath, Vec<FileChunk>>,
    hunks: BTreeMap<String, Vec<ContextRange>>,
    changed: BTreeSet<RepoPath>,
}

impl<'a> GraphSpec<'a> {
    fn new(files: &'a BTreeMap<RepoPath, ParsedSymbols>) -> Self {
        Self {
            files,
            contents: BTreeMap::new(),
            chunks: BTreeMap::new(),
            hunks: BTreeMap::new(),
            changed: BTreeSet::new(),
        }
    }

    fn with_contents(mut self, contents: BTreeMap<RepoPath, String>) -> Self {
        self.contents = contents;
        self
    }

    fn with_chunks(mut self, path: &str, chunks: Vec<FileChunk>) -> Self {
        self.chunks.insert(repo_path(path), chunks);
        self
    }

    fn with_hunk(mut self, path: &str, start: u32, end: u32) -> Self {
        self.hunks
            .entry(path.to_string())
            .or_default()
            .push(ContextRange {
                start_line: start,
                end_line: end,
            });
        self
    }

    fn with_changed(mut self, paths: &[&str]) -> Self {
        self.changed = paths.iter().map(|path| repo_path(path)).collect();
        self
    }

    fn build(&self) -> ContextGraph {
        ContextGraph::build(ContextGraphBuildInput {
            snapshot_id: SnapshotId("test-snapshot".to_string()),
            repo_root: Path::new("/nonexistent"),
            parsed_by_file: self.files,
            file_contents: &self.contents,
            chunks_by_file: self
                .chunks
                .iter()
                .map(|(path, chunks)| (path.clone(), chunks.as_slice()))
                .collect(),
            node_kind_by_file: BTreeMap::new(),
            hunk_ranges: &self.hunks,
            changed_paths: &self.changed,
            co_change_commit_limit: 0,
        })
    }
}

fn default_request() -> ContextGraphExpansionRequest {
    ContextGraphExpansionRequest {
        max_hops: 2,
        max_candidates_per_anchor: 16,
        min_confidence: 0.0,
        purpose: ContextGraphExpansionPurpose::Retrieval,
    }
}

fn importer_paths(graph: &ContextGraph, path: &str) -> Vec<(RepoPath, ContextEdgeKind, f32)> {
    graph
        .file_referencers(&repo_path(path))
        .filter_map(|edge| {
            edge.from_path()
                .map(|from| (from.clone(), edge.kind, edge.confidence))
        })
        .collect()
}

fn connects(edge: &ContextEdge, left: &str, right: &str) -> bool {
    let left = repo_path(left);
    let right = repo_path(right);
    matches!(
        (edge.from_path(), edge.to_path()),
        (Some(from), Some(to))
            if (*from == left && *to == right) || (*from == right && *to == left)
    )
}

// ---- resolution -------------------------------------------------------

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
            vec![import("crate::auth::token", &["authorize_request"])],
        ),
    );
    let graph = GraphSpec::new(&files).build();
    let edges = importer_paths(&graph, "src/auth/token.rs");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].0, repo_path("src/auth/routes.rs"));
    assert_eq!(edges[0].1, ContextEdgeKind::Imports);
    assert!(edges[0].2 >= 0.8, "resolved imports are high confidence");
}

#[test]
fn resolves_ts_relative_specifiers_without_bare_name_collisions() {
    let mut files = BTreeMap::new();
    // Two modules define the same name; only the imported one connects.
    files.insert(
        repo_path("src/a/load.ts"),
        parsed(&["loadUser"], Vec::new()),
    );
    files.insert(
        repo_path("src/b/load.ts"),
        parsed(&["loadUser"], Vec::new()),
    );
    files.insert(
        repo_path("src/app.ts"),
        parsed(&["main"], vec![import("./a/load", &["loadUser"])]),
    );
    let graph = GraphSpec::new(&files).build();
    assert_eq!(
        importer_paths(&graph, "src/a/load.ts").len(),
        1,
        "imported module gains the edge"
    );
    assert_eq!(
        importer_paths(&graph, "src/b/load.ts").len(),
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
        parsed(&["main"], vec![import("external_pkg", &["helper_fn"])]),
    );
    let graph = GraphSpec::new(&files).build();
    let edges = importer_paths(&graph, "vendor/lib.rs");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].1, ContextEdgeKind::References);
    assert!(edges[0].2 < 0.8, "name-match fallback is low confidence");
}

#[test]
fn feature_slice_edges_require_local_token_overlap() {
    let changed = "apps/web/src/features/review/components/diff/description-stream-page.tsx";
    let matching = "apps/web/src/features/review/server/description/review-description.ts";
    let unrelated = "apps/web/src/features/review/server/comment-publish.ts";
    let mut files = BTreeMap::new();
    files.insert(
        repo_path(changed),
        parsed(&["DescriptionStreamPage"], Vec::new()),
    );
    files.insert(
        repo_path(matching),
        parsed(&["buildReviewDescription"], Vec::new()),
    );
    files.insert(
        repo_path(unrelated),
        parsed(&["publishReviewComment"], Vec::new()),
    );

    let graph = GraphSpec::new(&files).with_changed(&[changed]).build();
    let feature_edges = graph
        .edges()
        .filter(|edge| {
            edge.kind == ContextEdgeKind::SameModule
                && edge.reason.starts_with("same feature slice")
        })
        .collect::<Vec<_>>();
    assert!(
        feature_edges
            .iter()
            .any(|edge| connects(edge, changed, matching)),
        "shared local token 'description' creates a bounded feature-slice edge"
    );
    assert!(
        !feature_edges
            .iter()
            .any(|edge| connects(edge, changed, unrelated)),
        "same feature root alone is not enough"
    );
}

#[test]
fn resolves_python_modules() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("auth/tokens.py"), parsed(&["Token"], Vec::new()));
    files.insert(
        repo_path("app/main.py"),
        parsed(&["main"], vec![import("auth.tokens", &["Token"])]),
    );
    let graph = GraphSpec::new(&files).build();
    assert_eq!(importer_paths(&graph, "auth/tokens.py").len(), 1);
}

#[test]
fn graph_debug_export_reports_bounded_expansion_state() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/app.ts"),
        parsed(&["main"], vec![import("./db", &["getDb"])]),
    );
    files.insert(repo_path("src/db.ts"), parsed(&["getDb"], Vec::new()));
    let graph = GraphSpec::new(&files).with_changed(&["src/app.ts"]).build();
    let expansion = graph.expand(default_request());
    let export = ContextGraphDebugExport::collect_bounded(
        &graph,
        &expansion,
        ContextGraphDebugLimits {
            max_nodes: 1,
            max_edges: 1,
            max_candidates: 1,
            max_omissions: 1,
        },
    );

    assert_eq!(export.schema_version, GRAPH_DEBUG_SCHEMA_VERSION);
    assert!(export.node_count > export.nodes.len());
    assert!(export.edge_count > export.edges.len());
    assert!(!export.changed_anchors.is_empty());
    assert!(export
        .edge_confidence_by_kind
        .contains_key(&ContextEdgeKind::Imports));
    assert!(export.candidates.iter().any(|candidate| {
        candidate.path.as_deref() == Some("src/db.ts") && !candidate.steps.is_empty()
    }));
}

// ---- tsconfig aliases and barrels --------------------------------------

#[test]
fn resolves_tsconfig_path_alias_with_provenance_detail() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("apps/web/src/lib/db.ts"),
        parsed(&["getDb"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/app.ts"),
        parsed(&["main"], vec![import("@/lib/db", &["getDb"])]),
    );
    let configs = contents(&[(
        "apps/web/tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
    )]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    let edges: Vec<_> = graph
        .file_referencers(&repo_path("apps/web/src/lib/db.ts"))
        .collect();
    assert_eq!(edges.len(), 1, "alias import resolves to the defining file");
    assert_eq!(edges[0].kind, ContextEdgeKind::Imports);
    assert_eq!(
        edges[0].provenance.source,
        ContextGraphSource::ImportResolver
    );
    assert!(
        edges[0].provenance.detail.contains("alias '@/*'"),
        "provenance names the matched alias rule: {}",
        edges[0].provenance.detail
    );
}

#[test]
fn resolves_base_url_relative_bare_specifier() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("web/src/lib/db.ts"),
        parsed(&["getDb"], Vec::new()),
    );
    files.insert(
        repo_path("web/src/app.ts"),
        parsed(&["main"], vec![import("lib/db", &["getDb"])]),
    );
    let configs = contents(&[(
        "web/tsconfig.json",
        r#"{ "compilerOptions": { "baseUrl": "./src" } }"#,
    )]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    assert_eq!(importer_paths(&graph, "web/src/lib/db.ts").len(), 1);
}

#[test]
fn resolves_paths_declared_in_extended_config() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("packages/api/src/client.ts"),
        parsed(&["createClient"], Vec::new()),
    );
    files.insert(
        repo_path("packages/api/src/app.ts"),
        parsed(&["main"], vec![import("~/client", &["createClient"])]),
    );
    let configs = contents(&[
        (
            "packages/api/tsconfig.json",
            r#"{ "extends": "../../tsconfig.base" }"#,
        ),
        (
            "tsconfig.base.json",
            // Paths resolve relative to the declaring file's directory.
            r#"{ "compilerOptions": { "paths": { "~/*": ["./packages/api/src/*"] } } }"#,
        ),
    ]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    assert_eq!(
        importer_paths(&graph, "packages/api/src/client.ts").len(),
        1
    );
}

#[test]
fn tsconfig_with_comments_and_trailing_commas_parses() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/lib/db.ts"), parsed(&["getDb"], Vec::new()));
    files.insert(
        repo_path("src/app.ts"),
        parsed(&["main"], vec![import("@/lib/db", &["getDb"])]),
    );
    let configs = contents(&[(
        "tsconfig.json",
        "{\n  // path aliases\n  \"compilerOptions\": {\n    /* block */\n    \"paths\": {\n      \"@/*\": [\"./src/*\"], // trailing comment\n    },\n  },\n}\n",
    )]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    assert_eq!(importer_paths(&graph, "src/lib/db.ts").len(), 1);
}

#[test]
fn alias_matching_no_file_degrades_to_bare_name_fallback() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/other.ts"), parsed(&["getDb"], Vec::new()));
    files.insert(
        repo_path("src/app.ts"),
        parsed(&["main"], vec![import("@/missing/db", &["getDb"])]),
    );
    let configs = contents(&[(
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
    )]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    let edges = importer_paths(&graph, "src/other.ts");
    assert_eq!(edges.len(), 1, "falls back to bare-name matching");
    assert_eq!(edges[0].1, ContextEdgeKind::References);
    assert!(edges[0].2 < 0.8, "fallback edges stay low confidence");
}

#[test]
fn wildcard_barrel_reexport_connects_importer_to_defining_file() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/lib/foo.ts"), parsed(&["foo"], Vec::new()));
    files.insert(
        // Barrel: `export * from './foo'` parses to an empty-name
        // statement with the module preserved.
        repo_path("src/lib/index.ts"),
        parsed(&[], vec![import("./foo", &[])]),
    );
    files.insert(
        repo_path("src/app.ts"),
        parsed(&["main"], vec![import("./lib", &["foo"])]),
    );
    let graph = GraphSpec::new(&files).build();
    assert!(
        importer_paths(&graph, "src/lib/index.ts")
            .iter()
            .any(|(from, _, _)| *from == repo_path("src/app.ts")),
        "importer keeps the direct barrel edge"
    );
    assert!(
        importer_paths(&graph, "src/lib/foo.ts")
            .iter()
            .any(|(from, kind, _)| *from == repo_path("src/app.ts")
                && *kind == ContextEdgeKind::Imports),
        "importer also connects to the defining file behind the barrel"
    );
}

#[test]
fn named_barrel_reexport_connects_importer_to_defining_file() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/lib/bar.ts"), parsed(&["bar"], Vec::new()));
    files.insert(
        // `export { bar } from './bar'` in the barrel.
        repo_path("src/lib/index.ts"),
        parsed(&[], vec![import("./bar", &["bar"])]),
    );
    files.insert(
        repo_path("src/app.ts"),
        parsed(&["main"], vec![import("./lib", &["bar"])]),
    );
    let graph = GraphSpec::new(&files).build();
    assert!(importer_paths(&graph, "src/lib/bar.ts")
        .iter()
        .any(
            |(from, kind, _)| *from == repo_path("src/app.ts") && *kind == ContextEdgeKind::Imports
        ));
}

#[test]
fn nearest_tsconfig_wins_and_lookup_walks_upward() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("apps/web/src/local.ts"),
        parsed(&["local"], Vec::new()),
    );
    files.insert(
        repo_path("shared/util.ts"),
        parsed(&["sharedUtil"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/app.ts"),
        parsed(
            &["main"],
            vec![
                import("@/local", &["local"]),
                import("#shared/util", &["sharedUtil"]),
            ],
        ),
    );
    let configs = contents(&[
        (
            "apps/web/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        ),
        (
            "tsconfig.json",
            r##"{ "compilerOptions": { "paths": { "#shared/*": ["./shared/*"] } } }"##,
        ),
    ]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    assert_eq!(importer_paths(&graph, "apps/web/src/local.ts").len(), 1);
    assert_eq!(importer_paths(&graph, "shared/util.ts").len(), 1);
}

// ---- package.json workspace exports ------------------------------------

#[test]
fn resolves_workspace_package_through_exports_map() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("packages/contracts/src/review/index.ts"),
        parsed(&["ReviewContract"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/app.ts"),
        parsed(
            &["main"],
            vec![import("@argus/contracts/review", &["ReviewContract"])],
        ),
    );
    let configs = contents(&[(
        "packages/contracts/package.json",
        r#"{ "name": "@argus/contracts", "exports": { ".": "./src/index.ts", "./review": "./src/review/index.ts" } }"#,
    )]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    let edges: Vec<_> = graph
        .file_referencers(&repo_path("packages/contracts/src/review/index.ts"))
        .collect();
    assert_eq!(edges.len(), 1, "exports subpath resolves to defining file");
    assert_eq!(edges[0].kind, ContextEdgeKind::Imports);
    assert!(
        edges[0].confidence >= 0.9,
        "declared exports routes carry full confidence"
    );
    assert!(
        edges[0]
            .provenance
            .detail
            .contains("package '@argus/contracts'"),
        "provenance names the package route: {}",
        edges[0].provenance.detail
    );
}

#[test]
fn create_require_package_import_reaches_manifest_types() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("apps/web/src/sdk-client.ts"),
        parse_symbols(
            "apps/web/src/sdk-client.ts",
            "import { createRequire } from 'node:module';\n\
             const requireFromModule = createRequire(import.meta.url);\n\
             export const sdk = requireFromModule('@argus/search');\n",
        ),
    );
    files.insert(
        repo_path("packages/search/package.json"),
        parsed(&[], Vec::new()),
    );
    files.insert(repo_path("packages/search/sdk.js"), parsed(&[], Vec::new()));
    files.insert(
        repo_path("packages/search/types.d.ts"),
        parsed(&["SearchSdk"], Vec::new()),
    );
    let configs = contents(&[(
        "packages/search/package.json",
        r#"{ "name": "@argus/search", "main": "sdk.js", "types": "types.d.ts", "exports": { ".": { "types": "./types.d.ts", "require": "./sdk.js", "default": "./sdk.js" } } }"#,
    )]);
    let graph = GraphSpec::new(&files)
        .with_contents(configs)
        .with_changed(&["apps/web/src/sdk-client.ts"])
        .build();

    let runtime_referencers = graph
        .file_referencers(&repo_path("packages/search/sdk.js"))
        .collect::<Vec<_>>();
    assert!(runtime_referencers.iter().any(|edge| {
        edge.kind == ContextEdgeKind::Imports
            && edge.from_path() == Some(&repo_path("apps/web/src/sdk-client.ts"))
    }));
    assert!(runtime_referencers.iter().any(|edge| {
        edge.kind == ContextEdgeKind::GeneratedFrom
            && edge.from_path() == Some(&repo_path("packages/search/types.d.ts"))
    }));
    let type_referencers = graph
        .file_referencers(&repo_path("packages/search/types.d.ts"))
        .collect::<Vec<_>>();
    assert!(type_referencers.iter().any(|edge| {
        edge.kind == ContextEdgeKind::Imports
            && edge.from_path() == Some(&repo_path("apps/web/src/sdk-client.ts"))
            && edge.provenance.detail.contains("types")
    }));

    let expansion = graph.expand(default_request());
    assert!(expansion.candidates.iter().any(|candidate| {
        candidate.repo_path() == Some(&repo_path("packages/search/types.d.ts"))
            && candidate.path.steps.iter().any(|step| {
                matches!(
                    step.kind,
                    ContextEdgeKind::Imports | ContextEdgeKind::GeneratedFrom
                )
            })
    }));
}

#[test]
fn resolves_workspace_package_exports_wildcard_and_conditions() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("packages/domain/src/argus-agent/config.ts"),
        parsed(&["AgentConfig"], Vec::new()),
    );
    files.insert(
        repo_path("packages/domain/src/index.ts"),
        parsed(&["domainRoot"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/app.ts"),
        parsed(
            &["main"],
            vec![
                import("@argus/domain/argus-agent/config", &["AgentConfig"]),
                import("@argus/domain", &["domainRoot"]),
            ],
        ),
    );
    let configs = contents(&[(
        "packages/domain/package.json",
        // Root export uses a conditions object; subpaths use a wildcard.
        r#"{ "name": "@argus/domain", "exports": { ".": { "import": "./src/index.ts" }, "./*": "./src/*.ts" } }"#,
    )]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    assert_eq!(
        importer_paths(&graph, "packages/domain/src/argus-agent/config.ts").len(),
        1,
        "wildcard subpath resolves"
    );
    assert_eq!(
        importer_paths(&graph, "packages/domain/src/index.ts").len(),
        1,
        "conditional root export resolves"
    );
}

#[test]
fn workspace_package_without_exports_uses_layout_fallback() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("packages/utils/src/strings.ts"),
        parsed(&["slugify"], Vec::new()),
    );
    files.insert(
        repo_path("packages/utils/src/index.ts"),
        parsed(&["utilsRoot"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/app.ts"),
        parsed(
            &["main"],
            vec![
                import("@argus/utils/strings", &["slugify"]),
                import("@argus/utils", &["utilsRoot"]),
            ],
        ),
    );
    let configs = contents(&[(
        "packages/utils/package.json",
        r#"{ "name": "@argus/utils" }"#,
    )]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    let subpath_edges = importer_paths(&graph, "packages/utils/src/strings.ts");
    assert_eq!(
        subpath_edges.len(),
        1,
        "subpath falls back to src/<rest> layout"
    );
    assert!(
        subpath_edges[0].2 < 0.9,
        "layout-guess routes carry heuristic confidence, got {}",
        subpath_edges[0].2
    );
    assert_eq!(
        importer_paths(&graph, "packages/utils/src/index.ts").len(),
        1,
        "bare package name falls back to src/index"
    );
}

#[test]
fn unknown_package_name_still_degrades_to_name_fallback() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/local.ts"),
        parsed(&["fancyHelper"], Vec::new()),
    );
    files.insert(
        repo_path("src/app.ts"),
        parsed(
            &["main"],
            vec![import("some-external-lib", &["fancyHelper"])],
        ),
    );
    let configs = contents(&[("package.json", r#"{ "name": "my-app" }"#)]);
    let graph = GraphSpec::new(&files).with_contents(configs).build();
    let edges = importer_paths(&graph, "src/local.ts");
    assert_eq!(edges.len(), 1, "external import falls back to bare name");
    assert_eq!(edges[0].1, ContextEdgeKind::References);
}

// ---- python relative imports --------------------------------------------

#[test]
fn resolves_python_relative_imports() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("app/tokens.py"), parsed(&["Token"], Vec::new()));
    files.insert(
        repo_path("core/db.py"),
        parsed(&["get_session"], Vec::new()),
    );
    files.insert(
        repo_path("app/__init__.py"),
        parsed(&["app_root"], Vec::new()),
    );
    files.insert(
        repo_path("app/views/main.py"),
        parsed(
            &["main"],
            vec![
                // from ..tokens import Token
                import("..tokens", &["Token"]),
                // from ...core.db import get_session
                import("...core.db", &["get_session"]),
                // from .. import app_root
                import("..", &["app_root"]),
            ],
        ),
    );
    let graph = GraphSpec::new(&files).build();
    assert_eq!(
        importer_paths(&graph, "app/tokens.py").len(),
        1,
        "single-level relative module resolves"
    );
    assert_eq!(
        importer_paths(&graph, "core/db.py").len(),
        1,
        "multi-level relative dotted module resolves"
    );
    assert_eq!(
        importer_paths(&graph, "app/__init__.py").len(),
        1,
        "bare relative import resolves to package __init__"
    );
}

#[test]
fn python_relative_import_sibling_module() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("pkg/helpers.py"), parsed(&["helper"], Vec::new()));
    files.insert(
        repo_path("pkg/main.py"),
        // from .helpers import helper
        parsed(&["main"], vec![import(".helpers", &["helper"])]),
    );
    let graph = GraphSpec::new(&files).build();
    assert_eq!(importer_paths(&graph, "pkg/helpers.py").len(), 1);
}

// ---- nodes and anchors --------------------------------------------------

#[test]
fn changed_hunk_maps_to_chunk_anchor() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/core.rs"),
        parsed_with_ranges(&[("core_fn", 10, 20)], Vec::new()),
    );
    let graph = GraphSpec::new(&files)
        .with_chunks(
            "src/core.rs",
            vec![chunk(1, 9, "use x;"), chunk(10, 20, "fn core_fn() {}")],
        )
        .with_hunk("src/core.rs", 12, 14)
        .with_changed(&["src/core.rs"])
        .build();
    let anchors: Vec<_> = graph.changed_anchors().collect();
    assert_eq!(anchors.len(), 1);
    assert_eq!(
        anchors[0],
        &ContextNodeId::Chunk {
            path: repo_path("src/core.rs"),
            range: ContextRange {
                start_line: 10,
                end_line: 20
            },
        },
        "the changed hunk maps to its enclosing chunk node"
    );
}

#[test]
fn changed_file_without_chunks_anchors_at_file_node() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/core.rs"), parsed(&["core_fn"], Vec::new()));
    let graph = GraphSpec::new(&files)
        .with_changed(&["src/core.rs"])
        .build();
    let anchors: Vec<_> = graph.changed_anchors().collect();
    assert_eq!(
        anchors,
        vec![&ContextNodeId::File {
            path: repo_path("src/core.rs")
        }]
    );
}

#[test]
fn edge_ids_are_deterministic_across_builds() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/lib.rs"),
        parsed_with_ranges(&[("helper", 1, 4)], Vec::new()),
    );
    files.insert(
        repo_path("src/main.rs"),
        parsed(&["main"], vec![import("crate::lib", &["helper"])]),
    );
    let spec = GraphSpec::new(&files).with_changed(&["src/lib.rs"]);
    let first: Vec<ContextEdgeId> = spec.build().edges().map(|edge| edge.id.clone()).collect();
    let second: Vec<ContextEdgeId> = spec.build().edges().map(|edge| edge.id.clone()).collect();
    assert_eq!(first, second);
}

// ---- expansion ----------------------------------------------------------

#[test]
fn expansion_respects_candidate_budget_and_reports_omissions() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/core.rs"), parsed(&["core_fn"], Vec::new()));
    // A sibling with no import relationship surfaces only as SameModule.
    files.insert(repo_path("src/passive.rs"), parsed(&["other"], Vec::new()));
    for index in 0..6 {
        files.insert(
            repo_path(&format!("src/user{index}.rs")),
            parsed(&["use_it"], vec![import("crate::core", &["core_fn"])]),
        );
    }
    let spec = GraphSpec::new(&files).with_changed(&["src/core.rs"]);
    let graph = spec.build();
    let bounded = graph.expand(ContextGraphExpansionRequest {
        max_candidates_per_anchor: 3,
        ..default_request()
    });
    assert_eq!(bounded.candidates.len(), 3);
    let counts = bounded.omitted_counts();
    assert!(
        counts
            .get(&ContextGraphOmissionReason::BudgetExceeded)
            .copied()
            .unwrap_or(0)
            > 0,
        "dropped candidates land in omissions: {counts:?}"
    );
    let all = graph.expand(default_request());
    assert!(all.candidates.len() > 3);
    assert!(all
        .candidates
        .iter()
        .any(|candidate| candidate.relationship_kind() == ContextRelationshipKind::CalledBy));
    assert!(all
        .candidates
        .iter()
        .any(|candidate| candidate.relationship_kind() == ContextRelationshipKind::SameModule));
}

#[test]
fn expansion_returns_a_path_for_every_candidate() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/core.rs"), parsed(&["core_fn"], Vec::new()));
    files.insert(
        repo_path("src/user.rs"),
        parsed(&["use_it"], vec![import("crate::core", &["core_fn"])]),
    );
    let graph = GraphSpec::new(&files)
        .with_changed(&["src/core.rs"])
        .build();
    let expansion = graph.expand(default_request());
    assert!(!expansion.candidates.is_empty());
    for candidate in &expansion.candidates {
        assert!(
            !candidate.path.steps.is_empty(),
            "every candidate carries its graph path"
        );
        assert!(!candidate.reason().is_empty());
    }
}

#[test]
fn expansion_is_deterministic() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/core.rs"), parsed(&["core_fn"], Vec::new()));
    for index in 0..5 {
        files.insert(
            repo_path(&format!("src/user{index}.rs")),
            parsed(&["use_it"], vec![import("crate::core", &["core_fn"])]),
        );
    }
    let spec = GraphSpec::new(&files).with_changed(&["src/core.rs"]);
    let first = spec.build().expand(default_request());
    let second = spec.build().expand(default_request());
    assert_eq!(first.candidates, second.candidates);
}

#[test]
fn test_importers_surface_as_tests_relationship() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/core.rs"), parsed(&["core_fn"], Vec::new()));
    files.insert(
        repo_path("tests/core_test.rs"),
        parsed(&["core_works"], vec![import("crate::core", &["core_fn"])]),
    );
    let graph = GraphSpec::new(&files)
        .with_changed(&["src/core.rs"])
        .build();
    let expansion = graph.expand(default_request());
    assert!(expansion.candidates.iter().any(|candidate| {
        candidate.relationship_kind() == ContextRelationshipKind::Tests
            && candidate.repo_path() == Some(&repo_path("tests/core_test.rs"))
    }));
}

#[test]
fn terminal_lateral_candidates_emit_their_direct_tests() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/feature/request.ts"),
        parsed(&["parseRequest"], Vec::new()),
    );
    files.insert(
        repo_path("src/feature/route.ts"),
        parsed(&["handler"], Vec::new()),
    );
    files.insert(
        repo_path("src/feature/__tests__/route.test.ts"),
        parsed(&["routeWorks"], vec![import("../route", &["handler"])]),
    );

    let graph = GraphSpec::new(&files)
        .with_changed(&["src/feature/request.ts"])
        .build();
    let expansion = graph.expand(default_request());
    let test_candidate = expansion.candidates.iter().find(|candidate| {
        candidate.repo_path() == Some(&repo_path("src/feature/__tests__/route.test.ts"))
    });

    assert!(
        test_candidate.is_some_and(|candidate| candidate
            .path
            .steps
            .iter()
            .any(|step| step.kind == ContextEdgeKind::Tests)),
        "tests of a same-module candidate should surface without opening broad lateral traversal"
    );
}

#[test]
fn terminal_lateral_candidates_emit_convention_tests_without_imports() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/feature/request.ts"),
        parsed(&["parseRequest"], Vec::new()),
    );
    files.insert(
        repo_path("src/feature/route.ts"),
        parsed(&["handler"], Vec::new()),
    );
    files.insert(
        repo_path("src/feature/__tests__/route.test.ts"),
        parsed(&["routeWorks"], Vec::new()),
    );

    let graph = GraphSpec::new(&files)
        .with_changed(&["src/feature/request.ts"])
        .build();
    let expansion = graph.expand(default_request());
    let test_candidate = expansion.candidates.iter().find(|candidate| {
        candidate.repo_path() == Some(&repo_path("src/feature/__tests__/route.test.ts"))
    });

    assert!(
        test_candidate.is_some_and(|candidate| candidate
            .path
            .steps
            .iter()
            .any(|step| step.kind == ContextEdgeKind::Tests)),
        "convention tests of a same-module candidate should surface without imports"
    );
}

#[test]
fn large_file_contributes_its_referencing_chunk_not_all_chunks() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/lib/db.ts"),
        parsed_with_ranges(&[("getDb", 1, 10)], Vec::new()),
    );
    files.insert(
        repo_path("src/big.ts"),
        parsed(&["consume"], vec![import("./lib/db", &["getDb"])]),
    );
    let graph = GraphSpec::new(&files)
        .with_chunks(
            "src/big.ts",
            vec![
                chunk(1, 50, "import { getDb } from './lib/db';"),
                chunk(51, 100, "function unrelated() {}"),
                chunk(101, 150, "export function consume() { return getDb(); }"),
            ],
        )
        .with_chunks(
            "src/lib/db.ts",
            vec![chunk(1, 10, "export function getDb() {}")],
        )
        .with_hunk("src/lib/db.ts", 2, 3)
        .with_changed(&["src/lib/db.ts"])
        .build();
    let expansion = graph.expand(default_request());
    let big_candidates: Vec<_> = expansion
        .candidates
        .iter()
        .filter(|candidate| candidate.repo_path() == Some(&repo_path("src/big.ts")))
        .collect();
    assert_eq!(
        big_candidates.len(),
        1,
        "one candidate per file per anchor group"
    );
    match &big_candidates[0].node_id {
        ContextNodeId::Chunk { range, .. } => {
            assert!(
                range.start_line == 1 || range.start_line == 101,
                "the candidate is a referencing chunk, got {range:?}"
            );
        }
        other => panic!("expected a chunk candidate, got {other:?}"),
    }
}

#[test]
fn stem_convention_tests_surface_without_imports() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/widget.ts"), parsed(&["widget"], Vec::new()));
    files.insert(
        repo_path("src/widget.test.ts"),
        parsed(&["widgetTest"], Vec::new()),
    );
    let graph = GraphSpec::new(&files)
        .with_changed(&["src/widget.ts"])
        .build();
    let test_edges: Vec<_> = graph
        .file_referencers(&repo_path("src/widget.ts"))
        .filter(|edge| edge.kind == ContextEdgeKind::Tests)
        .collect();
    assert_eq!(test_edges.len(), 1);
    assert_eq!(
        test_edges[0].provenance.source,
        ContextGraphSource::TestConvention
    );
}

#[test]
fn changed_test_reaches_source_by_stem_convention_without_imports() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("src/widget.ts"), parsed(&["widget"], Vec::new()));
    files.insert(
        repo_path("src/widget.test.ts"),
        parsed(&["widgetTest"], Vec::new()),
    );
    let graph = GraphSpec::new(&files)
        .with_changed(&["src/widget.test.ts"])
        .build();
    let expansion = graph.expand(default_request());
    assert!(
        expansion.candidates.iter().any(|candidate| {
            candidate.relationship_kind() == ContextRelationshipKind::Tests
                && candidate.repo_path() == Some(&repo_path("src/widget.ts"))
        }),
        "test-convention edges should work when the changed file is the test"
    );
}

#[test]
fn markdown_links_create_document_edges_to_repo_paths() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("docs/billing.md"), parsed(&[], Vec::new()));
    files.insert(
        repo_path("src/billing/settlement.ts"),
        parsed(&["settle"], Vec::new()),
    );
    files.insert(
        repo_path("tests/billing/settlement.test.ts"),
        parsed(&["testSettlement"], Vec::new()),
    );
    let graph = GraphSpec::new(&files)
        .with_contents(contents(&[(
            "docs/billing.md",
            "See [settlement](../src/billing/settlement.ts) and [test][settlement-test].\n\
             Autolink: <../tests/billing/settlement.test.ts:12>.\n\n\
             [settlement-test]: ../tests/billing/settlement.test.ts#contract",
        )]))
        .with_changed(&["docs/billing.md"])
        .build();

    let edges: Vec<_> = graph
        .file_references(&repo_path("docs/billing.md"))
        .filter(|edge| edge.kind == ContextEdgeKind::Documents)
        .collect();
    let targets = edges
        .iter()
        .filter_map(|edge| edge.to_path().map(RepoPath::display))
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec![
            "src/billing/settlement.ts",
            "tests/billing/settlement.test.ts"
        ]
    );
    assert!(edges
        .iter()
        .all(|edge| edge.provenance.source == ContextGraphSource::DocumentLink));

    let expansion = graph.expand(default_request());
    assert!(expansion.candidates.iter().any(|candidate| {
        candidate.node_id.path() == Some(&repo_path("src/billing/settlement.ts"))
            && candidate
                .path
                .steps
                .iter()
                .any(|step| step.kind == ContextEdgeKind::Documents)
    }));
}

#[test]
fn rst_links_create_document_edges_to_repo_paths() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("docs/runtime.rst"), parsed(&[], Vec::new()));
    files.insert(
        repo_path("src/runtime/retry_policy.py"),
        parsed(&["retry_policy"], Vec::new()),
    );
    files.insert(
        repo_path("tests/test_retry_policy.py"),
        parsed(&["test_retry_policy"], Vec::new()),
    );
    let graph = GraphSpec::new(&files)
        .with_contents(contents(&[(
            "docs/runtime.rst",
            "See `retry policy <../src/runtime/retry_policy.py>`_.\n\
             .. _retry tests: ../tests/test_retry_policy.py",
        )]))
        .build();

    let targets = graph
        .file_references(&repo_path("docs/runtime.rst"))
        .filter(|edge| edge.kind == ContextEdgeKind::Documents)
        .filter_map(|edge| edge.to_path().map(RepoPath::display))
        .collect::<Vec<_>>();

    assert_eq!(
        targets,
        vec!["src/runtime/retry_policy.py", "tests/test_retry_policy.py"]
    );
}

#[test]
fn absolute_doc_links_resolve_only_unique_repo_suffixes() {
    let mut files = BTreeMap::new();
    files.insert(repo_path("docs/index.md"), parsed(&[], Vec::new()));
    files.insert(
        repo_path("apps/web/src/config.ts"),
        parsed(&["web"], Vec::new()),
    );
    files.insert(
        repo_path("packages/api/src/config.ts"),
        parsed(&["api"], Vec::new()),
    );
    let graph = GraphSpec::new(&files)
        .with_contents(contents(&[(
            "docs/index.md",
            "[web](/tmp/workspace/apps/web/src/config.ts) [ambiguous](/tmp/config.ts)",
        )]))
        .build();

    let targets = graph
        .file_references(&repo_path("docs/index.md"))
        .filter(|edge| edge.kind == ContextEdgeKind::Documents)
        .filter_map(|edge| edge.to_path().map(RepoPath::display))
        .collect::<Vec<_>>();

    assert_eq!(targets, vec!["apps/web/src/config.ts"]);
}

#[test]
fn next_app_layouts_configure_changed_route_leaves() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("apps/web/src/app/layout.tsx"),
        parsed(&["RootLayout"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/app/(app)/layout.tsx"),
        parsed(&["AppLayout"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/app/(app)/settings/page.tsx"),
        parsed(&["SettingsPage"], Vec::new()),
    );
    let changed = "apps/web/src/app/(app)/settings/page.tsx";
    let graph = GraphSpec::new(&files).with_changed(&[changed]).build();

    let targets = graph
        .file_referencers(&repo_path(changed))
        .filter(|edge| edge.kind == ContextEdgeKind::Configures)
        .filter_map(|edge| edge.from_path().map(RepoPath::display))
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec![
            "apps/web/src/app/(app)/layout.tsx",
            "apps/web/src/app/layout.tsx"
        ]
    );

    let expansion = graph.expand(default_request());
    assert!(expansion.candidates.iter().any(|candidate| {
        candidate.node_id.path() == Some(&repo_path("apps/web/src/app/layout.tsx"))
            && candidate.relationship_kind() == ContextRelationshipKind::Configures
    }));
}

#[test]
fn next_app_layout_edges_ignore_non_app_route_files() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("src/layout.tsx"),
        parsed(&["RootLayout"], Vec::new()),
    );
    files.insert(repo_path("src/route.ts"), parsed(&["route"], Vec::new()));
    let graph = GraphSpec::new(&files)
        .with_changed(&["src/route.ts"])
        .build();

    assert!(graph
        .file_referencers(&repo_path("src/route.ts"))
        .all(|edge| edge.kind != ContextEdgeKind::Configures));
}

#[test]
fn next_app_route_params_connect_matching_feature_files() {
    let mut files = BTreeMap::new();
    files.insert(
        repo_path("apps/web/src/app/repos/[integrationId]/_components/viewer.tsx"),
        parsed(&["RepoViewer"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/features/gitlab/server/integration-connect.ts"),
        parsed(&["connectGitlabIntegration"], Vec::new()),
    );
    files.insert(
        repo_path("apps/web/src/features/review/server/review-agent.ts"),
        parsed(&["reviewAgent"], Vec::new()),
    );
    let changed = "apps/web/src/app/repos/[integrationId]/_components/viewer.tsx";
    let graph = GraphSpec::new(&files).with_changed(&[changed]).build();

    let convention_targets = graph
        .file_referencers(&repo_path(changed))
        .filter(|edge| edge.kind == ContextEdgeKind::Convention)
        .filter_map(|edge| edge.from_path().map(RepoPath::display))
        .collect::<Vec<_>>();
    assert_eq!(
        convention_targets,
        vec!["apps/web/src/features/gitlab/server/integration-connect.ts"]
    );

    let expansion = graph.expand(default_request());
    assert!(expansion.candidates.iter().any(|candidate| {
        candidate.node_id.path()
            == Some(&repo_path(
                "apps/web/src/features/gitlab/server/integration-connect.ts",
            ))
            && candidate.hop_count == 1
    }));
}

// ---- co-change ----------------------------------------------------------

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
    let (aggregate, pairs) = co_change_facts(repo.path(), &changed, 500);
    assert_eq!(aggregate.get(&repo_path("b.rs")).map(|s| s.count), Some(3));
    assert!(aggregate.get(&repo_path("b.rs")).unwrap().weight > 0.0);
    assert_eq!(aggregate.get(&repo_path("c.rs")).map(|s| s.count), Some(2));
    assert_eq!(
        pairs
            .get(&(repo_path("a.rs"), repo_path("b.rs")))
            .map(|s| s.count),
        Some(3),
        "pairwise facts back CoChanged edges"
    );
}

#[test]
fn missing_git_history_degrades_to_empty_co_change() {
    let dir = tempfile::tempdir().unwrap();
    let changed: BTreeSet<RepoPath> = [repo_path("a.rs")].into_iter().collect();
    let (aggregate, pairs) = co_change_facts(dir.path(), &changed, 500);
    assert!(aggregate.is_empty());
    assert!(pairs.is_empty());
}
