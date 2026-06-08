use super::{ContextEvidence, ContextEvidenceKind, ContextSymbolGraph};

pub(crate) fn path_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_string()
}

pub(crate) fn related_symbol_terms(
    evidence: &[ContextEvidence],
    path: &str,
    explicit_symbol: Option<String>,
) -> Vec<String> {
    let mut terms = std::collections::BTreeSet::new();
    if let Some(symbol) = explicit_symbol.filter(|symbol| !symbol.is_empty()) {
        terms.insert(symbol);
    }
    let stem = path_stem(path);
    if !stem.is_empty() {
        terms.insert(stem);
    }
    for candidate in evidence
        .iter()
        .filter(|candidate| candidate.kind == ContextEvidenceKind::Symbol)
        .filter(|candidate| {
            candidate
                .path
                .as_ref()
                .map(|candidate_path| candidate_path.display() == path)
                .unwrap_or(false)
        })
    {
        if let Some(summary) = &candidate.summary {
            if let Some(symbol) = summary
                .strip_prefix("symbol ")
                .and_then(|rest| rest.split_once(" in "))
                .map(|(symbol, _)| symbol)
                .filter(|symbol| !symbol.is_empty())
            {
                terms.insert(symbol.to_string());
            }
        }
    }
    terms.into_iter().collect()
}

pub(crate) fn related_symbol_score(
    evidence: &ContextEvidence,
    file_contents: &std::collections::BTreeMap<crate::runtime::contracts::RepoPath, String>,
    symbol_graph: &ContextSymbolGraph,
    path: &str,
    terms: &[String],
) -> Option<usize> {
    let evidence_path = evidence.path.as_ref()?;
    let evidence_path_text = evidence_path.display();
    if evidence_path_text == path {
        return Some(100);
    }
    let mut score = 0usize;
    if let Ok(query_path) = crate::runtime::contracts::RepoPath::parse(path) {
        if symbol_graph
            .related_importers(&query_path)
            .contains(evidence_path)
        {
            score = score.saturating_add(90);
        }
    }
    let summary = evidence
        .summary
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let content = file_contents
        .get(evidence_path)
        .map(|content| content.to_ascii_lowercase())
        .unwrap_or_default();
    for term in terms {
        let term = term.to_ascii_lowercase();
        if term.is_empty() {
            continue;
        }
        if summary.contains(&term) {
            score = score.saturating_add(60);
        }
        if content.contains(&term) {
            score = score.saturating_add(35);
        }
    }
    let import_hint = import_hint(path).to_ascii_lowercase();
    if !import_hint.is_empty() && content.contains(&import_hint) {
        score = score.saturating_add(45);
    }
    (score > 0).then_some(score)
}

fn import_hint(path: &str) -> String {
    path.strip_suffix(".rs")
        .or_else(|| path.strip_suffix(".ts"))
        .or_else(|| path.strip_suffix(".tsx"))
        .or_else(|| path.strip_suffix(".js"))
        .or_else(|| path.strip_suffix(".jsx"))
        .or_else(|| path.strip_suffix(".py"))
        .unwrap_or(path)
        .replace(['/', '\\'], "::")
}
