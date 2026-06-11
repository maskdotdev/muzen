//! Symbol extraction backed by tree-sitter parsers.
//!
//! Definitions carry the full span of the defining node, not just its
//! first line, so downstream evidence can cite real ranges.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use super::chunking::{language_for_path, parse_tree};
use super::ContextRange;
use crate::runtime::contracts::RepoPath;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSymbolGraph {
    pub definitions_by_file: BTreeMap<RepoPath, Vec<String>>,
    pub imports_by_file: BTreeMap<RepoPath, Vec<String>>,
    pub importers_by_symbol: BTreeMap<String, BTreeSet<RepoPath>>,
}

impl ContextSymbolGraph {
    pub fn add_file(&mut self, path: RepoPath, content: &str) -> ParsedSymbols {
        let parsed = parse_symbols(&path.display(), content);
        if !parsed.definitions.is_empty() {
            self.definitions_by_file
                .insert(path.clone(), parsed.definitions.clone());
        }
        if !parsed.imports.is_empty() {
            self.imports_by_file
                .insert(path.clone(), parsed.imports.clone());
        }
        for import in &parsed.imports {
            self.importers_by_symbol
                .entry(import.clone())
                .or_default()
                .insert(path.clone());
        }
        parsed
    }

    pub fn file_definitions(&self, path: &RepoPath) -> impl Iterator<Item = &str> {
        self.definitions_by_file
            .get(path)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    pub fn related_importers(&self, path: &RepoPath) -> BTreeSet<RepoPath> {
        self.file_definitions(path)
            .filter_map(|definition| self.importers_by_symbol.get(definition))
            .flatten()
            .filter(|importer| *importer != path)
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedSymbols {
    pub definitions: Vec<String>,
    pub definition_ranges: BTreeMap<String, ContextRange>,
    pub imports: Vec<String>,
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
    };
    collector.walk(tree.root_node());
    let mut parsed = ParsedSymbols {
        definitions: dedupe(collector.definitions),
        definition_ranges: collector.definition_ranges,
        imports: dedupe(collector.imports),
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
                    self.walk(child);
                }
                // ---- imports ----
                "use_declaration" => self.record_rust_use(child),
                "import_statement" | "import_from_statement" => self.record_import_names(child),
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

    /// Collect every path segment and alias inside a `use` tree.
    fn record_rust_use(&mut self, node: Node) {
        self.collect_identifiers(node, &["identifier", "type_identifier"]);
    }

    /// Collect imported and aliased names from JS/TS/Python import nodes.
    fn record_import_names(&mut self, node: Node) {
        self.collect_identifiers(
            node,
            &[
                "identifier",
                "property_identifier",
                "dotted_name",
                "type_identifier",
            ],
        );
    }

    fn collect_identifiers(&mut self, node: Node, kinds: &[&str]) {
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
            self.collect_identifiers(child, kinds);
        }
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
    fn unparsed_language_yields_no_symbols() {
        let parsed = parse_symbols("notes.adoc", "some text\nmore text\n");
        assert_eq!(parsed, ParsedSymbols::default());
    }
}
