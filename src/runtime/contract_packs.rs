use std::collections::{BTreeMap, BTreeSet};

use crate::review_plan::{ReviewPlan, ReviewPlanFileMode};
use crate::runtime::contracts::{stable_id, RepoPath};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractPackPlan {
    pub(crate) packs: Vec<ContractPack>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractPack {
    pub(crate) id: String,
    pub(crate) kind: ContractPackKind,
    pub(crate) primary_path: RepoPath,
    pub(crate) related_paths: Vec<RepoPath>,
    pub(crate) seed_queries: Vec<String>,
    pub(crate) required_evidence: Vec<String>,
    pub(crate) questions: Vec<String>,
    pub(crate) publishability_criteria: Vec<String>,
    pub(crate) summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContractPackKind {
    ReturnShape,
    CredentialOwnership,
    QueryFilterScope,
    TimeBoundary,
}

impl ContractPackPlan {
    pub(crate) fn empty() -> Self {
        Self { packs: Vec::new() }
    }

    pub(crate) fn pack_count(&self) -> usize {
        self.packs.len()
    }

    pub(crate) fn packs_for_path(&self, path: &RepoPath) -> Vec<&ContractPack> {
        self.packs
            .iter()
            .filter(|pack| {
                pack.primary_path == *path || pack.related_paths.iter().any(|related| related == path)
            })
            .collect()
    }
}

pub(crate) fn build_contract_pack_plan(review_plan: &ReviewPlan, diff: &str) -> ContractPackPlan {
    let changed_paths = review_plan
        .files
        .iter()
        .filter(|file| file.mode == ReviewPlanFileMode::Full)
        .map(|file| file.path.display())
        .collect::<BTreeSet<_>>();
    if changed_paths.is_empty() {
        return ContractPackPlan::empty();
    }
    let added = added_lines_by_path(diff);
    let producers = producer_symbols(&added, &changed_paths);
    let consumers = consumer_paths_by_symbol(&added, &changed_paths, &producers);
    let mut packs = Vec::new();
    for (symbol, producer_path) in producers {
        let Some(related) = consumers.get(&symbol) else {
            continue;
        };
        if related.is_empty() || !looks_like_return_shape_symbol(&symbol, &added, &producer_path) {
            continue;
        }
        let Ok(primary_path) = RepoPath::parse(&producer_path) else {
            continue;
        };
        let mut related_set = related.clone();
        for path in related {
            related_set.extend(companion_callback_paths(path, &changed_paths));
        }
        let related_paths = related_set
            .iter()
            .filter_map(|path| RepoPath::parse(path).ok())
            .collect::<Vec<_>>();
        if related_paths.is_empty() {
            continue;
        }
        packs.push(ContractPack {
            id: stable_id(&["return-shape", &producer_path, &symbol]),
            kind: ContractPackKind::ReturnShape,
            primary_path,
            related_paths,
            seed_queries: vec![symbol.clone(), "return".to_string(), "credential".to_string()],
            required_evidence: vec![
                "read the primary helper implementation and changed return statements".to_string(),
                "read at least one changed caller or consumer that uses the helper result"
                    .to_string(),
                "compare the returned value shape with the fields the caller reads or stores"
                    .to_string(),
            ],
            questions: vec![
                format!("What concrete value shape does `{symbol}` return after this change?"),
                "What value shape do changed callers expect at the use site?".to_string(),
                "Does the old/new behavior comparison show a caller-visible contract break?"
                    .to_string(),
            ],
            publishability_criteria: vec![
                "a finding cites evidence from the helper and at least one consumer".to_string(),
                "the claim names the mismatched returned fields or object shape".to_string(),
                "the claim describes a concrete failure path, not only a type concern".to_string(),
            ],
            summary: format!(
                "Investigate changed helper `{symbol}` for a caller-visible return-shape contract change."
            ),
        });
    }
    for path in credential_ownership_paths(&added) {
        let Ok(primary_path) = RepoPath::parse(&path) else {
            continue;
        };
        packs.push(ContractPack {
            id: stable_id(&["credential-ownership", &path]),
            kind: ContractPackKind::CredentialOwnership,
            primary_path,
            related_paths: Vec::new(),
            seed_queries: vec![
                "credential".to_string(),
                "userId".to_string(),
                "teamId".to_string(),
                "appId".to_string(),
            ],
            required_evidence: vec![
                "read the credential persistence path and changed owner fields".to_string(),
                "identify the source values assigned to ownership fields".to_string(),
                "compare those assignments to the credential lookup/use contract".to_string(),
            ],
            questions: vec![
                "Which user, team, app, or organization owns the persisted credential after this change?"
                    .to_string(),
                "Which owner fields do later reads expect to resolve the credential?".to_string(),
                "Does the change create a credential that cannot be found or is owned by the wrong actor?"
                    .to_string(),
            ],
            publishability_criteria: vec![
                "a finding cites evidence for the changed write and the expected owner contract"
                    .to_string(),
                "the claim names the incorrect owner field or source value".to_string(),
                "the claim explains how the persisted credential is later misused or missed".to_string(),
            ],
            summary:
                "Investigate changed credential persistence for ownership contract drift."
                    .to_string(),
        });
    }
    for path in query_filter_scope_paths(&added) {
        let Ok(primary_path) = RepoPath::parse(&path) else {
            continue;
        };
        packs.push(ContractPack {
            id: stable_id(&["query-filter-scope", &path]),
            kind: ContractPackKind::QueryFilterScope,
            primary_path,
            related_paths: Vec::new(),
            seed_queries: vec![
                "where".to_string(),
                "OR".to_string(),
                "AND".to_string(),
                "scope".to_string(),
            ],
            required_evidence: vec![
                "read the full changed query or filter construction".to_string(),
                "identify every branch of the predicate after the change".to_string(),
                "compare whether previous scope guards still apply to every branch".to_string(),
            ],
            questions: vec![
                "Which rows or objects matched before the predicate change?".to_string(),
                "Which rows or objects match after the predicate change?".to_string(),
                "Did an added OR/AND branch escape a user, method, owner, status, date, or tenant guard?"
                    .to_string(),
            ],
            publishability_criteria: vec![
                "a finding names the escaped predicate branch or missing guard".to_string(),
                "the claim explains the old and new matched set".to_string(),
                "the claim cites evidence from the changed query/filter code".to_string(),
            ],
            summary: "Investigate changed query/filter predicates for scope broadening."
                .to_string(),
        });
    }
    for path in time_boundary_paths(&added) {
        let Ok(primary_path) = RepoPath::parse(&path) else {
            continue;
        };
        packs.push(ContractPack {
            id: stable_id(&["time-boundary", &path]),
            kind: ContractPackKind::TimeBoundary,
            primary_path,
            related_paths: Vec::new(),
            seed_queries: vec![
                "start".to_string(),
                "end".to_string(),
                "timezone".to_string(),
                "boundary".to_string(),
            ],
            required_evidence: vec![
                "read the full changed date/time boundary calculation".to_string(),
                "identify which timezone each compared value is expressed in".to_string(),
                "compare start and end boundary calculations against the intended interval"
                    .to_string(),
            ],
            questions: vec![
                "Which instant or local date does each side of the comparison represent?".to_string(),
                "Does the end boundary use the event duration or accidentally reuse the start boundary?"
                    .to_string(),
                "Do zero-length or all-day overrides compare values or object identity?".to_string(),
            ],
            publishability_criteria: vec![
                "a finding names the specific boundary or timezone conversion that changed"
                    .to_string(),
                "the claim explains the old/new acceptance or rejection behavior".to_string(),
                "the claim describes a concrete slot, date, or interval that is misclassified"
                    .to_string(),
            ],
            summary: "Investigate changed date/time boundary logic for interval or timezone drift."
                .to_string(),
        });
    }
    packs.sort_by(|left, right| {
        left.primary_path
            .display()
            .cmp(&right.primary_path.display())
            .then(left.id.cmp(&right.id))
    });
    packs.dedup_by(|left, right| left.id == right.id);
    ContractPackPlan { packs }
}

fn added_lines_by_path(diff: &str) -> BTreeMap<String, Vec<String>> {
    let mut current_path: Option<String> = None;
    let mut lines = BTreeMap::<String, Vec<String>>::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("+++ /dev/null") {
            current_path = None;
            continue;
        }
        let Some(path) = current_path.as_ref() else {
            continue;
        };
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if added.starts_with("+++") {
            continue;
        }
        lines
            .entry(path.clone())
            .or_default()
            .push(added.trim().to_string());
    }
    lines
}

fn producer_symbols(
    added: &BTreeMap<String, Vec<String>>,
    changed_paths: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let mut symbols = BTreeMap::new();
    for (path, lines) in added {
        if !changed_paths.contains(path) {
            continue;
        }
        for line in lines {
            if let Some(symbol) = const_symbol(line)
                .or_else(|| function_symbol(line))
                .or_else(|| default_export_symbol(path, line))
            {
                symbols.insert(symbol, path.clone());
            }
        }
    }
    symbols
}

fn consumer_paths_by_symbol(
    added: &BTreeMap<String, Vec<String>>,
    changed_paths: &BTreeSet<String>,
    producers: &BTreeMap<String, String>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut consumers = BTreeMap::<String, BTreeSet<String>>::new();
    for (path, lines) in added {
        if !changed_paths.contains(path) {
            continue;
        }
        for (symbol, producer_path) in producers {
            if path == producer_path {
                continue;
            }
            if lines.iter().any(|line| contains_identifier(line, symbol)) {
                consumers
                    .entry(symbol.clone())
                    .or_default()
                    .insert(path.clone());
            }
        }
    }
    consumers
}

fn credential_ownership_paths(added: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    added
        .iter()
        .filter(|(path, lines)| {
            path.contains("credential")
                && lines.iter().any(|line| line.contains("prisma.credential.create"))
                && lines.iter().any(|line| line.contains("userId"))
                && lines.iter().any(|line| line.contains("appId"))
        })
        .map(|(path, _)| path.clone())
        .collect()
}

fn query_filter_scope_paths(added: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    added
        .iter()
        .filter(|(_, lines)| {
            lines.iter().any(|line| {
                let lower = line.to_ascii_lowercase();
                lower.contains("deletemany")
                    || lower.contains("updatemany")
                    || lower.contains("findmany")
                    || lower.contains("where:")
                    || lower.contains("filter(")
            }) && lines.iter().any(|line| {
                let lower = line.to_ascii_lowercase();
                lower.contains(" or")
                    || lower.contains("or:")
                    || lower.contains(" and")
                    || lower.contains("and:")
                    || lower.contains("where")
            })
        })
        .map(|(path, _)| path.clone())
        .collect()
}

fn time_boundary_paths(added: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    added
        .iter()
        .filter(|(_, lines)| {
            let joined = lines.join("\n").to_ascii_lowercase();
            (joined.contains("dayjs")
                || joined.contains("timezone")
                || joined.contains("utcoffset")
                || joined.contains("slotstart")
                || joined.contains("slotend")
                || joined.contains("workinghour")
                || joined.contains("dateoverride"))
                && (joined.contains("start")
                    || joined.contains("end")
                    || joined.contains("before")
                    || joined.contains("after")
                    || joined.contains("same")
                    || joined.contains("boundary"))
        })
        .map(|(path, _)| path.clone())
        .collect()
}

fn companion_callback_paths(path: &str, changed_paths: &BTreeSet<String>) -> Vec<String> {
    let Some((prefix, _)) = path.split_once("/lib/") else {
        return Vec::new();
    };
    let callback = format!("{prefix}/api/callback.ts");
    changed_paths
        .contains(&callback)
        .then_some(callback)
        .into_iter()
        .collect()
}

fn looks_like_return_shape_symbol(
    symbol: &str,
    added: &BTreeMap<String, Vec<String>>,
    path: &str,
) -> bool {
    let lower = symbol.to_ascii_lowercase();
    if lower.contains("refresh") || lower.contains("parse") || lower.contains("credential") {
        return true;
    }
    added.get(path).is_some_and(|lines| {
        lines.iter().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("return") && (lower.contains("response") || lower.contains('{'))
        })
    })
}

fn const_symbol(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("const ")
        .or_else(|| line.strip_prefix("export const "))?;
    let name = rest.split(['=', ':', ' ']).next()?.trim();
    is_identifier(name).then(|| name.to_string())
}

fn function_symbol(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("function ")
        .or_else(|| line.strip_prefix("export function "))
        .or_else(|| line.strip_prefix("async function "))
        .or_else(|| line.strip_prefix("export async function "))?;
    let name = rest.split(['(', '<', ' ']).next()?.trim();
    is_identifier(name).then(|| name.to_string())
}

fn default_export_symbol(path: &str, line: &str) -> Option<String> {
    if !line.starts_with("export default ") {
        return None;
    }
    let stem = path.rsplit('/').next()?.split('.').next()?;
    is_identifier(stem).then(|| stem.to_string())
}

fn contains_identifier(line: &str, needle: &str) -> bool {
    line.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|part| part == needle)
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::ChangedFileStatus;
    use crate::review_plan::{
        PlannedFileContentState, PlannedReviewFile, ReviewPlan, ReviewPlanCounts,
    };
    use crate::runtime::contracts::SnapshotId;

    #[test]
    fn detects_return_shape_helper_and_changed_consumers() {
        let review_plan = test_plan(vec![
            "packages/app-store/_utils/oauth/refreshOAuthTokens.ts",
            "packages/app-store/googlecalendar/lib/CalendarService.ts",
            "packages/app-store/zoomvideo/lib/VideoApiAdapter.ts",
        ]);
        let diff =
            "diff --git a/packages/app-store/_utils/oauth/refreshOAuthTokens.ts b/packages/app-store/_utils/oauth/refreshOAuthTokens.ts\n\
--- a/packages/app-store/_utils/oauth/refreshOAuthTokens.ts\n\
+++ b/packages/app-store/_utils/oauth/refreshOAuthTokens.ts\n\
+const refreshOAuthTokens = async () => {\n\
+  return response;\n\
+};\n\
+export default refreshOAuthTokens;\n\
diff --git a/packages/app-store/googlecalendar/lib/CalendarService.ts b/packages/app-store/googlecalendar/lib/CalendarService.ts\n\
--- a/packages/app-store/googlecalendar/lib/CalendarService.ts\n\
+++ b/packages/app-store/googlecalendar/lib/CalendarService.ts\n\
+import refreshOAuthTokens from \"../../_utils/oauth/refreshOAuthTokens\";\n\
+const res = await refreshOAuthTokens();\n\
diff --git a/packages/app-store/zoomvideo/lib/VideoApiAdapter.ts b/packages/app-store/zoomvideo/lib/VideoApiAdapter.ts\n\
--- a/packages/app-store/zoomvideo/lib/VideoApiAdapter.ts\n\
+++ b/packages/app-store/zoomvideo/lib/VideoApiAdapter.ts\n\
+import refreshOAuthTokens from \"../../_utils/oauth/refreshOAuthTokens\";\n\
+const response = await refreshOAuthTokens();\n";
        let plan = build_contract_pack_plan(&review_plan, diff);

        assert_eq!(plan.packs.len(), 1);
        assert_eq!(plan.packs[0].kind, ContractPackKind::ReturnShape);
        assert_eq!(plan.packs[0].related_paths.len(), 2);
    }

    #[test]
    fn detects_credential_ownership_write() {
        let review_plan = test_plan(vec!["apps/web/pages/api/webhook/app-credential.ts"]);
        let diff = "diff --git a/apps/web/pages/api/webhook/app-credential.ts b/apps/web/pages/api/webhook/app-credential.ts\n\
--- a/apps/web/pages/api/webhook/app-credential.ts\n\
+++ b/apps/web/pages/api/webhook/app-credential.ts\n\
+await prisma.credential.create({\n\
+  data: {\n\
+    userId: reqBody.userId,\n\
+    appId: appMetadata.slug,\n\
+  },\n\
+});\n";
        let plan = build_contract_pack_plan(&review_plan, diff);

        assert_eq!(plan.packs.len(), 1);
        assert_eq!(plan.packs[0].kind, ContractPackKind::CredentialOwnership);
        assert!(!plan.packs[0].questions.is_empty());
    }

    #[test]
    fn detects_query_filter_scope_obligation() {
        let review_plan = test_plan(vec!["src/workflow.ts"]);
        let diff = "diff --git a/src/workflow.ts b/src/workflow.ts\n\
--- a/src/workflow.ts\n\
+++ b/src/workflow.ts\n\
+await prisma.workflowReminder.deleteMany({ where: { OR: [{ method: 'SMS' }, { retryCount: { gt: 1 } }] } });\n";
        let plan = build_contract_pack_plan(&review_plan, diff);

        assert!(plan.packs.iter().any(|pack| {
            pack.kind == ContractPackKind::QueryFilterScope
                && pack
                    .questions
                    .iter()
                    .any(|question| question.contains("matched before"))
        }));
    }

    #[test]
    fn detects_time_boundary_obligation() {
        let review_plan = test_plan(vec!["src/slots.ts"]);
        let diff = "diff --git a/src/slots.ts b/src/slots.ts\n\
--- a/src/slots.ts\n\
+++ b/src/slots.ts\n\
+const slotEndTime = slotStartTime.add(duration, 'minutes');\n\
+if (slotEndTime.isBefore(dayjs(date.start).tz(timeZone))) return false;\n";
        let plan = build_contract_pack_plan(&review_plan, diff);

        assert!(plan.packs.iter().any(|pack| {
            pack.kind == ContractPackKind::TimeBoundary
                && pack
                    .required_evidence
                    .iter()
                    .any(|item| item.contains("date/time"))
        }));
    }

    fn test_plan(paths: Vec<&str>) -> ReviewPlan {
        let files = paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| PlannedReviewFile {
                file_id: format!("changed-{index:04}"),
                path: RepoPath::parse(path).expect("path"),
                status: ChangedFileStatus::Modified,
                content_state: PlannedFileContentState::Available,
                estimated_bytes: Some(100),
                mode: ReviewPlanFileMode::Full,
                score: 80,
                reasons: Vec::new(),
            })
            .collect::<Vec<_>>();
        ReviewPlan {
            snapshot_id: SnapshotId("snapshot-test".to_string()),
            counts: ReviewPlanCounts {
                total_files: files.len(),
                excluded_files: 0,
                full_files: files.len(),
                execution_eligible_files: files.len(),
            },
            files,
        }
    }
}
