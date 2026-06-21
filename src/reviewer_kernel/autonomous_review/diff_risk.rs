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
            for (category, obligation) in risk_categories_for_added_line(&current_path, added) {
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

fn risk_categories_for_added_line(path: &str, line: &str) -> Vec<(&'static str, &'static str)> {
    let mut categories = Vec::new();
    let lowered = line.to_ascii_lowercase();
    let lowered_path = path.to_ascii_lowercase();
    if lowered_path.ends_with(".properties") || lowered_path.ends_with(".po") {
        categories.push((
            "localized_resource_change",
            "Verify locale-appropriate text, placeholder parity, markup parity, and copied-source-language mistakes in changed localized resources.",
        ));
    }
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
    if lowered.contains("substring(")
        || lowered.contains("sublist(")
        || lowered.contains("charat(")
        || lowered.contains("indexof(")
    {
        categories.push((
            "offset_or_slice_boundary",
            "Verify start/end offsets, inclusive/exclusive bounds, and branch polarity against the encoded data shape.",
        ));
    }
    if lowered.contains("requirenonnull(")
        || lowered.contains("checknotnull(")
        || lowered.contains("assertnotnull(")
        || lowered.contains(" != null")
        || lowered.contains(" == null")
    {
        categories.push((
            "nullability_contract",
            "Verify the checked value is the value later consumed and that null handling preserves the intended API contract.",
        ));
    }
    if lowered.contains("optional.get()")
        || lowered.contains(".get()")
            && (lowered.contains("optional") || lowered.contains("orelse"))
    {
        categories.push((
            "unchecked_optional_access",
            "Verify presence is established before unwrapping optional/result-like values and that absence cannot crash the changed path.",
        ));
    }
    if lowered.contains("list.class")
        || lowered.contains("map.class")
        || lowered.contains("@suppresswarnings(\"unchecked\")")
        || lowered.contains("@suppresswarnings({\"unchecked\"")
        || lowered.contains("(list<")
        || lowered.contains("(map<")
    {
        categories.push((
            "unchecked_collection_shape",
            "Verify deserialized or cast collection elements have the expected type and shape before downstream use.",
        ));
    }
    if lowered.contains("system.exit(")
        || lowered.contains(".exit(")
            && (lowered.contains("picocli")
                || lowered.contains("commandline")
                || lowered.contains("exitcode"))
    {
        categories.push((
            "process_exit_boundary",
            "Verify changed command/control-flow code returns status through the expected boundary instead of terminating the host process unexpectedly.",
        ));
    }
    if lowered.contains("catch (exception")
        || lowered.contains("catch (runtimeexception")
        || lowered.contains("catch (throwable")
    {
        categories.push((
            "broad_exception_boundary",
            "Verify broad exception handling does not hide unrelated failures and matches the precise failure mode being tested or handled.",
        ));
    }
    if lowered.contains("feature")
        && (lowered.contains("enabled")
            || lowered.contains("isfeature")
            || lowered.contains("profile.")
            || lowered.contains("flag"))
    {
        categories.push((
            "feature_gate_consistency",
            "Verify cleanup, migration, and shared behavior are guarded by the same feature gate as the code that creates or consumes the state.",
        ));
    }
    if lowered.contains("findbyname(")
        || lowered.contains("getbyid(")
        || lowered.contains("getclientbyid(")
        || lowered.contains("getid()")
            && (lowered.contains("getname()") || lowered.contains("find"))
    {
        categories.push((
            "identifier_lookup_contract",
            "Verify identifier/name/owner fields used for lookup match the fields used when resources are created and later consumed.",
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
