use std::collections::{BTreeMap, BTreeSet};

use super::diff_risk::parse_hunk_head_start;
use super::findings::CandidateFinding;

pub(super) fn autonomous_candidate_rejection_reason(
    candidate: &CandidateFinding,
    changed_paths: &BTreeSet<String>,
    changed_ranges: &BTreeMap<String, Vec<(usize, usize)>>,
) -> Option<&'static str> {
    let path = candidate.path.trim();
    let title = candidate.title.trim();
    let claim = candidate.claim.trim();
    let negative_outcome = candidate.negative_outcome.trim();
    if path.is_empty() {
        return Some("invalid_path");
    }
    let primary_path_changed = changed_paths.contains(path);
    let has_changed_related_path = candidate
        .related_paths
        .iter()
        .any(|related_path| changed_paths.contains(related_path.trim()));
    if !primary_path_changed && !has_changed_related_path {
        return Some("unchanged_path");
    }
    if title.is_empty() || claim.is_empty() {
        return Some("empty_title_or_claim");
    }
    if negative_outcome.is_empty() {
        return Some("missing_negative_outcome");
    }
    let Some((start_line, end_line)) = candidate.start_line.zip(candidate.end_line) else {
        return Some("missing_line_range");
    };
    if primary_path_changed {
        if let Some(ranges) = changed_ranges.get(path) {
            if !ranges.iter().any(|(start, end)| {
                ranges_overlap(start_line, end_line.max(start_line), *start, *end)
            }) {
                return Some("line_range_not_changed");
            }
        } else if !changed_ranges.is_empty() {
            return Some("path_has_no_changed_lines");
        }
    }

    let behavior_before = candidate.behavior_before.as_deref().unwrap_or_default();
    let behavior_after = candidate.behavior_after.as_deref().unwrap_or_default();
    let title_and_claim = format!("{title} {claim} {negative_outcome}");
    let title_claim_describes_negative_outcome = describes_negative_outcome(&title_and_claim);
    let spelling_identifier_issue = is_spelling_identifier_issue_text(&title_and_claim);
    if behavior_comparison_missing(behavior_before, behavior_after)
        && !is_documentation_contract_mismatch_text(&title_and_claim)
        && !spelling_identifier_issue
        && !title_claim_describes_negative_outcome
    {
        return Some("missing_behavior_comparison");
    }
    if is_non_finding_text(&title_and_claim)
        || is_non_bug_observation_text(&title_and_claim)
        || is_counterfactual_support_observation_text(&title_and_claim)
    {
        return Some("non_finding_text");
    }
    if is_inline_comment_only_documentation_contract_text(&format!("{title} {claim}")) {
        return Some("weak_documentation_contract");
    }
    if is_bundled_finding_text(&title_and_claim) {
        return Some("bundled_claim");
    }
    if is_speculative_finding(
        &title_and_claim,
        behavior_before,
        behavior_after,
        title_claim_describes_negative_outcome,
    ) && !spelling_identifier_issue
    {
        return Some("speculative_claim");
    }
    if is_test_fixture_path(path) && claims_production_user_or_security_impact(&title_and_claim) {
        return Some("test_fixture_production_impact");
    }
    let full_text = format!("{title_and_claim} {behavior_before} {behavior_after}");
    if !describes_negative_outcome(&full_text) && !spelling_identifier_issue {
        return Some("missing_negative_outcome");
    }
    None
}

pub(super) fn changed_line_ranges_by_path(diff: &str) -> BTreeMap<String, Vec<(usize, usize)>> {
    let mut current_path: Option<String> = None;
    let mut current_new_line: Option<usize> = None;
    let mut ranges: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            current_new_line = None;
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_path = None;
            current_new_line = None;
            continue;
        }
        let Some(path) = current_path.as_ref() else {
            continue;
        };
        if line.starts_with("@@") {
            current_new_line = parse_hunk_head_start(line);
            continue;
        }
        let Some(new_line) = current_new_line else {
            continue;
        };
        if line.starts_with('+') && !line.starts_with("+++") {
            push_line_range(ranges.entry(path.clone()).or_default(), new_line);
            current_new_line = Some(new_line + 1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            continue;
        } else {
            current_new_line = Some(new_line + 1);
        }
    }
    ranges
}

fn behavior_comparison_missing(behavior_before: &str, behavior_after: &str) -> bool {
    behavior_text_is_vague(behavior_before) || behavior_text_is_vague(behavior_after)
}

fn behavior_text_is_vague(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    let normalized = trimmed.to_ascii_lowercase();
    [
        "not inspected",
        "not compared",
        "unknown",
        "unavailable",
        "no behavior",
        "no behavioral",
        "unclear",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_speculative_finding(
    text: &str,
    behavior_before: &str,
    behavior_after: &str,
    text_describes_negative_outcome: bool,
) -> bool {
    if is_hypothetical_finding_text(text) {
        return true;
    }
    is_hedged_finding_text(text)
        && !text_describes_negative_outcome
        && (behavior_before.trim().is_empty() || behavior_after.trim().is_empty())
}

fn is_hypothetical_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        " if later ",
        "if a future",
        "future change",
        "could become",
        "may become",
        "hypothetical",
        "speculative",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_documentation_contract_mismatch_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    let mentions_documentation = [
        "javadoc",
        "doc comment",
        "documentation",
        "documented",
        "documents",
        "comment",
        "contract",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if !mentions_documentation {
        return false;
    }
    let mentions_actual_contract = [
        "actual",
        "implementation",
        "implementations",
        "requires",
        "expects",
        "enforced",
        "fixed",
        "constraint",
        "shortcut",
        "format",
        "length",
        "count",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    mentions_actual_contract && describes_negative_outcome(&normalized)
}

fn is_spelling_identifier_issue_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    let spelling_signal = [
        "misspell",
        "spelling",
        "typo",
        "missing letter",
        "identifier",
        "method name",
        "helper name",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    let maintenance_signal = [
        "maintenance",
        "maintainer",
        "maintainers",
        "search",
        "discoverability",
        "readability",
        "future caller",
        "future callers",
        "propagate the typo",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    spelling_signal && maintenance_signal
}

fn is_inline_comment_only_documentation_contract_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    let inline_comment_signal = normalized.contains("todo")
        || normalized.contains("inline comment")
        || normalized.contains("comment sits")
        || normalized.contains("comment suggests")
        || normalized.contains("comment says");
    if !inline_comment_signal {
        return false;
    }
    let public_contract_signal = [
        "javadoc",
        "public api",
        "api documentation",
        "schema",
        "generated doc",
        "generated documentation",
        "example",
        "caller",
        "callers",
        "implementer",
        "implementers",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    !public_contract_signal
}

fn is_test_fixture_path(path: &str) -> bool {
    path.contains("/src/test/")
        || path.contains("/tests/")
        || path.starts_with("test/")
        || path.starts_with("tests/")
        || path.starts_with("testsuite/")
        || path.contains("/testsuite/")
}

fn claims_production_user_or_security_impact(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    let direct_user_or_security_impact = [
        " user ",
        " users ",
        " authenticate",
        " authentication",
        " login",
        " browser flow",
        " mfa",
        " second-factor",
        " security",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if direct_user_or_security_impact {
        return true;
    }
    let mentions_credential = normalized.contains(" credential")
        || normalized.contains(" credentials")
        || normalized.contains(" recovery code")
        || normalized.contains(" recovery-code");
    let mentions_auth_flow = normalized.contains("authenticator")
        || normalized.contains("authentication")
        || normalized.contains("authenticate")
        || normalized.contains("login");
    mentions_credential && mentions_auth_flow
}

fn is_hedged_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        " likely ",
        " may ",
        " might ",
        "potential",
        "appears to",
        " probably",
        " fragile",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_non_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "no additional",
        "no actionable",
        "no concrete",
        "no new incompatible",
        "no supported bug",
        "not a bug",
        "does not show",
        "no further issue",
        "no issue",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_non_bug_observation_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    let has_failure_signal = [
        "wrong",
        "fails",
        "failure",
        "broken",
        "missing",
        "undefined",
        "throws",
        "crash",
        "invalid",
        "incorrect",
        "inconsistent",
        "mismatch",
        "does not preserve",
        "fails to preserve",
        "not preserved",
        "lost",
        "drops",
        "dropped",
        "stale",
        "does not expose",
        "unparsed",
        "cannot",
        "never",
        "regression",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    let has_clean_signal = ["correct", "consistent"]
        .iter()
        .any(|needle| contains_ascii_word(&normalized, needle))
        || text_contains_clean_preservation_observation(&normalized)
        || [
            "continues",
            "still parses",
            "still consumes",
            "still returns",
            "still persists",
            "still writes",
            "no bug",
            "no issue",
            "no new incompatible",
        ]
        .iter()
        .any(|needle| normalized.contains(needle));
    has_clean_signal && !has_failure_signal
}

fn text_contains_clean_preservation_observation(text: &str) -> bool {
    if !text.contains("preserve") {
        return false;
    }
    ![
        "does not preserve",
        "do not preserve",
        "did not preserve",
        "doesn't preserve",
        "don't preserve",
        "didn't preserve",
        "fails to preserve",
        "failed to preserve",
        "not preserve",
        "not preserved",
        "without preserving",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn contains_ascii_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(start, _)| {
        let before_is_boundary = start == 0
            || !text.as_bytes()[start - 1].is_ascii_alphanumeric()
                && text.as_bytes()[start - 1] != b'_';
        let end = start + word.len();
        let after_is_boundary = end == text.len()
            || !text.as_bytes()[end].is_ascii_alphanumeric() && text.as_bytes()[end] != b'_';
        before_is_boundary && after_is_boundary
    })
}

fn is_counterfactual_support_observation_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    if normalized.contains("only safe because") || normalized.contains("safe because") {
        return true;
    }
    let support_scaffolding = [
        "added to support",
        "introduced to support",
        "required to support",
        "needed to support",
        "supports ",
        "supporting ",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    support_scaffolding && (normalized.contains("otherwise ") || normalized.contains("would break"))
}

fn is_bundled_finding_text(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "multiple issues",
        "multiple bugs",
        "several issues",
        "several bugs",
        "two issues",
        "two bugs",
        "first issue",
        "second issue",
        "another issue",
        "another bug",
        "separate issue",
        "separate bug",
        "independent issue",
        "independent bug",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn describes_negative_outcome(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    if [
        "data loss",
        "does not",
        "can no longer",
        "returns success before",
        "reports success before",
        "outside the",
        "outside tenant",
        "other tenant",
        "unrelated record",
        "unrelated row",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return true;
    }
    if describes_localization_failure(&normalized) {
        return true;
    }
    let tokens = normalized
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .collect::<BTreeSet<_>>();
    [
        "wrong",
        "incorrect",
        "invalid",
        "fail",
        "fails",
        "failed",
        "failure",
        "failing",
        "reject",
        "rejects",
        "rejected",
        "breaks",
        "broken",
        "missing",
        "drops",
        "dropped",
        "skips",
        "skipped",
        "omits",
        "omitted",
        "loses",
        "lost",
        "leaks",
        "leaked",
        "throws",
        "throw",
        "thrown",
        "crash",
        "crashes",
        "crashed",
        "panic",
        "panics",
        "undefined",
        "null",
        "cannot",
        "never",
        "unhandled",
        "unawaited",
        "race",
        "deadlock",
        "timeout",
        "hangs",
        "hanging",
        "stale",
        "duplicate",
        "duplicated",
        "outside",
        "unrelated",
        "unauthorized",
        "forbidden",
        "bypass",
        "bypasses",
        "bypassed",
        "unscoped",
        "unchecked",
        "mismatch",
        "inconsistent",
        "regression",
        "corrupt",
        "accidentally",
    ]
    .iter()
    .any(|needle| tokens.contains(needle))
}

fn describes_localization_failure(normalized: &str) -> bool {
    let localization_context = [
        "locale",
        "localized",
        "localization",
        "translation",
        "translated",
        "language",
        "simplified chinese",
        "traditional chinese",
        "zh_cn",
        "zh-tw",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if !localization_context {
        return false;
    }
    [
        "wrong language",
        "mixed-language",
        "copied",
        "copy-source",
        "inconsistent",
        "traditional chinese",
        "simplified chinese",
        " see ",
        "will see",
        "can see",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn push_line_range(ranges: &mut Vec<(usize, usize)>, line: usize) {
    if let Some((_, end)) = ranges.last_mut() {
        if line == *end + 1 {
            *end = line;
            return;
        }
    }
    ranges.push((line, line));
}

fn ranges_overlap(
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> bool {
    left_start <= right_end && right_start <= left_end
}
