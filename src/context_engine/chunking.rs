//! AST-aligned chunking: the retrieval unit for code evidence.
//!
//! A chunk is a contiguous line span aligned to syntax-tree boundaries
//! (function, impl/class member, top-level item). Nodes larger than the
//! token limit are split into child-node chunks; adjacent small siblings
//! are merged back up to the limit (cAST-style structural chunking).
//! Languages without a parser fall back to blank-line chunking.

use std::collections::BTreeMap;

use tree_sitter::{Language, Node, Parser};

use super::ContextRange;

/// One retrieval-unit chunk of a file. Lines are 1-based and inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
    /// Enclosing symbol path, e.g. `impl ContextIndex :: fn build`.
    pub symbol_path: Option<String>,
    /// Syntax node kind the chunk is rooted at, or `text` for fallback.
    pub node_kind: String,
}

impl FileChunk {
    pub fn range(&self) -> ContextRange {
        ContextRange {
            start_line: self.start_line,
            end_line: self.end_line,
        }
    }

    pub fn token_estimate(&self) -> usize {
        estimate_tokens(self.text.len())
    }

    /// First doc/comment line of the chunk, if any.
    pub fn doc_line(&self) -> Option<&str> {
        let first = self.text.lines().next()?.trim();
        let is_doc = first.starts_with("///")
            || first.starts_with("//!")
            || first.starts_with("//")
            || first.starts_with('#')
            || first.starts_with("/*")
            || first.starts_with("\"\"\"");
        is_doc.then_some(first)
    }
}

pub(crate) fn estimate_tokens(bytes_or_chars: usize) -> usize {
    bytes_or_chars.div_ceil(4).max(1)
}

pub(crate) fn language_for_path(path: &str) -> Option<Language> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        Some(tree_sitter_rust::LANGUAGE.into())
    } else if lower.ends_with(".tsx") {
        Some(tree_sitter_typescript::LANGUAGE_TSX.into())
    } else if lower.ends_with(".ts") {
        Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") || lower.ends_with(".mjs") {
        Some(tree_sitter_javascript::LANGUAGE.into())
    } else if lower.ends_with(".py") {
        Some(tree_sitter_python::LANGUAGE.into())
    } else {
        None
    }
}

pub(crate) fn parse_tree(language: Language, content: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(content, None)
}

/// Chunk a file into AST-aligned line spans, each within `max_tokens`
/// (best effort: a single oversized line still becomes one chunk).
pub fn chunk_file(path: &str, content: &str, max_tokens: usize) -> Vec<FileChunk> {
    if content.trim().is_empty() {
        return Vec::new();
    }
    let atoms = match language_for_path(path).and_then(|language| {
        parse_tree(language, content).map(|tree| {
            let mut atoms = Vec::new();
            collect_atoms(
                tree.root_node(),
                content,
                max_tokens,
                &mut Vec::new(),
                &mut atoms,
            );
            atoms
        })
    }) {
        Some(atoms) if !atoms.is_empty() => atoms,
        _ => blank_line_atoms(content, max_tokens),
    };
    assemble_chunks(content, atoms, max_tokens)
}

/// A pre-merge chunk candidate: a line span with symbol context.
#[derive(Debug, Clone)]
struct Atom {
    start_line: u32,
    end_line: u32,
    symbol_path: Option<String>,
    node_kind: String,
}

fn collect_atoms(
    node: Node,
    content: &str,
    max_tokens: usize,
    symbol_stack: &mut Vec<String>,
    atoms: &mut Vec<Atom>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        let child_tokens = estimate_tokens(child.byte_range().len());
        let has_named_children = child.named_child_count() > 0;
        if child_tokens > max_tokens && has_named_children {
            // Too large: descend into the node's children, remembering the
            // enclosing symbol so child chunks stay attributable.
            let label = node_symbol_label(child, content);
            if let Some(label) = label.clone() {
                symbol_stack.push(label);
            }
            collect_atoms(child, content, max_tokens, symbol_stack, atoms);
            if label.is_some() {
                symbol_stack.pop();
            }
        } else {
            atoms.push(Atom {
                start_line: child.start_position().row as u32 + 1,
                end_line: child.end_position().row as u32 + 1,
                symbol_path: atom_symbol_path(child, content, symbol_stack),
                node_kind: child.kind().to_string(),
            });
        }
    }
}

fn atom_symbol_path(node: Node, content: &str, symbol_stack: &[String]) -> Option<String> {
    let own = node_symbol_label(node, content);
    if symbol_stack.is_empty() {
        return own;
    }
    let mut parts = symbol_stack.to_vec();
    if let Some(own) = own {
        parts.push(own);
    }
    Some(parts.join(" :: "))
}

/// Human-readable label for a definition-like node, e.g. `fn build`.
fn node_symbol_label(node: Node, content: &str) -> Option<String> {
    let name_field = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))
        .or_else(|| node.child_by_field_name("declarator"));
    let name = name_field.and_then(|name| content.get(name.byte_range()))?;
    let prefix = match node.kind() {
        "function_item"
        | "function_declaration"
        | "function_definition"
        | "generator_function_declaration" => "fn",
        "struct_item" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "mod_item" => "mod",
        "class_declaration" | "class_definition" => "class",
        "interface_declaration" => "interface",
        "type_alias_declaration" | "type_item" => "type",
        "method_definition" => "method",
        _ => return Some(name.to_string()),
    };
    Some(format!("{prefix} {name}"))
}

/// Fallback for unparsed languages: split on blank-line boundaries.
fn blank_line_atoms(content: &str, max_tokens: usize) -> Vec<Atom> {
    let mut atoms = Vec::new();
    let mut block_start: Option<u32> = None;
    let mut block_bytes = 0usize;
    let mut last_line = 0u32;
    for (index, line) in content.lines().enumerate() {
        let line_number = index as u32 + 1;
        last_line = line_number;
        if line.trim().is_empty() {
            if let Some(start) = block_start.take() {
                atoms.push(text_atom(start, line_number.saturating_sub(1)));
                block_bytes = 0;
            }
            continue;
        }
        if block_start.is_none() {
            block_start = Some(line_number);
        }
        block_bytes += line.len() + 1;
        if estimate_tokens(block_bytes) > max_tokens {
            if let Some(start) = block_start.take() {
                atoms.push(text_atom(start, line_number));
                block_bytes = 0;
            }
        }
    }
    if let Some(start) = block_start {
        atoms.push(text_atom(start, last_line));
    }
    atoms
}

fn text_atom(start_line: u32, end_line: u32) -> Atom {
    Atom {
        start_line,
        end_line: end_line.max(start_line),
        symbol_path: None,
        node_kind: "text".to_string(),
    }
}

/// Build final chunks: cover the whole file by extending atoms over
/// unclaimed gap lines (doc comments, attributes), then merge adjacent
/// small atoms up to the token limit.
fn assemble_chunks(content: &str, mut atoms: Vec<Atom>, max_tokens: usize) -> Vec<FileChunk> {
    let lines = content.lines().collect::<Vec<_>>();
    let total_lines = lines.len() as u32;
    if total_lines == 0 {
        return Vec::new();
    }
    atoms.sort_by_key(|atom| (atom.start_line, atom.end_line));
    atoms.retain(|atom| atom.start_line <= total_lines);

    // Claim gap lines: each atom absorbs unclaimed lines before it.
    let mut covered = 0u32; // last line claimed so far
    let mut spans: Vec<Atom> = Vec::new();
    for atom in atoms {
        if atom.end_line <= covered {
            continue; // nested/overlapping atom already covered
        }
        let start = if atom.start_line > covered + 1 {
            covered + 1
        } else {
            atom.start_line.max(covered + 1)
        };
        let end = atom.end_line.min(total_lines);
        covered = end;
        spans.push(Atom {
            start_line: start,
            end_line: end,
            symbol_path: atom.symbol_path,
            node_kind: atom.node_kind,
        });
    }
    if spans.is_empty() {
        spans.push(text_atom(1, total_lines));
        covered = total_lines;
    }
    if covered < total_lines {
        let last = spans.last_mut().expect("spans non-empty");
        last.end_line = total_lines;
    }

    // Merge adjacent small spans while the merged text stays within budget.
    let span_text =
        |start: u32, end: u32| -> String { lines[(start as usize - 1)..(end as usize)].join("\n") };
    let mut chunks: Vec<FileChunk> = Vec::new();
    for span in spans {
        let text = span_text(span.start_line, span.end_line);
        if let Some(last) = chunks.last_mut() {
            let merged_len = last.text.len() + 1 + text.len();
            if estimate_tokens(merged_len) <= max_tokens {
                last.end_line = span.end_line;
                last.text = span_text(last.start_line, span.end_line);
                if last.symbol_path != span.symbol_path {
                    last.symbol_path =
                        merge_symbol_paths(last.symbol_path.take(), span.symbol_path.clone());
                }
                continue;
            }
        }
        chunks.push(FileChunk {
            start_line: span.start_line,
            end_line: span.end_line,
            text,
            symbol_path: span.symbol_path,
            node_kind: span.node_kind,
        });
    }
    chunks
}

fn merge_symbol_paths(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => {
            const MAX_JOINED: usize = 3;
            let mut parts = left.split(", ").map(str::to_string).collect::<Vec<_>>();
            if parts.len() < MAX_JOINED && !parts.contains(&right) {
                parts.push(right);
            }
            Some(parts.join(", "))
        }
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

/// A signatures-only view of a span (R7): definitions, doc comments,
/// fields, and imports retained; function bodies elided to `...`.
/// Each retained line carries its original 1-based line number so the
/// view stays citable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkeletonView {
    pub text: String,
    pub token_estimate: usize,
    /// Number of original lines the view covers.
    pub line_count: u32,
}

/// Function-like node kinds whose bodies are elided in skeleton views.
/// Class/impl/trait/struct bodies are kept so nested signatures and
/// fields stay visible; their methods elide individually.
fn elides_body(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_declaration"
            | "function_definition"
            | "function_expression"
            | "generator_function_declaration"
            | "method_definition"
    )
}

/// Per-file map of lines elided in skeleton views: true for interior
/// body lines of function-like definitions. `None` when the file has no
/// parser or nothing elides.
pub(crate) fn body_elision_map(path: &str, content: &str) -> Option<Vec<bool>> {
    let language = language_for_path(path)?;
    let tree = parse_tree(language, content)?;
    let line_count = content.lines().count();
    if line_count == 0 {
        return None;
    }
    let mut elided = vec![false; line_count];
    mark_elided_lines(tree.root_node(), &mut elided);
    elided.iter().any(|flag| *flag).then_some(elided)
}

/// Render the skeleton view of a 1-based inclusive line range. Returns
/// `None` when the span elides nothing or when the view would not save
/// tokens over the full span (the line-number gutter has a cost).
pub(crate) fn skeleton_view(
    lines: &[&str],
    range: ContextRange,
    elided: &[bool],
) -> Option<SkeletonView> {
    let start = range.start_line.max(1) as usize - 1;
    let end = (range.end_line as usize).min(lines.len()).min(elided.len());
    if start >= end {
        return None;
    }
    if !elided[start..end].iter().any(|flag| *flag) {
        return None;
    }
    let mut text = String::new();
    let mut span_len = 0usize;
    let mut in_elision = false;
    for index in start..end {
        span_len += lines[index].len() + 1;
        if elided[index] {
            if !in_elision {
                text.push_str("     | ...\n");
                in_elision = true;
            }
        } else {
            text.push_str(&format!("{:>5}| {}\n", index + 1, lines[index]));
            in_elision = false;
        }
    }
    let text = text.trim_end().to_string();
    let token_estimate = estimate_tokens(text.len());
    if token_estimate >= estimate_tokens(span_len) {
        return None;
    }
    Some(SkeletonView {
        text,
        token_estimate,
        line_count: (end - start) as u32,
    })
}

/// Mark interior body lines of function-like nodes as elided. The
/// body's first and last lines stay (signature/opening and closing
/// delimiter for brace languages), preserving line-number anchors.
fn mark_elided_lines(node: Node, elided: &mut [bool]) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if elides_body(child.kind()) {
            if let Some(body) = child.child_by_field_name("body") {
                let start = body.start_position().row;
                let end = body.end_position().row;
                for row in (start + 1)..end.min(elided.len()) {
                    elided[row] = true;
                }
            }
            continue;
        }
        mark_elided_lines(child, elided);
    }
}

/// Parse the new-side line ranges touched by each file in a unified diff.
pub(crate) fn diff_hunk_ranges(diff: &str) -> BTreeMap<String, Vec<ContextRange>> {
    let mut ranges: BTreeMap<String, Vec<ContextRange>> = BTreeMap::new();
    let mut current_path: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.strip_prefix("b/").unwrap_or(rest).trim();
            current_path = (path != "/dev/null").then(|| path.to_string());
        } else if let Some(header) = line.strip_prefix("@@ ") {
            let Some(path) = current_path.clone() else {
                continue;
            };
            if let Some(range) = parse_hunk_new_range(header) {
                ranges.entry(path).or_default().push(range);
            }
        }
    }
    ranges
}

fn parse_hunk_new_range(header: &str) -> Option<ContextRange> {
    // header looks like: `-12,3 +14,6 @@ optional context`
    let plus = header
        .split_whitespace()
        .find(|part| part.starts_with('+'))?;
    let plus = plus.trim_start_matches('+');
    let (start, count) = match plus.split_once(',') {
        Some((start, count)) => (start.parse::<u32>().ok()?, count.parse::<u32>().ok()?),
        None => (plus.parse::<u32>().ok()?, 1),
    };
    let start = start.max(1);
    let end = start + count.saturating_sub(1).max(0);
    Some(ContextRange {
        start_line: start,
        end_line: end,
    })
}

pub(crate) fn range_overlaps(left: &ContextRange, right: &ContextRange) -> bool {
    left.start_line <= right.end_line && right.start_line <= left.end_line
}

/// Slice file content to the 1-based inclusive line range of evidence.
/// Returns the whole content when the range is absent.
pub(crate) fn slice_evidence_lines<'content>(
    content: &'content str,
    range: Option<&ContextRange>,
) -> std::borrow::Cow<'content, str> {
    let Some(range) = range else {
        return std::borrow::Cow::Borrowed(content);
    };
    let selected = content
        .lines()
        .skip(range.start_line.saturating_sub(1) as usize)
        .take((range.end_line.saturating_sub(range.start_line) as usize) + 1)
        .collect::<Vec<_>>()
        .join("\n");
    std::borrow::Cow::Owned(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_file(functions: usize, lines_per_fn: usize) -> String {
        let mut content = String::new();
        for index in 0..functions {
            content.push_str(&format!("/// Doc for f{index}\npub fn f{index}() {{\n"));
            for line in 0..lines_per_fn {
                content.push_str(&format!("    let v{line} = {line} + {index};\n"));
            }
            content.push_str("}\n\n");
        }
        content
    }

    #[test]
    fn large_rust_file_yields_function_level_chunks_within_limit() {
        let content = rust_file(50, 18); // ~1000 lines
        let chunks = chunk_file("src/big.rs", &content, 400);
        assert!(
            chunks.len() > 5,
            "expected many chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            assert!(
                chunk.token_estimate() <= 400,
                "chunk {}..{} exceeds limit: {}",
                chunk.start_line,
                chunk.end_line,
                chunk.token_estimate()
            );
            assert!(chunk.start_line >= 1 && chunk.end_line >= chunk.start_line);
        }
        // Chunks must cover the file contiguously.
        let total_lines = content.lines().count() as u32;
        assert_eq!(chunks.first().unwrap().start_line, 1);
        assert_eq!(chunks.last().unwrap().end_line, total_lines);
        for window in chunks.windows(2) {
            assert_eq!(window[1].start_line, window[0].end_line + 1);
        }
    }

    #[test]
    fn chunk_text_matches_line_range() {
        let content = rust_file(8, 10);
        let lines = content.lines().collect::<Vec<_>>();
        for chunk in chunk_file("src/lib.rs", &content, 200) {
            let expected =
                lines[(chunk.start_line as usize - 1)..(chunk.end_line as usize)].join("\n");
            assert_eq!(chunk.text, expected);
        }
    }

    #[test]
    fn oversized_impl_splits_into_member_chunks_with_symbol_path() {
        let mut content = String::from("impl Engine {\n");
        for index in 0..30 {
            content.push_str(&format!("    pub fn method{index}(&self) -> usize {{\n"));
            for line in 0..12 {
                content.push_str(&format!("        let value{line} = {line} * {index};\n"));
            }
            content.push_str("        0\n    }\n");
        }
        content.push_str("}\n");
        let chunks = chunk_file("src/engine.rs", &content, 300);
        assert!(chunks.len() > 3);
        let with_member = chunks
            .iter()
            .filter(|chunk| {
                chunk
                    .symbol_path
                    .as_deref()
                    .is_some_and(|path| path.contains("impl Engine") && path.contains("fn method"))
            })
            .count();
        assert!(with_member > 0, "expected impl member symbol paths");
    }

    #[test]
    fn unparsed_language_falls_back_to_blank_line_chunks() {
        let content = "para one line a\npara one line b\n\npara two line a\n\npara three\n";
        let chunks = chunk_file("README.adoc", content, 400);
        assert!(!chunks.is_empty());
        assert_eq!(chunks.first().unwrap().start_line, 1);
        assert_eq!(chunks.last().unwrap().end_line, 6);
    }

    #[test]
    fn small_siblings_merge_into_one_chunk() {
        let content = "pub fn a() {}\n\npub fn b() {}\n\npub fn c() {}\n";
        let chunks = chunk_file("src/small.rs", &content, 400);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 5);
    }

    #[test]
    fn doc_line_detected() {
        let content = "/// Validates the token expiry.\npub fn validate() {}\n";
        let chunks = chunk_file("src/auth.rs", content, 400);
        assert_eq!(
            chunks[0].doc_line(),
            Some("/// Validates the token expiry.")
        );
    }

    #[test]
    fn diff_hunk_ranges_parse_new_side_spans() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -10,3 +12,5 @@ fn ctx()\n context\n+added\n@@ -40 +50 @@\n+more\ndiff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1,3 +0,0 @@\n";
        let ranges = diff_hunk_ranges(diff);
        assert_eq!(
            ranges.get("src/a.rs"),
            Some(&vec![
                ContextRange {
                    start_line: 12,
                    end_line: 16
                },
                ContextRange {
                    start_line: 50,
                    end_line: 50
                },
            ])
        );
        assert!(!ranges.contains_key("/dev/null"));
        assert_eq!(ranges.len(), 1);
    }

    fn skeleton_of(path: &str, content: &str, range: Option<ContextRange>) -> Option<SkeletonView> {
        let elided = body_elision_map(path, content)?;
        let lines = content.lines().collect::<Vec<_>>();
        let range = range.unwrap_or(ContextRange {
            start_line: 1,
            end_line: lines.len() as u32,
        });
        skeleton_view(&lines, range, &elided)
    }

    #[test]
    fn skeleton_elides_function_bodies_and_preserves_line_numbers() {
        let content = rust_file(10, 20);
        let skeleton = skeleton_of("src/big.rs", &content, None).expect("skeleton view");
        assert!(skeleton.token_estimate < estimate_tokens(content.len()));
        assert_eq!(skeleton.line_count, content.lines().count() as u32);
        // Signatures and docs survive with their original line numbers.
        assert!(skeleton.text.contains("    1| /// Doc for f0"));
        assert!(skeleton.text.contains("    2| pub fn f0() {"));
        assert!(skeleton.text.contains("     | ..."));
        // Bodies are gone.
        assert!(!skeleton.text.contains("let v0"));
    }

    #[test]
    fn skeleton_of_chunk_range_keeps_original_line_numbers() {
        let content = rust_file(10, 20);
        // A mid-file span: each function block spans 24 lines, so the
        // second function's doc comment sits at line 25.
        let skeleton = skeleton_of(
            "src/big.rs",
            &content,
            Some(ContextRange {
                start_line: 25,
                end_line: 47,
            }),
        )
        .expect("span skeleton");
        assert!(skeleton.text.contains("   25| /// Doc for f1"));
        assert!(!skeleton.text.contains("    1| "));
    }

    #[test]
    fn skeleton_keeps_impl_member_signatures() {
        let mut content = String::from("impl Engine {\n");
        for index in 0..5 {
            content.push_str(&format!("    pub fn method{index}(&self) -> usize {{\n"));
            for line in 0..15 {
                content.push_str(&format!("        let value{line} = {line} * {index};\n"));
            }
            content.push_str("        0\n    }\n");
        }
        content.push_str("}\n");
        let skeleton = skeleton_of("src/engine.rs", &content, None).expect("skeleton view");
        for index in 0..5 {
            assert!(
                skeleton
                    .text
                    .contains(&format!("pub fn method{index}(&self) -> usize {{")),
                "method{index} signature retained"
            );
        }
        assert!(!skeleton.text.contains("let value0"));
    }

    #[test]
    fn skeleton_absent_when_it_saves_nothing() {
        assert!(skeleton_of("src/tiny.rs", "pub fn a() {}\n", None).is_none());
        assert!(skeleton_of("README.adoc", "no parser here\n", None).is_none());
        assert!(skeleton_of("src/empty.rs", "", None).is_none());
    }

    #[test]
    fn chunks_are_stable_across_runs() {
        let content = rust_file(20, 15);
        let first = chunk_file("src/stable.rs", &content, 400);
        let second = chunk_file("src/stable.rs", &content, 400);
        assert_eq!(first, second);
    }
}
