#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DiffRiskEntry {
    pub(super) id: String,
    pub(super) path: String,
    pub(super) line: Option<usize>,
    pub(super) category: &'static str,
    pub(super) code: String,
    pub(super) obligation: &'static str,
}

pub(super) fn format_diff_risk_inventory(diff: &str, max_entries: usize) -> String {
    let entries = diff_risk_inventory(diff, max_entries);
    if entries.is_empty() {
        return "(none detected by the heuristic inventory; still review all changed behavior)"
            .to_string();
    }
    entries
        .iter()
        .map(|entry| {
            let location = entry
                .line
                .map(|line| format!("{}:{line}", entry.path))
                .unwrap_or_else(|| entry.path.clone());
            format!(
                "- {} {} [{}] `{}`: {}",
                entry.id, location, entry.category, entry.code, entry.obligation
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn diff_risk_inventory(diff: &str, max_entries: usize) -> Vec<DiffRiskEntry> {
    let mut entries = Vec::new();
    let mut current_path = String::new();
    let mut head_line = None::<usize>;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = path.to_string();
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            current_path.clear();
            continue;
        }
        if line.starts_with("@@") {
            head_line = parse_hunk_head_start(line);
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            let changed_line = head_line;
            if let Some(line) = head_line.as_mut() {
                *line += 1;
            }
            if current_path.is_empty() {
                continue;
            }
            let added = line.trim_start_matches('+').trim();
            for (category, obligation) in risk_categories_for_added_line(added) {
                if entries.len() >= max_entries {
                    return entries;
                }
                entries.push(DiffRiskEntry {
                    id: format!("R{}", entries.len() + 1),
                    path: current_path.clone(),
                    line: changed_line,
                    category,
                    code: truncate_chars(added, 140),
                    obligation,
                });
            }
        } else if line.starts_with(' ') {
            if let Some(line) = head_line.as_mut() {
                *line += 1;
            }
        }
    }
    entries
}

pub(super) fn parse_hunk_head_start(line: &str) -> Option<usize> {
    let plus = line.split_whitespace().find(|part| part.starts_with('+'))?;
    let digits = plus
        .trim_start_matches('+')
        .split(',')
        .next()
        .unwrap_or_default();
    digits.parse().ok()
}

fn risk_categories_for_added_line(line: &str) -> Vec<(&'static str, &'static str)> {
    let mut categories = Vec::new();
    let lowered = line.to_ascii_lowercase();
    let callback_async = [
        ".foreach(async",
        ".map(async",
        ".filter(async",
        ".reduce(async",
        ".flatmap(async",
        ".some(async",
        ".every(async",
    ]
    .iter()
    .any(|pattern| lowered.contains(pattern));
    if callback_async {
        categories.push((
            "async_callback",
            "Verify the outer control flow awaits callback-produced work and side effects cannot complete after the caller reports success.",
        ));
    }
    if lowered.contains("await ")
        || lowered.contains(" async ")
        || lowered.starts_with("async ")
        || lowered.contains("promise<")
        || lowered.contains("promise.")
        || lowered.contains("new promise")
    {
        categories.push((
            "async_boundary",
            "Verify callers, return shape, ordering, cancellation, and error propagation across the new async boundary.",
        ));
    }
    if lowered.contains("import(") || lowered.contains("await appstore[") {
        categories.push((
            "lazy_module_loading",
            "Verify module lookup failures, rejected loads, and changed value shape are handled by consumers.",
        ));
    }
    if lowered.contains("promise.all")
        || lowered.contains(".push(")
            && (lowered.contains("promise")
                || lowered.contains("delete")
                || lowered.contains("update")
                || lowered.contains("send")
                || lowered.contains("write")
                || lowered.contains("create"))
    {
        categories.push((
            "side_effect_aggregation",
            "Verify every produced side-effect promise is included in the awaited aggregate before state changes or success returns.",
        ));
    }
    categories
}

pub(super) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str("\n[truncated]");
    }
    output
}
