use std::collections::BTreeSet;

pub(crate) fn diff_risk_hint_items(diff: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let async_iteration_sites = async_iteration_callback_sites(diff);
    if !async_iteration_sites.is_empty() {
        hints.push(format!(
            "async callbacks in array/collection iteration at {}; verify returned promises are awaited or intentionally harmless",
            async_iteration_sites.join(", ")
        ));
    }
    if introduces_sync_to_async_contract(diff) {
        let symbols = async_contract_symbols(diff);
        let subject = if symbols.is_empty() {
            "changed APIs".to_string()
        } else {
            format!("changed APIs `{}`", symbols.join("`, `"))
        };
        hints.push(format!(
            "sync-to-async API contract changes in {subject}; inspect direct callers for missing awaits and changed error propagation"
        ));
    }
    if has_changed_url_fetch_boundary(diff) {
        hints.push(
            "changed URL fetching/opening boundary; inspect whether untrusted URL input is parsed and allowlisted before any network fetch or navigation"
                .to_string(),
        );
    }
    if has_changed_origin_or_frame_boundary(diff) {
        hints.push(
            "changed origin/referrer/postMessage/frame boundary; inspect parsed-origin validation, exact target origins, and frame embedding assumptions"
                .to_string(),
        );
    }
    if has_changed_template_or_render_boundary(diff) {
        hints.push(
            "changed template/rendering or string-to-HTML boundary; inspect escaping, nil/null handling, and template syntax on the changed render path"
                .to_string(),
        );
    }
    hints
}

pub(crate) fn diff_risk_hint_paths(diff: &str) -> BTreeSet<String> {
    async_iteration_callback_site_paths(diff)
        .into_iter()
        .chain(introduces_sync_to_async_contract(diff).then(|| "*".to_string()))
        .chain(has_changed_url_fetch_boundary(diff).then(|| "*".to_string()))
        .chain(has_changed_origin_or_frame_boundary(diff).then(|| "*".to_string()))
        .chain(has_changed_template_or_render_boundary(diff).then(|| "*".to_string()))
        .collect()
}

fn async_iteration_callback_sites(diff: &str) -> Vec<String> {
    async_iteration_callback_site_entries(diff)
        .into_iter()
        .map(|(path, pattern)| {
            path.map(|path| format!("{path} `{pattern}`"))
                .unwrap_or_else(|| format!("diff line `{pattern}`"))
        })
        .collect()
}

fn async_iteration_callback_site_paths(diff: &str) -> Vec<String> {
    async_iteration_callback_site_entries(diff)
        .into_iter()
        .filter_map(|(path, _)| path)
        .collect()
}

fn async_iteration_callback_site_entries(diff: &str) -> Vec<(Option<String>, &'static str)> {
    let mut current_file: Option<String> = None;
    let mut sites = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = Some(path.to_string());
            continue;
        }
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let Some(pattern) = [
            ".forEach(async",
            ".map(async",
            ".filter(async",
            ".reduce(async",
            ".some(async",
            ".every(async",
        ]
        .iter()
        .find(|pattern| line.contains(**pattern)) else {
            continue;
        };
        let site = (current_file.clone(), *pattern);
        if !sites.contains(&site) {
            sites.push(site);
        }
    }
    sites
}

fn has_changed_url_fetch_boundary(diff: &str) -> bool {
    changed_diff_lines(diff).any(|line| {
        contains_any(
            &line.to_ascii_lowercase(),
            &[
                "open(",
                "fetch(",
                "http.get",
                "http.post",
                "net::http",
                "uri.open",
                "open-uri",
                "urlopen",
                "requests.get",
                "requests.post",
                "new url(",
                "uri.parse",
            ],
        )
    })
}

fn has_changed_origin_or_frame_boundary(diff: &str) -> bool {
    changed_diff_lines(diff).any(|line| {
        contains_any(
            &line.to_ascii_lowercase(),
            &[
                "postmessage",
                "targetorigin",
                "origin",
                "referrer",
                "referer",
                "x-frame-options",
                "frame-ancestors",
                "allowall",
                "indexof(",
                ".include?",
                ".includes(",
                "startswith(",
                "starts_with",
            ],
        )
    })
}

fn has_changed_template_or_render_boundary(diff: &str) -> bool {
    changed_diff_lines(diff).any(|line| {
        contains_any(
            &line.to_ascii_lowercase(),
            &[
                "<%",
                "<%=",
                "render ",
                "render(",
                "html_safe",
                "raw(",
                "escape",
                "sanitize",
                "content_tag",
                "safe_join",
                "nil",
                "null",
                ".html",
                "template",
            ],
        )
    })
}

fn changed_diff_lines(diff: &str) -> impl Iterator<Item = &str> {
    diff.lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .map(|line| line.trim_start_matches('+').trim())
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn async_contract_symbols(diff: &str) -> Vec<String> {
    let mut symbols = BTreeSet::new();
    for line in diff.lines() {
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let line = line.trim_start_matches('+').trim_start();
        if !line.contains("async") {
            continue;
        }
        if let Some(name) = async_function_name(line) {
            symbols.insert(name);
        }
        if let Some(name) = async_const_name(line) {
            symbols.insert(name);
        }
    }
    symbols.into_iter().collect()
}

fn async_function_name(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("export async function ")
        .or_else(|| line.strip_prefix("async function "))?;
    leading_identifier(rest)
}

fn async_const_name(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("export const ")
        .or_else(|| line.strip_prefix("const "))
        .or_else(|| line.strip_prefix("export let "))
        .or_else(|| line.strip_prefix("let "))?;
    let (name, rhs) = rest.split_once('=')?;
    if !rhs.trim_start().starts_with("async") {
        return None;
    }
    let name = name.trim();
    if is_identifier(name) {
        Some(name.to_string())
    } else {
        None
    }
}

fn leading_identifier(rest: &str) -> Option<String> {
    let ident = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '$')
        .collect::<String>();
    if is_identifier(&ident) {
        Some(ident)
    } else {
        None
    }
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn introduces_sync_to_async_contract(diff: &str) -> bool {
    let mut removed_sync_function = false;
    let mut added_async_function = false;
    for line in diff.lines() {
        if line.starts_with('-') && !line.starts_with("---") && line.contains("=>") {
            removed_sync_function |= !line.contains("async") && !line.contains("Promise<");
        }
        if line.starts_with('+') && !line.starts_with("+++") && line.contains("=>") {
            added_async_function |= line.contains("async") || line.contains("Promise<");
        }
    }
    removed_sync_function && added_async_function
}
