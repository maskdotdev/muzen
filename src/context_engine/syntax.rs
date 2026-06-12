//! Symbol extraction backed by tree-sitter parsers.
//!
//! Definitions carry the full span of the defining node, not just its
//! first line, so downstream evidence can cite real ranges.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

use super::chunking::{language_for_path, parse_tree};
use super::ContextRange;
use crate::runtime::contracts::RepoPath;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSymbolGraph {
    pub definitions_by_file: BTreeMap<RepoPath, Vec<String>>,
    pub imports_by_file: BTreeMap<RepoPath, Vec<String>>,
}

impl ContextSymbolGraph {
    pub fn add_file(&mut self, path: RepoPath, content: &str) -> ParsedSymbols {
        let parsed = parse_symbols(&path.display(), content);
        self.add_parsed(path, &parsed);
        parsed
    }

    /// Register already-parsed symbols (R9: cached derived data) without
    /// re-parsing the file.
    pub fn add_parsed(&mut self, path: RepoPath, parsed: &ParsedSymbols) {
        if !parsed.definitions.is_empty() {
            self.definitions_by_file
                .insert(path.clone(), parsed.definitions.clone());
        }
        if !parsed.imports.is_empty() {
            self.imports_by_file.insert(path, parsed.imports.clone());
        }
    }

    pub fn file_definitions(&self, path: &RepoPath) -> impl Iterator<Item = &str> {
        self.definitions_by_file
            .get(path)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSymbols {
    pub definitions: Vec<String>,
    pub definition_ranges: BTreeMap<String, ContextRange>,
    pub imports: Vec<String>,
    /// Imports with their module specifier preserved, for scoped
    /// resolution to defining files (R4 graph expansion).
    pub import_statements: Vec<ImportStatement>,
}

/// One import statement: the module it names and the symbols it binds.
/// `module` is the language-level specifier (`crate::auth::token`,
/// `./api`, `auth.tokens`); `None` when the statement has no resolvable
/// specifier.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportStatement {
    pub module: Option<String>,
    pub names: Vec<String>,
}

pub fn parse_symbols(path: &str, content: &str) -> ParsedSymbols {
    let Some(language) = language_for_path(path) else {
        return ParsedSymbols::default();
    };
    let Some(tree) = parse_tree(language, content) else {
        return ParsedSymbols::default();
    };
    let mut collector = SymbolCollector {
        content,
        definitions: Vec::new(),
        definition_ranges: BTreeMap::new(),
        imports: Vec::new(),
        import_statements: Vec::new(),
        require_like_bindings: BTreeSet::new(),
    };
    collector.walk(tree.root_node());
    let mut parsed = ParsedSymbols {
        definitions: dedupe(collector.definitions),
        definition_ranges: collector.definition_ranges,
        imports: dedupe(collector.imports),
        import_statements: collector.import_statements,
    };
    parsed
        .definition_ranges
        .retain(|definition, _| parsed.definitions.contains(definition));
    parsed
}

struct SymbolCollector<'content> {
    content: &'content str,
    definitions: Vec<String>,
    definition_ranges: BTreeMap<String, ContextRange>,
    imports: Vec<String>,
    import_statements: Vec<ImportStatement>,
    require_like_bindings: BTreeSet<String>,
}

impl SymbolCollector<'_> {
    fn walk(&mut self, node: Node) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if !child.is_named() {
                continue;
            }
            match child.kind() {
                // ---- definitions ----
                "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "mod_item"
                | "function_signature_item"
                | "union_item"
                | "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "method_definition"
                | "method_signature"
                | "function_definition"
                | "class_definition" => {
                    self.record_definition(child);
                    self.walk(child); // nested definitions (impl methods, inner fns)
                }
                "variable_declarator" => {
                    // const/let/var NAME = ...
                    self.record_definition(child);
                    self.record_create_require_binding(child);
                    self.walk(child);
                }
                // ---- imports ----
                "use_declaration" => self.record_rust_use(child),
                "import_statement" | "import_from_statement" => self.record_import_names(child),
                "call_expression" => {
                    self.record_require_call(child);
                    self.walk(child);
                }
                "export_statement" => {
                    // re-exports (`export { x } from './y'`) act as imports
                    if child.child_by_field_name("source").is_some() {
                        self.record_import_names(child);
                    }
                    self.walk(child);
                }
                _ => self.walk(child),
            }
        }
    }

    fn record_definition(&mut self, node: Node) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let Some(name) = self.content.get(name_node.byte_range()) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        self.definition_ranges
            .insert(name.to_string(), node_range(node));
        self.definitions.push(name.to_string());
    }

    /// Collect every path segment and alias inside a `use` tree, plus
    /// structured `(module, names)` records for scoped resolution.
    fn record_rust_use(&mut self, node: Node) {
        self.collect_identifiers(node, &["identifier", "type_identifier"], None);
        if let Some(argument) = node.child_by_field_name("argument") {
            self.collect_rust_use_tree(argument, "");
        }
    }

    fn collect_rust_use_tree(&mut self, node: Node, prefix: &str) {
        match node.kind() {
            "identifier" | "type_identifier" => {
                let Some(name) = self.content.get(node.byte_range()) else {
                    return;
                };
                self.import_statements.push(ImportStatement {
                    module: (!prefix.is_empty()).then(|| prefix.to_string()),
                    names: vec![name.to_string()],
                });
            }
            "scoped_identifier" => {
                let Some(text) = self.content.get(node.byte_range()) else {
                    return;
                };
                let (module, name) = split_rust_path(&join_rust_path(prefix, text));
                self.import_statements.push(ImportStatement {
                    module,
                    names: vec![name],
                });
            }
            "scoped_use_list" => {
                let path_text = node
                    .child_by_field_name("path")
                    .and_then(|path| self.content.get(path.byte_range()))
                    .unwrap_or("");
                let joined = join_rust_path(prefix, path_text);
                if let Some(list) = node.child_by_field_name("list") {
                    self.collect_rust_use_tree(list, &joined);
                }
            }
            "use_list" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.is_named() {
                        self.collect_rust_use_tree(child, prefix);
                    }
                }
            }
            "use_as_clause" => {
                if let Some(path) = node.child_by_field_name("path") {
                    self.collect_rust_use_tree(path, prefix);
                }
            }
            "use_wildcard" => {
                let module = node
                    .named_child(0)
                    .and_then(|child| self.content.get(child.byte_range()))
                    .map(|text| join_rust_path(prefix, text))
                    .unwrap_or_else(|| prefix.to_string());
                if !module.is_empty() {
                    self.import_statements.push(ImportStatement {
                        module: Some(module),
                        names: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Collect imported and aliased names from JS/TS/Python import nodes,
    /// plus structured `(module, names)` records for scoped resolution.
    fn record_import_names(&mut self, node: Node) {
        let kinds: &[&str] = &[
            "identifier",
            "property_identifier",
            "dotted_name",
            "type_identifier",
        ];
        // TS/JS `import ... from "./x"` / Python `from a.b import ...`.
        let module_node = node
            .child_by_field_name("source")
            .or_else(|| node.child_by_field_name("module_name"));
        self.collect_identifiers(node, kinds, None);
        if let Some(module_node) = module_node {
            let module = self
                .content
                .get(module_node.byte_range())
                .map(|text| text.trim_matches(['"', '\'']).to_string());
            let before = self.import_statements.len();
            let mut names = Vec::new();
            collect_statement_names(
                node,
                self.content,
                kinds,
                Some(module_node.byte_range()),
                &mut names,
            );
            debug_assert_eq!(before, self.import_statements.len());
            self.import_statements.push(ImportStatement {
                module: module.filter(|module| !module.is_empty()),
                names,
            });
        } else {
            // Python `import a.b`: each name child is a module reference.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                let module_text = match child.kind() {
                    "dotted_name" => self.content.get(child.byte_range()),
                    "aliased_import" => child
                        .child_by_field_name("name")
                        .and_then(|name| self.content.get(name.byte_range())),
                    _ => None,
                };
                if let Some(module_text) = module_text {
                    self.import_statements.push(ImportStatement {
                        module: Some(module_text.to_string()),
                        names: Vec::new(),
                    });
                }
            }
        }
    }

    fn record_create_require_binding(&mut self, node: Node) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let Some(value_node) = node.child_by_field_name("value") else {
            return;
        };
        if value_node.kind() != "call_expression" {
            return;
        }
        let Some(callee) = call_callee_text(value_node, self.content) else {
            return;
        };
        if callee == "createRequire" || callee.ends_with(".createRequire") {
            if let Some(name) = self.content.get(name_node.byte_range()) {
                self.require_like_bindings.insert(name.to_string());
            }
        }
    }

    fn record_require_call(&mut self, node: Node) {
        let Some(callee) = call_callee_text(node, self.content) else {
            return;
        };
        if callee != "require" && !self.require_like_bindings.contains(callee) {
            return;
        }
        let Some(module) = first_string_call_argument(node, self.content) else {
            return;
        };
        if module.is_empty() {
            return;
        }
        self.import_statements.push(ImportStatement {
            module: Some(module),
            names: Vec::new(),
        });
    }

    fn collect_identifiers(
        &mut self,
        node: Node,
        kinds: &[&str],
        skip: Option<&std::ops::Range<usize>>,
    ) {
        if let Some(skip) = skip {
            if node.byte_range() == *skip {
                return;
            }
        }
        if kinds.contains(&node.kind()) {
            if let Some(text) = self.content.get(node.byte_range()) {
                if node.kind() == "dotted_name" {
                    self.imports.extend(
                        text.split('.')
                            .map(str::to_string)
                            .filter(|part| !part.is_empty()),
                    );
                } else if !is_path_keyword(text) {
                    self.imports.push(text.to_string());
                }
            }
            // dotted_name still has identifier children; stop here either way
            if node.kind() == "dotted_name" {
                return;
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_identifiers(child, kinds, skip);
        }
    }
}

fn call_callee_text<'content>(node: Node, content: &'content str) -> Option<&'content str> {
    node.child_by_field_name("function")
        .and_then(|callee| content.get(callee.byte_range()))
        .map(str::trim)
}

fn first_string_call_argument(node: Node, content: &str) -> Option<String> {
    let mut call_cursor = node.walk();
    let arguments = node.child_by_field_name("arguments").or_else(|| {
        node.named_children(&mut call_cursor)
            .find(|child| child.kind() == "arguments")
    })?;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() != "string" {
            continue;
        }
        let Some(raw) = content.get(child.byte_range()).map(str::trim) else {
            continue;
        };
        let stripped = raw
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                raw.strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            });
        if let Some(value) = stripped {
            return Some(value.to_string());
        }
    }
    None
}

/// Collect imported names within a statement, skipping the module
/// specifier subtree so module path segments are not mistaken for names.
fn collect_statement_names(
    node: Node,
    content: &str,
    kinds: &[&str],
    skip: Option<std::ops::Range<usize>>,
    names: &mut Vec<String>,
) {
    if let Some(skip) = &skip {
        if node.byte_range() == *skip {
            return;
        }
    }
    if kinds.contains(&node.kind()) {
        if let Some(text) = content.get(node.byte_range()) {
            if !is_path_keyword(text) && !text.is_empty() {
                names.push(text.to_string());
            }
        }
        if node.kind() == "dotted_name" {
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_statement_names(child, content, kinds, skip.clone(), names);
    }
}

fn join_rust_path(prefix: &str, rest: &str) -> String {
    if prefix.is_empty() {
        rest.to_string()
    } else {
        format!("{prefix}::{rest}")
    }
}

fn split_rust_path(path: &str) -> (Option<String>, String) {
    match path.rsplit_once("::") {
        Some((module, name)) => (Some(module.to_string()), name.to_string()),
        None => (None, path.to_string()),
    }
}

fn node_range(node: Node) -> ContextRange {
    ContextRange {
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    }
}

fn is_path_keyword(text: &str) -> bool {
    matches!(text, "crate" | "self" | "super" | "Self")
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rust_definitions_and_imports() {
        let parsed = parse_symbols(
            "src/auth/token.rs",
            "use crate::auth::token::{authorize_request, Token};\npub struct Session {}\npub fn validate() {}\n",
        );
        assert_eq!(parsed.definitions, vec!["Session", "validate"]);
        assert_eq!(
            parsed.definition_ranges.get("validate"),
            Some(&ContextRange {
                start_line: 3,
                end_line: 3,
            })
        );
        assert!(parsed.imports.contains(&"Token".to_string()));
        assert!(parsed.imports.contains(&"authorize_request".to_string()));
    }

    #[test]
    fn definition_ranges_span_whole_bodies() {
        let parsed = parse_symbols(
            "src/lib.rs",
            "pub fn long() {\n    let a = 1;\n    let b = 2;\n}\n",
        );
        assert_eq!(
            parsed.definition_ranges.get("long"),
            Some(&ContextRange {
                start_line: 1,
                end_line: 4,
            })
        );
    }

    #[test]
    fn parses_re_exports_aliases_nested_imports_and_methods() {
        let rust = parse_symbols(
            "src/lib.rs",
            "pub use crate::{auth::{Token as AuthToken, authorize_request}, db::Pool};\nimpl Service { pub fn refresh_token(&self) {} }\n",
        );
        assert!(rust.definitions.contains(&"refresh_token".to_string()));
        assert!(rust.imports.contains(&"Token".to_string()));
        assert!(rust.imports.contains(&"AuthToken".to_string()));
        assert!(rust.imports.contains(&"authorize_request".to_string()));
        assert!(rust.imports.contains(&"Pool".to_string()));

        let ts = parse_symbols(
            "src/user.ts",
            "import UserClient, { loadUser as fetchUser } from './api';\nexport { saveUser as persistUser } from './save';\nclass Store {\n  reloadUser(id: string) { return id; }\n}\n",
        );
        assert!(ts.definitions.contains(&"reloadUser".to_string()));
        assert_eq!(
            ts.definition_ranges.get("reloadUser"),
            Some(&ContextRange {
                start_line: 4,
                end_line: 4,
            })
        );
        assert!(ts.imports.contains(&"UserClient".to_string()));
        assert!(ts.imports.contains(&"loadUser".to_string()));
        assert!(ts.imports.contains(&"fetchUser".to_string()));
        assert!(ts.imports.contains(&"saveUser".to_string()));
        assert!(ts.imports.contains(&"persistUser".to_string()));
    }

    #[test]
    fn parses_typescript_and_python_shapes() {
        let ts = parse_symbols(
            "src/user.ts",
            "import { loadUser } from './api';\nexport class UserStore {}\nconst activeUser = true;\n",
        );
        assert!(ts.definitions.contains(&"UserStore".to_string()));
        assert!(ts.definitions.contains(&"activeUser".to_string()));
        assert!(ts.imports.contains(&"loadUser".to_string()));

        let py = parse_symbols(
            "app/auth.py",
            "from auth.tokens import Token, authorize_request\nclass AuthService:\n    pass\ndef check_user():\n    pass\n",
        );
        assert!(py.definitions.contains(&"AuthService".to_string()));
        assert!(py.definitions.contains(&"check_user".to_string()));
        assert!(py.imports.contains(&"Token".to_string()));
    }

    #[test]
    fn captures_structured_import_statements_with_modules() {
        let rust = parse_symbols(
            "src/lib.rs",
            "use crate::auth::token::{authorize_request, Token as AuthToken};\nuse crate::db::*;\nuse serde::Serialize;\n",
        );
        assert!(rust.import_statements.contains(&ImportStatement {
            module: Some("crate::auth::token".to_string()),
            names: vec!["authorize_request".to_string()],
        }));
        assert!(rust.import_statements.contains(&ImportStatement {
            module: Some("crate::auth::token".to_string()),
            names: vec!["Token".to_string()],
        }));
        assert!(rust.import_statements.contains(&ImportStatement {
            module: Some("crate::db".to_string()),
            names: Vec::new(),
        }));
        assert!(rust.import_statements.contains(&ImportStatement {
            module: Some("serde".to_string()),
            names: vec!["Serialize".to_string()],
        }));

        let ts = parse_symbols(
            "src/user.ts",
            "import UserClient, { loadUser } from './api';\n",
        );
        assert_eq!(ts.import_statements.len(), 1);
        assert_eq!(ts.import_statements[0].module.as_deref(), Some("./api"));
        assert!(ts.import_statements[0]
            .names
            .contains(&"UserClient".to_string()));
        assert!(ts.import_statements[0]
            .names
            .contains(&"loadUser".to_string()));

        let py = parse_symbols(
            "app/auth.py",
            "from auth.tokens import Token\nimport os.path\n",
        );
        assert!(py.import_statements.contains(&ImportStatement {
            module: Some("auth.tokens".to_string()),
            names: vec!["Token".to_string()],
        }));
        assert!(py.import_statements.contains(&ImportStatement {
            module: Some("os.path".to_string()),
            names: Vec::new(),
        }));
    }

    #[test]
    fn captures_node_require_module_specifiers() {
        let ts = parse_symbols(
            "src/sdk.ts",
            "import { createRequire } from 'node:module';\n\
             const requireFromModule = createRequire(import.meta.url);\n\
             const sdk = requireFromModule('@argus/argus-search');\n\
             const local = require('./local');\n\
             const dynamic = requireFromModule(packageName);\n",
        );

        assert!(ts.import_statements.contains(&ImportStatement {
            module: Some("node:module".to_string()),
            names: vec!["createRequire".to_string()],
        }));
        assert!(ts.import_statements.contains(&ImportStatement {
            module: Some("@argus/argus-search".to_string()),
            names: Vec::new(),
        }));
        assert!(ts.import_statements.contains(&ImportStatement {
            module: Some("./local".to_string()),
            names: Vec::new(),
        }));
        assert!(!ts
            .import_statements
            .iter()
            .any(|statement| { statement.module.as_deref() == Some("packageName") }));
    }

    #[test]
    fn unparsed_language_yields_no_symbols() {
        let parsed = parse_symbols("notes.adoc", "some text\nmore text\n");
        assert_eq!(parsed, ParsedSymbols::default());
    }
}
