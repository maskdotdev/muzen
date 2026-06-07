use std::collections::{BTreeMap, BTreeSet};

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
    pub imports: Vec<String>,
}

pub fn parse_symbols(path: &str, content: &str) -> ParsedSymbols {
    let lower = path.to_ascii_lowercase();
    let mut parsed = if lower.ends_with(".rs") {
        parse_rust_symbols(content)
    } else if lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
    {
        parse_typescript_symbols(content)
    } else if lower.ends_with(".py") {
        parse_python_symbols(content)
    } else {
        ParsedSymbols::default()
    };
    parsed.definitions = dedupe(parsed.definitions);
    parsed.imports = dedupe(parsed.imports);
    parsed
}

fn parse_rust_symbols(content: &str) -> ParsedSymbols {
    let mut definitions = Vec::new();
    let mut imports = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        let tokens = lexical_tokens(line);
        for (index, token) in tokens.iter().enumerate() {
            if matches!(
                token.as_str(),
                "fn" | "struct" | "enum" | "trait" | "type" | "const" | "static" | "mod"
            ) {
                if let Some(name) = tokens.get(index + 1).and_then(|token| symbol_name(token)) {
                    definitions.push(name);
                }
            }
        }
        if let Some(rest) = line
            .strip_prefix("use ")
            .or_else(|| line.strip_prefix("pub use "))
        {
            imports.extend(parse_rust_use(rest));
        }
    }
    ParsedSymbols {
        definitions,
        imports,
    }
}

fn parse_typescript_symbols(content: &str) -> ParsedSymbols {
    let mut definitions = Vec::new();
    let mut imports = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        let tokens = lexical_tokens(line);
        for (index, token) in tokens.iter().enumerate() {
            if matches!(
                token.as_str(),
                "function" | "class" | "interface" | "type" | "enum" | "const" | "let" | "var"
            ) {
                if let Some(name) = tokens.get(index + 1).and_then(|token| symbol_name(token)) {
                    definitions.push(name);
                }
            }
        }
        if line.starts_with("import ") || line.starts_with("export ") {
            imports.extend(parse_typescript_imports(line));
        }
        if let Some(method) = parse_typescript_method_definition(line) {
            definitions.push(method);
        }
    }
    ParsedSymbols {
        definitions,
        imports,
    }
}

fn parse_python_symbols(content: &str) -> ParsedSymbols {
    let mut definitions = Vec::new();
    let mut imports = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        let tokens = lexical_tokens(line);
        if let Some(first) = tokens.first() {
            if matches!(first.as_str(), "def" | "class") {
                if let Some(name) = tokens.get(1).and_then(|token| symbol_name(token)) {
                    definitions.push(name);
                }
            } else if first == "import" {
                imports.extend(tokens.iter().skip(1).filter_map(|token| symbol_name(token)));
            } else if first == "from" {
                if let Some(import_index) = tokens.iter().position(|token| token == "import") {
                    imports.extend(
                        tokens
                            .iter()
                            .skip(import_index + 1)
                            .filter_map(|token| symbol_name(token)),
                    );
                }
            }
        }
    }
    ParsedSymbols {
        definitions,
        imports,
    }
}

fn parse_rust_use(rest: &str) -> Vec<String> {
    let trimmed = rest.trim_end_matches(';');
    if trimmed.contains('{') {
        return parse_symbol_names(trimmed);
    }
    if let Some((_, tail)) = trimmed.rsplit_once("::") {
        return parse_symbol_names(tail)
            .into_iter()
            .chain(symbol_name(tail))
            .collect();
    }
    parse_symbol_names(trimmed)
        .into_iter()
        .chain(symbol_name(trimmed))
        .collect()
}

fn parse_typescript_imports(line: &str) -> Vec<String> {
    let before_from = line
        .split_once(" from ")
        .map(|(head, _)| head)
        .unwrap_or(line);
    let selected = before_from
        .strip_prefix("import ")
        .or_else(|| before_from.strip_prefix("export "))
        .unwrap_or(before_from);
    parse_symbol_names(selected)
}

fn parse_typescript_method_definition(line: &str) -> Option<String> {
    if line.starts_with("function ")
        || line.starts_with("if ")
        || line.starts_with("for ")
        || line.starts_with("while ")
        || line.starts_with("switch ")
        || line.starts_with("catch ")
        || line.starts_with("return ")
        || line.starts_with("export ")
        || line.starts_with("import ")
        || line.contains("=>")
        || line.contains('=')
        || line.contains('.')
    {
        return None;
    }
    let (candidate, _) = line.split_once('(')?;
    let name = candidate.split_whitespace().last().and_then(symbol_name)?;
    (!is_typescript_method_modifier(&name)).then_some(name)
}

fn parse_symbol_names(text: &str) -> Vec<String> {
    let selected = if text.contains('{') {
        text.chars()
            .map(|ch| if matches!(ch, '{' | '}') { ',' } else { ch })
            .collect::<String>()
    } else {
        text.to_string()
    };
    lexical_tokens(&selected)
        .into_iter()
        .filter(|token| {
            !matches!(
                token.as_str(),
                "as" | "from" | "import" | "export" | "default"
            )
        })
        .filter_map(|token| symbol_name(&token))
        .collect()
}

fn is_typescript_method_modifier(token: &str) -> bool {
    matches!(
        token,
        "abstract" | "async" | "private" | "protected" | "public" | "readonly" | "static"
    )
}

fn lexical_tokens(line: &str) -> Vec<String> {
    line.split(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn symbol_name(token: &str) -> Option<String> {
    let first = token.as_bytes().first()?;
    if !first.is_ascii_alphabetic() && *first != b'_' {
        return None;
    }
    (!is_keyword(token)).then(|| token.to_string())
}

fn is_keyword(token: &str) -> bool {
    matches!(
        token,
        "as" | "async"
            | "await"
            | "crate"
            | "default"
            | "export"
            | "from"
            | "import"
            | "let"
            | "mut"
            | "pub"
            | "self"
            | "Self"
            | "super"
            | "use"
            | "var"
    )
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
        assert!(parsed.imports.contains(&"Token".to_string()));
        assert!(parsed.imports.contains(&"authorize_request".to_string()));
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
}
