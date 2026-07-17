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
    let mut localized_resource_keys = std::collections::BTreeSet::new();
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
            if is_plain_import_line(added) {
                continue;
            }
            for (category, obligation) in risk_categories_for_added_line(&current_path, added) {
                if category.starts_with("localized_")
                    && !localized_resource_keys.insert((current_path.clone(), category))
                {
                    continue;
                }
                entries.push(DiffRiskEntry {
                    id: String::new(),
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
    entries.sort_by(|left, right| {
        (
            risk_inventory_category_priority(left.category),
            risk_inventory_code_priority(left.category, &left.code),
            left.path.as_str(),
            left.line.unwrap_or(usize::MAX),
            left.code.as_str(),
        )
            .cmp(&(
                risk_inventory_category_priority(right.category),
                risk_inventory_code_priority(right.category, &right.code),
                right.path.as_str(),
                right.line.unwrap_or(usize::MAX),
                right.code.as_str(),
            ))
    });
    entries.truncate(max_entries);
    for (index, entry) in entries.iter_mut().enumerate() {
        entry.id = format!("R{}", index + 1);
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
        if localized_script_mismatch(&lowered_path, line) {
            categories.push((
                "localized_script_mismatch",
                "Verify the changed localized text uses the script and language conventions expected by the target locale, not a copied neighboring/source locale.",
            ));
        }
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
    if lowered.contains("pattern.compile(")
        || lowered.contains(".matcher(")
        || lowered.contains("matcher ")
        || lowered.contains(".find()")
        || lowered.contains(".group(")
        || lowered.contains("replacefirst(")
        || lowered.contains("replaceall(")
        || lowered.contains("regex")
    {
        categories.push((
            "regex_matcher_contract",
            "Verify matcher state, group consumption, replacement scope, and source/target parity when regex results drive validation or sanitization.",
        ));
    }
    if is_comment_or_doc_line(line) && comment_or_doc_has_contract_signal(&lowered) {
        categories.push((
            "documentation_contract_consistency",
            "Verify documentation, examples, and comments still match the enforced format and actual implementations.",
        ));
    }
    if suspicious_identifier_spelling(line) {
        categories.push((
            "suspicious_identifier_spelling",
            "Verify newly introduced identifiers are consistently spelled and do not split call sites, overrides, or intended API names.",
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
        || is_java_like_path(&lowered_path)
            && (lowered.contains(".findfirst().get()")
                || lowered.contains(".findany().get()")
                || lowered.contains(").get()")
                || lowered.contains(".get()"))
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
    if persisted_identity_signal(&lowered, &lowered_path) {
        categories.push((
            "persisted_identity_propagation",
            "Verify created or stored domain models preserve the persisted id/identity required by later update, remove, lookup, or audit operations.",
        ));
    }
    categories
}

fn risk_inventory_category_priority(category: &str) -> u8 {
    match category {
        "regex_matcher_contract" => 0,
        "localized_script_mismatch" => 1,
        "unchecked_optional_access"
        | "broad_exception_boundary"
        | "documentation_contract_consistency"
        | "persisted_identity_propagation"
        | "suspicious_identifier_spelling" => 2,
        "localized_resource_change" => 3,
        "nullability_contract"
        | "offset_or_slice_boundary"
        | "unchecked_collection_shape"
        | "identifier_lookup_contract" => 4,
        "async_callback" | "side_effect_aggregation" | "lazy_module_loading" => 5,
        "async_boundary" | "feature_gate_consistency" | "process_exit_boundary" => 6,
        _ => 9,
    }
}

fn risk_inventory_code_priority(category: &str, code: &str) -> u8 {
    let lowered = code.to_ascii_lowercase();
    match category {
        "regex_matcher_contract" => {
            if lowered.contains(".group(") || lowered.contains("replacefirst(") {
                return 0;
            }
            if lowered.contains(".find()") || lowered.contains(".matcher(") {
                return 1;
            }
            if lowered.contains("replaceall(") {
                return 2;
            }
        }
        "persisted_identity_propagation" => {
            if reconstructs_domain_model_from_stored_state(&lowered) {
                return 0;
            }
            if lowered.contains("generateid(") || lowered.contains("randomuuid(") {
                return 2;
            }
        }
        _ => {}
    }
    1
}

fn persisted_identity_signal(lowered: &str, lowered_path: &str) -> bool {
    if lowered.contains("setid(")
        || lowered.contains(".setid(")
        || lowered.contains("removebyid(")
        || lowered.contains("deletebyid(")
        || lowered.contains("updatebyid(")
        || lowered.contains("auditid")
    {
        return true;
    }
    if reconstructs_domain_model_from_stored_state(lowered) {
        return true;
    }
    let identity_verbs = [
        "create", "store", "persist", "save", "update", "remove", "delete",
    ];
    let identity_nouns = ["id", "identifier", "key"];
    let domain_objects = [
        "model",
        "entity",
        "record",
        "credential",
        "session",
        "token",
        "account",
        "user",
    ];
    let has_identity_verb = identity_verbs.iter().any(|verb| lowered.contains(verb));
    let has_identity_noun = identity_nouns.iter().any(|noun| lowered.contains(noun));
    let has_domain_object = domain_objects.iter().any(|noun| lowered.contains(noun));
    if has_identity_verb && has_identity_noun && has_domain_object {
        return true;
    }
    is_java_like_path(lowered_path)
        && lowered.contains("new ")
        && has_domain_object
        && (lowered.contains("model(")
            || lowered.contains("entity(")
            || lowered.contains("record("))
}

fn reconstructs_domain_model_from_stored_state(lowered: &str) -> bool {
    let has_domain_object = [
        "model",
        "entity",
        "record",
        "credential",
        "session",
        "token",
        "account",
        "user",
    ]
    .iter()
    .any(|noun| lowered.contains(noun));
    if !has_domain_object {
        return false;
    }
    let has_persisted_source = [
        "stored",
        "persisted",
        "saved",
        "existing",
        "current",
        "credentialmodel",
        "entity",
        "record",
    ]
    .iter()
    .any(|source| lowered.contains(source));
    if !has_persisted_source {
        return false;
    }
    [
        "createfrom",
        "fromcredential",
        "fromstored",
        "frommodel",
        "fromentity",
        "fromrecord",
        "copyfrom",
        "buildfrom",
        "hydrate",
        "reconstruct",
        "new ",
    ]
    .iter()
    .any(|factory| lowered.contains(factory))
}

fn is_java_like_path(lowered_path: &str) -> bool {
    lowered_path.ends_with(".java")
        || lowered_path.ends_with(".kt")
        || lowered_path.ends_with(".scala")
}

fn is_plain_import_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("import ")
        || trimmed.starts_with("import static ")
        || trimmed.starts_with("using ")
        || trimmed.starts_with("use ")
}

fn is_comment_or_doc_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with('*')
        || trimmed.starts_with("/*")
        || trimmed.starts_with("/**")
}

fn comment_or_doc_has_contract_signal(lowered: &str) -> bool {
    lowered
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token,
                "must"
                    | "should"
                    | "expected"
                    | "usually"
                    | "format"
                    | "length"
                    | "shortcut"
                    | "contract"
            )
        })
}

fn suspicious_identifier_spelling(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    [
        "santiz",
        "intial",
        "reciev",
        "sucess",
        "occured",
        "lenght",
        "enviroment",
        "seperat",
        "defualt",
        "paramter",
        "authenit",
        "validatoin",
    ]
    .iter()
    .any(|typo| lowered.contains(typo))
}

fn localized_script_mismatch(lowered_path: &str, line: &str) -> bool {
    let Some(value) = localized_value(line) else {
        return false;
    };
    if lowered_path.contains("_zh_cn.") || lowered_path.contains("-zh_cn.") {
        return contains_traditional_chinese_only_char(value);
    }
    if lowered_path.contains("_zh_tw.")
        || lowered_path.contains("-zh_tw.")
        || lowered_path.contains("_zh_hk.")
        || lowered_path.contains("-zh_hk.")
    {
        return contains_simplified_chinese_only_char(value);
    }
    false
}

fn localized_value(line: &str) -> Option<&str> {
    line.split_once('=')
        .or_else(|| line.split_once(':'))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
}

fn contains_traditional_chinese_only_char(value: &str) -> bool {
    value.chars().any(|ch| {
        matches!(
            ch,
            '個' | '們'
                | '來'
                | '會'
                | '傳'
                | '儲'
                | '關'
                | '啟'
                | '帳'
                | '復'
                | '應'
                | '擇'
                | '數'
                | '時'
                | '機'
                | '權'
                | '檔'
                | '測'
                | '瀏'
                | '覽'
                | '為'
                | '無'
                | '狀'
                | '現'
                | '產'
                | '畫'
                | '異'
                | '發'
                | '碼'
                | '確'
                | '稱'
                | '穩'
                | '組'
                | '經'
                | '網'
                | '置'
                | '義'
                | '與'
                | '舊'
                | '萬'
                | '號'
                | '衝'
                | '裝'
                | '見'
                | '規'
                | '訊'
                | '設'
                | '證'
                | '該'
                | '誤'
                | '請'
                | '變'
                | '資'
                | '連'
                | '選'
                | '開'
                | '間'
                | '雜'
                | '顯'
                | '驗'
                | '體'
                | '點'
        )
    })
}

fn contains_simplified_chinese_only_char(value: &str) -> bool {
    value.chars().any(|ch| {
        matches!(
            ch,
            '个' | '们'
                | '来'
                | '会'
                | '传'
                | '储'
                | '关'
                | '启'
                | '帐'
                | '复'
                | '应'
                | '择'
                | '数'
                | '时'
                | '机'
                | '权'
                | '档'
                | '测'
                | '览'
                | '为'
                | '无'
                | '状'
                | '现'
                | '产'
                | '画'
                | '异'
                | '发'
                | '码'
                | '确'
                | '称'
                | '稳'
                | '组'
                | '经'
                | '网'
                | '置'
                | '义'
                | '与'
                | '旧'
                | '万'
                | '号'
                | '冲'
                | '装'
                | '见'
                | '规'
                | '讯'
                | '设'
                | '证'
                | '该'
                | '误'
                | '请'
                | '变'
                | '资'
                | '连'
                | '选'
                | '开'
                | '间'
                | '杂'
                | '显'
                | '验'
                | '体'
                | '点'
        )
    })
}

pub(super) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push_str("\n[truncated]");
    }
    output
}
