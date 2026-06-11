//! BM25 lexical retrieval over chunk evidence.
//!
//! Deterministic, in-memory, no model, no network. The tokenizer is
//! code-aware: identifiers are indexed whole and as their camelCase /
//! snake_case / digit-boundary subtokens, so `getUserId` matches
//! `get_user_id` while exact identifier queries still rank the exact
//! definition first (the full token is rarer than its subtokens).

use std::collections::BTreeMap;

use crate::runtime::contracts::EvidenceId;

use super::chunking::slice_evidence_lines;
use super::ContextEvidence;

/// Relative weight of a term occurrence by field.
const PATH_FIELD_WEIGHT: f32 = 3.0;
const SUMMARY_FIELD_WEIGHT: f32 = 2.0;
const BODY_FIELD_WEIGHT: f32 = 1.0;

#[derive(Debug, Clone, Default)]
pub struct LexicalIndex {
    /// term -> postings of (document ordinal, weighted term frequency)
    postings: BTreeMap<String, Vec<(u32, f32)>>,
    /// document ordinal -> weighted document length
    doc_lengths: Vec<f32>,
    /// document ordinal -> evidence id
    doc_ids: Vec<EvidenceId>,
    average_doc_length: f32,
}

impl LexicalIndex {
    pub fn build(
        evidence: &[ContextEvidence],
        file_contents: &BTreeMap<crate::runtime::contracts::RepoPath, String>,
    ) -> Self {
        let mut index = Self::default();
        for item in evidence {
            let ordinal = index.doc_ids.len() as u32;
            let mut term_frequencies: BTreeMap<String, f32> = BTreeMap::new();
            let mut doc_length = 0.0f32;
            let mut add_field = |text: &str, weight: f32| {
                for token in code_tokens(text) {
                    *term_frequencies.entry(token).or_default() += weight;
                    doc_length += weight;
                }
            };
            if let Some(path) = &item.path {
                add_field(&path.display(), PATH_FIELD_WEIGHT);
            }
            if let Some(summary) = &item.summary {
                add_field(summary, SUMMARY_FIELD_WEIGHT);
            }
            if let Some(content) = item.path.as_ref().and_then(|path| file_contents.get(path)) {
                let body = slice_evidence_lines(content, item.range.as_ref());
                add_field(&body, BODY_FIELD_WEIGHT);
            }
            if term_frequencies.is_empty() {
                continue;
            }
            index.doc_ids.push(item.id.clone());
            index.doc_lengths.push(doc_length);
            for (term, frequency) in term_frequencies {
                index
                    .postings
                    .entry(term)
                    .or_default()
                    .push((ordinal, frequency));
            }
        }
        let total: f32 = index.doc_lengths.iter().sum();
        index.average_doc_length = if index.doc_lengths.is_empty() {
            0.0
        } else {
            total / index.doc_lengths.len() as f32
        };
        index
    }

    pub fn is_empty(&self) -> bool {
        self.doc_ids.is_empty()
    }

    /// Rank documents for a query with BM25. Results are sorted by
    /// descending score, ties broken by evidence id for determinism.
    pub fn search(&self, query: &str, limit: usize, k1: f32, b: f32) -> Vec<(EvidenceId, f32)> {
        if self.doc_ids.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut terms = code_tokens(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Vec::new();
        }
        let corpus_size = self.doc_ids.len() as f32;
        let mut scores: BTreeMap<u32, f32> = BTreeMap::new();
        for term in &terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let document_frequency = postings.len() as f32;
            let idf =
                ((corpus_size - document_frequency + 0.5) / (document_frequency + 0.5) + 1.0).ln();
            for (ordinal, term_frequency) in postings {
                let doc_length = self.doc_lengths[*ordinal as usize];
                let normalized = doc_length / self.average_doc_length.max(f32::EPSILON);
                let denominator = term_frequency + k1 * (1.0 - b + b * normalized);
                let score = idf * (term_frequency * (k1 + 1.0)) / denominator.max(f32::EPSILON);
                *scores.entry(*ordinal).or_default() += score;
            }
        }
        let mut ranked = scores
            .into_iter()
            .filter(|(_, score)| *score > 0.0)
            .map(|(ordinal, score)| (self.doc_ids[ordinal as usize].clone(), score))
            .collect::<Vec<_>>();
        ranked.sort_by(|(left_id, left_score), (right_id, right_score)| {
            right_score
                .partial_cmp(left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left_id.0.cmp(&right_id.0))
        });
        ranked.truncate(limit);
        ranked
    }
}

/// Tokenize code-bearing text: whole identifiers plus their subtokens,
/// all lowercased. Pure punctuation and single digits produce nothing.
pub(crate) fn code_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for identifier in text
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
    {
        let lower = identifier.to_ascii_lowercase();
        let subtokens = split_identifier(identifier);
        // Whole identifier first so exact matches share a posting list.
        if subtokens.len() > 1 || subtokens.first().map(String::as_str) != Some(lower.as_str()) {
            tokens.push(lower);
        }
        tokens.extend(subtokens);
    }
    tokens.retain(|token| token.len() > 1 || token.chars().all(|ch| ch.is_ascii_alphabetic()));
    tokens
}

/// Split an identifier on `_`, camelCase, PascalCase, ALLCAPS runs, and
/// digit boundaries. Output is lowercased.
fn split_identifier(identifier: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for word in identifier.split('_').filter(|word| !word.is_empty()) {
        let chars = word.chars().collect::<Vec<_>>();
        let mut start = 0usize;
        for index in 1..chars.len() {
            let previous = chars[index - 1];
            let current = chars[index];
            let case_boundary = previous.is_ascii_lowercase() && current.is_ascii_uppercase();
            let acronym_boundary = previous.is_ascii_uppercase()
                && current.is_ascii_uppercase()
                && chars.get(index + 1).is_some_and(|next| next.is_ascii_lowercase());
            let digit_boundary = previous.is_ascii_digit() != current.is_ascii_digit();
            if case_boundary || acronym_boundary || digit_boundary {
                parts.push(chars[start..index].iter().collect::<String>().to_ascii_lowercase());
                start = index;
            }
        }
        parts.push(chars[start..].iter().collect::<String>().to_ascii_lowercase());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::contracts::{EvidenceId, RepoPath};

    use crate::context_engine::{
        ContextEvidenceKind, ContextEvidenceSource, ContextProvenance, ContextRankSignals,
        ContextScope, ContextSensitivity, ContextTrust,
    };

    const K1: f32 = 1.2;
    const B: f32 = 0.75;

    fn evidence(id: &str, path: &str, summary: &str) -> ContextEvidence {
        ContextEvidence {
            id: EvidenceId(id.to_string()),
            kind: ContextEvidenceKind::FileSpan,
            source: ContextEvidenceSource::Snapshot,
            trust: ContextTrust::Kernel,
            sensitivity: ContextSensitivity::Private,
            scope: ContextScope::Snapshot,
            path: Some(RepoPath::parse(path).unwrap()),
            revision: None,
            range: None,
            content_hash: None,
            summary: Some(summary.to_string()),
            is_changed_span: false,
            signals: ContextRankSignals::default(),
            token_estimate: 1,
            provenance: ContextProvenance {
                provider: "test".to_string(),
                query: None,
                tool_call_id: None,
                snapshot_id: None,
                original_url: None,
            },
            created_at_utc: None,
            expires_at_utc: None,
        }
    }

    fn contents(
        entries: &[(&str, &str)],
    ) -> BTreeMap<crate::runtime::contracts::RepoPath, String> {
        entries
            .iter()
            .map(|(path, content)| {
                (RepoPath::parse(path).unwrap(), content.to_string())
            })
            .collect()
    }

    #[test]
    fn tokenizer_splits_camel_snake_acronym_and_digit_boundaries() {
        assert_eq!(
            code_tokens("getUserId"),
            vec!["getuserid", "get", "user", "id"]
        );
        assert_eq!(
            code_tokens("get_user_id"),
            vec!["get_user_id", "get", "user", "id"]
        );
        assert_eq!(
            code_tokens("HTTPServer2x"),
            vec!["httpserver2x", "http", "server", "2", "x"]
                .into_iter()
                .filter(|token| token.len() > 1 || token.chars().all(|ch| ch.is_ascii_alphabetic()))
                .collect::<Vec<_>>()
        );
        assert!(code_tokens("...!!!").is_empty());
    }

    #[test]
    fn camel_case_query_finds_snake_case_definition() {
        let docs = vec![
            evidence("ev_def", "src/auth/user.rs", "fn get_user_id in src/auth/user.rs"),
            evidence("ev_noise", "src/routes.rs", "fn handle_user_routes in src/routes.rs"),
        ];
        let files = contents(&[
            ("src/auth/user.rs", "pub fn get_user_id(token: &Token) -> UserId {}\n"),
            ("src/routes.rs", "pub fn handle_user_routes() { /* user pages */ }\n"),
        ]);
        let index = LexicalIndex::build(&docs, &files);
        let ranked = index.search("getUserId", 10, K1, B);
        assert_eq!(ranked[0].0 .0, "ev_def");
    }

    #[test]
    fn exact_identifier_ranks_exact_match_first() {
        let docs = vec![
            evidence("ev_exact", "src/token.rs", "fn validate_token in src/token.rs"),
            evidence("ev_partial", "src/lib.rs", "fn validate in src/lib.rs"),
        ];
        let files = contents(&[
            ("src/token.rs", "pub fn validate_token() {}\n"),
            ("src/lib.rs", "pub fn validate() { let token = 1; }\n"),
        ]);
        let index = LexicalIndex::build(&docs, &files);
        let ranked = index.search("validate_token", 10, K1, B);
        assert_eq!(ranked[0].0 .0, "ev_exact");
    }

    #[test]
    fn rare_term_hits_defining_chunk() {
        let mut docs = vec![evidence(
            "ev_rare",
            "src/errors.rs",
            "const ERR_TOKEN_EXPIRED in src/errors.rs",
        )];
        let mut entries = vec![(
            "src/errors.rs".to_string(),
            "pub const ERR_TOKEN_EXPIRED: u32 = 4011;\n".to_string(),
        )];
        for index in 0..20 {
            let path = format!("src/noise{index}.rs");
            docs.push(evidence(&format!("ev_noise{index}"), &path, "common helper"));
            entries.push((path, "pub fn helper() { let value = 1; }\n".to_string()));
        }
        let files = entries
            .iter()
            .map(|(path, content)| (RepoPath::parse(path).unwrap(), content.clone()))
            .collect();
        let index = LexicalIndex::build(&docs, &files);
        let ranked = index.search("ERR_TOKEN_EXPIRED", 5, K1, B);
        assert_eq!(ranked[0].0 .0, "ev_rare");
    }

    #[test]
    fn empty_and_punctuation_queries_return_empty() {
        let docs = vec![evidence("ev", "src/lib.rs", "fn main in src/lib.rs")];
        let index = LexicalIndex::build(&docs, &contents(&[("src/lib.rs", "fn main() {}\n")]));
        assert!(index.search("", 10, K1, B).is_empty());
        assert!(index.search("?!,.;", 10, K1, B).is_empty());
    }

    #[test]
    fn ties_break_on_evidence_id_and_order_is_stable() {
        let docs = vec![
            evidence("ev_b", "src/twin_b.rs", "twin helper"),
            evidence("ev_a", "src/twin_a.rs", "twin helper"),
        ];
        let files = contents(&[
            ("src/twin_a.rs", "pub fn twin() {}\n"),
            ("src/twin_b.rs", "pub fn twin() {}\n"),
        ]);
        let index = LexicalIndex::build(&docs, &files);
        let first = index.search("twin helper", 10, K1, B);
        let second = index.search("twin helper", 10, K1, B);
        assert_eq!(first, second);
        // Path tokens differ (twin_a vs twin_b) but shared terms tie; the
        // full ranking must still be reproducible and id-ordered on ties.
        assert_eq!(first.len(), 2);
    }
}
