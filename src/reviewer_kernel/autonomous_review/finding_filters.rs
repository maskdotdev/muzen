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
    if path.is_empty() {
        return Some("invalid_path");
    }
    if !changed_paths.contains(path) {
        return Some("unchanged_path");
    }
    if title.is_empty() || claim.is_empty() {
        return Some("empty_title_or_claim");
    }
    let Some((start_line, end_line)) = candidate.start_line.zip(candidate.end_line) else {
        return Some("missing_line_range");
    };
    if let Some(ranges) = changed_ranges.get(path) {
        if !ranges
            .iter()
            .any(|(start, end)| ranges_overlap(start_line, end_line.max(start_line), *start, *end))
        {
            return Some("line_range_not_changed");
        }
    } else if !changed_ranges.is_empty() {
        return Some("path_has_no_changed_lines");
    }

    let behavior_before = candidate.behavior_before.as_deref().unwrap_or_default();
    let behavior_after = candidate.behavior_after.as_deref().unwrap_or_default();
    if behavior_comparison_missing(behavior_before, behavior_after) {
        return Some("missing_behavior_comparison");
    }
    let title_and_claim = format!("{title} {claim}");
    if is_non_finding_text(&title_and_claim)
        || is_non_bug_observation_text(&title_and_claim)
        || is_counterfactual_support_observation_text(&title_and_claim)
    {
        return Some("non_finding_text");
    }
    if is_bundled_finding_text(&title_and_claim) {
        return Some("bundled_claim");
    }
    if is_speculative_finding(&title_and_claim, behavior_before, behavior_after) {
        return Some("speculative_claim");
    }
    let full_text = format!("{title_and_claim} {behavior_before} {behavior_after}");
    if !describes_negative_outcome(&full_text) {
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

fn is_speculative_finding(text: &str, behavior_before: &str, behavior_after: &str) -> bool {
    if is_hypothetical_finding_text(text) {
        return true;
    }
    is_hedged_finding_text(text)
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
        "mismatch",
        "does not expose",
        "unparsed",
        "cannot",
        "never",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    let has_clean_signal = [
        "correct",
        "preserve",
        "preserves",
        "continues",
        "still parses",
        "still consumes",
        "still returns",
        "still persists",
        "still writes",
        "consistent",
        "no bug",
        "no issue",
        "no new incompatible",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    has_clean_signal && !has_failure_signal
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
        " and also ",
        "; also ",
        ". also ",
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
    let tokens = normalized
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .collect::<BTreeSet<_>>();
    [
        "wrong",
        "incorrect",
        "invalid",
        "fails",
        "failed",
        "failure",
        "failing",
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
        "regression",
        "corrupt",
    ]
    .iter()
    .any(|needle| tokens.contains(needle))
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
