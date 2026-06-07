use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::contracts::{ByteRangeV1, EvidenceLocationV1, EvidenceRefV1, FindingV1, ToolName};
use crate::runtime::contracts::*;
use crate::runtime::policy::{diff_risk_hint_items, diff_risk_hint_paths};
use crate::runtime::repo::RepoSnapshot;
use crate::util::redaction_none;

use super::authorization::ToolAuthorizer;
use super::cache::ToolResultCache;
use super::dispatch::ToolProviderDispatcher;
use super::metrics::{ConcurrentAtomicCounters, ConcurrentToolMetricsStore};
use super::provider::{
    BuiltinReviewToolProvider, InProcessToolProvider, JsonRpcToolProvider, ToolProvider,
};
use super::read::ReadService;
use super::redaction::Redactor;
use super::registry::{CustomToolContext, CustomToolOutput, ToolRegistry};
use super::search::SearchCoordinator;
use super::store::{
    artifact_kind_for_tool, evidence_line_range, evidence_location, evidence_revision_for_tool,
    finding_id_for_call, ConcurrentArtifactStore, ConcurrentFindingStore,
};
use super::validation::validate_invocation;

pub(crate) struct ToolEngine {
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) artifacts: Arc<ConcurrentArtifactStore>,
    pub(crate) findings: Arc<ConcurrentFindingStore>,
    pub(crate) read: Arc<ReadService>,
    pub(crate) search: Arc<SearchCoordinator>,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) limits: Arc<RuntimeLimits>,
    pub(crate) redactor: Arc<Redactor>,
    result_cache: ToolResultCache,
    read_permits: Arc<Semaphore>,
    authorizer: ToolAuthorizer,
    dispatcher: ToolProviderDispatcher,
    pub(crate) counters: Arc<ConcurrentAtomicCounters>,
    pub(crate) metrics: Arc<ConcurrentToolMetricsStore>,
}

impl ToolEngine {
    #[cfg(test)]
    pub(crate) fn new(
        snapshot: Arc<RepoSnapshot>,
        limits: Arc<RuntimeLimits>,
    ) -> RuntimeResult<Self> {
        Self::with_registry(snapshot, limits, Arc::new(ToolRegistry::review_defaults()?))
    }

    pub(crate) fn with_registry(
        snapshot: Arc<RepoSnapshot>,
        limits: Arc<RuntimeLimits>,
        registry: Arc<ToolRegistry>,
    ) -> RuntimeResult<Self> {
        let counters = Arc::new(ConcurrentAtomicCounters::default());
        let metrics = Arc::new(ConcurrentToolMetricsStore::default());
        let redactor = Arc::new(Redactor::new()?);
        let artifacts = Arc::new(ConcurrentArtifactStore::default());
        let read = Arc::new(ReadService::new(
            Arc::clone(&snapshot),
            Arc::clone(&limits),
            Arc::clone(&counters),
        ));
        let search = Arc::new(SearchCoordinator::new(
            Arc::clone(&snapshot),
            Arc::clone(&limits),
            Arc::clone(&redactor),
            Arc::clone(&artifacts),
            Arc::clone(&counters),
        )?);
        let mut providers: Vec<Arc<dyn ToolProvider>> = vec![
            Arc::new(BuiltinReviewToolProvider),
            Arc::new(InProcessToolProvider),
        ];
        providers.extend(registry.jsonrpc_transports().into_iter().map(
            |(provider_id, transport)| {
                Arc::new(JsonRpcToolProvider::new(provider_id, transport)) as Arc<dyn ToolProvider>
            },
        ));
        Ok(Self {
            snapshot,
            artifacts,
            findings: Arc::new(ConcurrentFindingStore::default()),
            read,
            search,
            registry,
            result_cache: ToolResultCache::new(limits.search_result_cache_bytes),
            read_permits: Arc::new(Semaphore::new(limits.max_read_concurrency_global.max(1))),
            authorizer: ToolAuthorizer::new(),
            dispatcher: ToolProviderDispatcher::new(providers, Arc::clone(&limits)),
            limits,
            redactor,
            counters,
            metrics,
        })
    }

    pub(crate) fn record_finding_result(
        &self,
        session_id: &SessionId,
        result: &ToolResultEnvelope,
        evidence_results: &[ToolResultEnvelope],
        revision_id: &str,
    ) -> Option<String> {
        let evidence = self.evidence_refs(evidence_results, revision_id);
        self.findings
            .insert_from_tool_result(session_id, result, evidence)
    }

    fn session_finding_matches_path(
        &self,
        session_id: &SessionId,
        finding_id: &str,
        path: &RepoPath,
    ) -> bool {
        let target = path.display();
        self.findings.all().iter().any(|finding| {
            finding.id == finding_id
                && finding.discovered_by.iter().any(|id| id == &session_id.0)
                && finding_primary_path(finding) == target
        })
    }

    fn session_finding_options_message(&self, session_id: &SessionId) -> String {
        let options = self
            .findings
            .all()
            .iter()
            .filter(|finding| finding.discovered_by.iter().any(|id| id == &session_id.0))
            .take(8)
            .map(|finding| format!("{}:{}", finding.id, finding_primary_path(finding)))
            .collect::<Vec<_>>();
        if options.is_empty() {
            "No findings have been recorded in this session yet.".to_string()
        } else {
            format!(
                "Available findings for this session: {}.",
                options.join(", ")
            )
        }
    }

    pub(crate) async fn execute_batch(
        &self,
        scope: SessionScope,
        turn_id: TurnId,
        calls: Vec<crate::runtime::contracts::ModelToolCall>,
        cancel: CancellationToken,
    ) -> Vec<ToolResultEnvelope> {
        if calls.len() > self.limits.max_tool_calls_per_turn {
            let results = calls
                .into_iter()
                .map(|call| {
                    self.error_result(
                        call.call_id,
                        call.name,
                        ToolErrorCode::TooManyMatches,
                        "too many tool calls in one model turn",
                        false,
                    )
                })
                .collect::<Vec<_>>();
            self.record_batch_metrics(&results);
            return results;
        }
        if calls.len() > 1
            && calls
                .iter()
                .any(|call| call.name.as_builtin() == Some(ToolName::Finish))
        {
            let results = calls
                .into_iter()
                .map(|call| {
                    self.error_result(
                        call.call_id,
                        call.name,
                        ToolErrorCode::InvalidArgs,
                        "finish must be called alone",
                        false,
                    )
                })
                .collect::<Vec<_>>();
            self.record_batch_metrics(&results);
            return results;
        }

        let mut seen_calls = HashSet::new();
        let mut duplicate_results = Vec::new();
        let mut calls_to_execute = Vec::new();
        for call in calls {
            let key = stable_id(&[call.name.as_str(), &call.raw_arguments]);
            if matches!(
                call.name.as_builtin(),
                Some(ToolName::RecordFileReview | ToolName::RecordFinding | ToolName::Finish)
            ) || seen_calls.insert(key)
            {
                calls_to_execute.push(call);
            } else {
                duplicate_results.push((
                    call.index,
                    self.error_result(
                        call.call_id,
                        call.name,
                        ToolErrorCode::InvalidArgs,
                        "duplicate tool call in same model turn",
                        false,
                    ),
                ));
            }
        }

        let per_session = Arc::new(Semaphore::new(
            self.limits.max_tool_parallelism_per_session.max(1),
        ));
        let scope_key = scope
            .capabilities
            .fs_scope
            .scope_key(&self.snapshot.snapshot_id);
        let assigned_changed_files = assigned_changed_files(&scope);
        let scope = Arc::new(scope);
        let mut futures = FuturesUnordered::new();
        for call in calls_to_execute {
            let engine = self;
            let per_session = Arc::clone(&per_session);
            let cancel = cancel.clone();
            let session_id = scope.id.clone();
            let capabilities = scope.capabilities.clone();
            let scope_key = scope_key.clone();
            let assigned_changed_files = assigned_changed_files.clone();
            futures.push(async move {
                let original_index = call.index;
                let result = match validate_invocation(
                    session_id,
                    turn_id,
                    call,
                    capabilities,
                    scope_key,
                    &engine.registry,
                ) {
                    Ok(mut invocation) => {
                        invocation.assigned_changed_files = assigned_changed_files;
                        let Ok(_permit) = per_session.acquire_owned().await else {
                            return (
                                original_index,
                                engine.error_result(
                                    invocation.call_id,
                                    invocation.tool_id.clone(),
                                    ToolErrorCode::Internal,
                                    "tool semaphore closed",
                                    false,
                                ),
                            );
                        };
                        let input_bytes = invocation.input_bytes;
                        let started = Instant::now();
                        let mut result = engine.execute_invocation(invocation, cancel).await;
                        result.limits.input_bytes = input_bytes;
                        result.limits.latency_ms = elapsed_ms(started);
                        if matches!(result.cache.status, CacheStatus::Hit | CacheStatus::Deduped) {
                            result.limits.queue_wait_ms = 0;
                        }
                        result
                    }
                    Err((call_id, name, error)) => engine.error_result(
                        call_id,
                        name,
                        error,
                        validation_error_message(error),
                        false,
                    ),
                };
                (original_index, result)
            });
        }

        let mut ordered = Vec::new();
        ordered.extend(duplicate_results);
        while let Some(result) = futures.next().await {
            ordered.push(result);
        }
        ordered.sort_by_key(|(index, _)| *index);
        let results = ordered
            .into_iter()
            .map(|(_, result)| result)
            .collect::<Vec<_>>();
        self.record_batch_metrics(&results);
        results
    }

    async fn execute_invocation(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
    ) -> ToolResultEnvelope {
        if cancel.is_cancelled() {
            return self.error_result(
                invocation.call_id,
                invocation.tool_id,
                ToolErrorCode::Cancelled,
                "tool call cancelled",
                false,
            );
        }
        let Some(definition) = self.registry.definition(&invocation.tool_id) else {
            return self.error_result(
                invocation.call_id,
                invocation.tool_id,
                ToolErrorCode::UnknownTool,
                "tool is not registered",
                false,
            );
        };
        if let Err(error) = self.authorizer.authorize(&invocation, definition) {
            return self.error_result(
                invocation.call_id,
                invocation.tool_id,
                error.code,
                error.message,
                false,
            );
        }
        if let Some(key) = self.cache_key(&invocation) {
            return self.execute_cacheable(key, invocation, cancel).await;
        }
        self.compute_uncached(invocation, cancel, CacheStatus::NotCacheable)
            .await
    }

    async fn execute_cacheable(
        &self,
        key: String,
        invocation: ToolInvocation,
        cancel: CancellationToken,
    ) -> ToolResultEnvelope {
        self.result_cache
            .execute(
                key,
                invocation,
                cancel,
                self.counters.as_ref(),
                self.snapshot.snapshot_id.clone(),
                |invocation, cancel, cache_status| {
                    self.compute_uncached(invocation, cancel, cache_status)
                },
            )
            .await
    }

    async fn compute_uncached(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        self.dispatcher
            .execute(self, invocation, cancel, cache_status)
            .await
    }

    pub(super) async fn execute_builtin_tool(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        match invocation.builtin_name {
            Some(ToolName::ListChangedFiles) => self.list_changed_files(
                invocation.call_id,
                &invocation.capabilities.fs_scope,
                &invocation.scope_key,
                cache_status,
            ),
            Some(ToolName::ReadDiff) => self.read_diff(
                invocation.call_id,
                cache_status,
                invocation.assigned_changed_files.as_slice(),
            ),
            Some(ToolName::ListFiles) => self.list_files(
                invocation.call_id,
                &invocation.capabilities.fs_scope,
                &invocation.scope_key,
                cache_status,
            ),
            Some(ToolName::ReadBaseFile) => {
                let ToolArgs::ReadFile { path } = invocation.args else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "read_base_file requires path",
                        false,
                    );
                };
                if !invocation.capabilities.fs_scope.allows(&path) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::PathDenied,
                        "path is outside the session filesystem scope",
                        false,
                    );
                }
                self.base_snapshot_unavailable(invocation.call_id, path, cache_status)
            }
            Some(ToolName::ReadFile | ToolName::ReadHeadFile) => {
                let ToolArgs::ReadFile { path } = invocation.args else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "read_file requires path",
                        false,
                    );
                };
                if !invocation.capabilities.fs_scope.allows(&path) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::PathDenied,
                        "path is outside the session filesystem scope",
                        false,
                    );
                }
                self.read_file(
                    invocation.call_id,
                    invocation
                        .builtin_name
                        .expect("read_file branch has builtin tool"),
                    path,
                    cache_status,
                )
                .await
            }
            Some(ToolName::ReadFileRange) => {
                let ToolArgs::ReadFileRange {
                    path,
                    start_line,
                    end_line,
                } = invocation.args
                else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "read_file_range requires path, start_line, and end_line",
                        false,
                    );
                };
                if !invocation.capabilities.fs_scope.allows(&path) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::PathDenied,
                        "path is outside the session filesystem scope",
                        false,
                    );
                }
                self.read_file_range(invocation.call_id, path, start_line, end_line, cache_status)
                    .await
            }
            Some(ToolName::SearchText) => {
                let ToolArgs::SearchText { query } = invocation.args else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "search_text requires query",
                        false,
                    );
                };
                self.search_text(
                    invocation.call_id,
                    query,
                    &invocation.capabilities.fs_scope,
                    &invocation.scope_key,
                    cancel,
                    cache_status,
                )
                .await
            }
            Some(ToolName::FindRelatedFiles | ToolName::FindTestsForFile) => {
                let ToolArgs::ReadFile { path } = invocation.args else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "related/test file tools require path",
                        false,
                    );
                };
                if !invocation.capabilities.fs_scope.allows(&path) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::PathDenied,
                        "path is outside the session filesystem scope",
                        false,
                    );
                }
                self.find_files_for_path(
                    invocation.call_id,
                    invocation
                        .builtin_name
                        .expect("related/test branch has builtin tool"),
                    path,
                    &invocation.capabilities.fs_scope,
                    &invocation.scope_key,
                    cache_status,
                )
            }
            Some(ToolName::ListImports) => {
                let ToolArgs::ReadFile { path } = invocation.args else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "list_imports requires path",
                        false,
                    );
                };
                if !invocation.capabilities.fs_scope.allows(&path) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::PathDenied,
                        "path is outside the session filesystem scope",
                        false,
                    );
                }
                self.list_imports(invocation.call_id, path, cache_status)
                    .await
            }
            Some(ToolName::RecordFinding) => {
                let ToolArgs::RecordFinding {
                    title,
                    claim,
                    path,
                    start_line,
                    end_line,
                } = invocation.args
                else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "record_finding requires title, claim, path, start_line, and end_line",
                        false,
                    );
                };
                if !invocation.capabilities.fs_scope.allows(&path) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::PathDenied,
                        "path is outside the session filesystem scope",
                        false,
                    );
                }
                if is_non_finding_claim(&title, &claim) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "record_finding is only for actionable bugs; use finish for clean or inconclusive batches",
                        false,
                    );
                }
                self.record_finding(invocation.call_id, title, claim, path, start_line, end_line)
            }
            Some(ToolName::RecordFileReview) => {
                let ToolArgs::RecordFileReview {
                    path,
                    verdict,
                    summary,
                    finding_id,
                    related_paths,
                } = invocation.args
                else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "record_file_review requires path, verdict, summary, and related_paths",
                        false,
                    );
                };
                if !invocation.capabilities.fs_scope.allows(&path)
                    || related_paths
                        .iter()
                        .any(|path| !invocation.capabilities.fs_scope.allows(path))
                {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::PathDenied,
                        "path is outside the session filesystem scope",
                        false,
                    );
                }
                let (path_diff, _) =
                    scoped_diff_content(&self.snapshot.diff.content, std::slice::from_ref(&path));
                if weak_clean_review_for_risk_hinted_file(&path_diff, &path, &verdict, &summary) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "clean file review dismisses a changed-code risk hint without concrete safety evidence; inspect the hinted changed code and either record a finding or explain why the changed behavior is safe",
                        true,
                    );
                }
                if inconsistent_clean_review(&verdict, &summary) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "record_file_review verdict=clean cannot describe a concrete bug or dropped async work; record_finding first, then use verdict=issue_found with that finding_id",
                        true,
                    );
                }
                if inconsistent_issue_found_review(&verdict, &summary) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "record_file_review verdict=issue_found requires a summary of the concrete issue; use verdict=clean for files without an additional issue",
                        true,
                    );
                }
                if verdict.trim() == "issue_found" {
                    let Some(finding_id) = finding_id.as_deref() else {
                        let message = format!(
                            "record_file_review verdict=issue_found requires finding_id for a recorded finding in this session. {}",
                            self.session_finding_options_message(&invocation.session_id)
                        );
                        return self.error_result(
                            invocation.call_id,
                            invocation.tool_id,
                            ToolErrorCode::InvalidArgs,
                            &message,
                            true,
                        );
                    };
                    if !self.session_finding_matches_path(&invocation.session_id, finding_id, &path)
                    {
                        let message = format!(
                            "record_file_review finding_id must refer to a recorded finding from this session on the same path. {}",
                            self.session_finding_options_message(&invocation.session_id)
                        );
                        return self.error_result(
                            invocation.call_id,
                            invocation.tool_id,
                            ToolErrorCode::InvalidArgs,
                            &message,
                            true,
                        );
                    }
                } else if finding_id.is_some() {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "record_file_review finding_id is only allowed with verdict=issue_found",
                        true,
                    );
                }
                if inconsistent_skipped_review(&verdict, &summary) {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "record_file_review verdict=skipped is only for files that could not be inspected; use clean or issue_found after inspection",
                        true,
                    );
                }
                self.record_file_review(
                    invocation.call_id,
                    path,
                    verdict,
                    summary,
                    finding_id,
                    related_paths,
                )
            }
            Some(ToolName::ChallengeFinding) => {
                let ToolArgs::ChallengeFinding {
                    finding_id,
                    rationale,
                } = invocation.args
                else {
                    return self.error_result(
                        invocation.call_id,
                        invocation.tool_id,
                        ToolErrorCode::InvalidArgs,
                        "challenge_finding requires finding_id and rationale",
                        false,
                    );
                };
                self.challenge_finding(invocation.call_id, finding_id, rationale)
            }
            Some(ToolName::Finish) => {
                let reason = match invocation.args {
                    ToolArgs::Finish { reason } => reason,
                    _ => "finished".to_string(),
                };
                self.finish(invocation.call_id, reason)
            }
            None => self.error_result(
                invocation.call_id,
                invocation.tool_id,
                ToolErrorCode::UnknownTool,
                "tool is not registered as a built-in provider tool",
                false,
            ),
        }
    }

    fn cache_key(&self, invocation: &ToolInvocation) -> Option<String> {
        match &invocation.args {
            ToolArgs::Empty
                if matches!(
                    invocation.builtin_name,
                    Some(ToolName::ListChangedFiles | ToolName::ReadDiff | ToolName::ListFiles)
                ) =>
            {
                Some(stable_id(&[
                    &self.snapshot.snapshot_id.0,
                    invocation.tool_id.as_str(),
                    invocation.scope_key.as_str(),
                    &CONCURRENT_CONTRACT_VERSION.to_string(),
                    &REDACTION_POLICY_VERSION.to_string(),
                ]))
            }
            ToolArgs::ReadFile { path } => Some(stable_id(&[
                &self.snapshot.snapshot_id.0,
                invocation.tool_id.as_str(),
                invocation.scope_key.as_str(),
                &path.display(),
                &CONCURRENT_CONTRACT_VERSION.to_string(),
                &REDACTION_POLICY_VERSION.to_string(),
            ])),
            ToolArgs::ReadFileRange {
                path,
                start_line,
                end_line,
            } => Some(stable_id(&[
                &self.snapshot.snapshot_id.0,
                invocation.tool_id.as_str(),
                invocation.scope_key.as_str(),
                &path.display(),
                &start_line.to_string(),
                &end_line.to_string(),
                &CONCURRENT_CONTRACT_VERSION.to_string(),
                &REDACTION_POLICY_VERSION.to_string(),
            ])),
            ToolArgs::SearchText { query } => Some(stable_id(&[
                &self.snapshot.snapshot_id.0,
                invocation.tool_id.as_str(),
                invocation.scope_key.as_str(),
                query,
                &self.limits.max_search_matches.to_string(),
                &CONCURRENT_CONTRACT_VERSION.to_string(),
                &REDACTION_POLICY_VERSION.to_string(),
            ])),
            ToolArgs::Raw(raw) => self
                .registry
                .definition(&invocation.tool_id)
                .filter(|definition| definition.cacheable)
                .map(|_| {
                    stable_id(&[
                        &self.snapshot.snapshot_id.0,
                        invocation.tool_id.as_str(),
                        invocation.scope_key.as_str(),
                        &raw.to_string(),
                        &CONCURRENT_CONTRACT_VERSION.to_string(),
                        &REDACTION_POLICY_VERSION.to_string(),
                    ])
                }),
            _ => None,
        }
    }

    fn read_diff(
        &self,
        call_id: ToolCallId,
        cache_status: CacheStatus,
        assigned_changed_files: &[RepoPath],
    ) -> ToolResultEnvelope {
        let (raw_content, scoped_paths) =
            scoped_diff_content(&self.snapshot.diff.content, assigned_changed_files);
        let content_hash = stable_id(&[&self.snapshot.diff.content_hash, &raw_content]);
        let content = self.redactor.redact(&raw_content);
        let artifact_id = self.artifacts.insert_views(
            ArtifactKey(stable_id(&[
                &self.snapshot.snapshot_id.0,
                "read_diff",
                &content_hash,
            ])),
            raw_content.clone(),
            content.clone(),
        );
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::ReadDiff),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: Some(artifact_id),
            cache: CacheInfo {
                status: cache_status,
                key_hash: Some(content_hash.clone()),
            },
            limits: LimitInfo {
                output_bytes: content.len(),
                ..LimitInfo::default()
            },
            data: Some(json!({
                "content": content,
                "contentHash": content_hash,
                "riskHints": diff_risk_hint_items(&raw_content),
                "scopedPaths": scoped_paths,
                "scoped": !assigned_changed_files.is_empty(),
            })),
            error: None,
        }
    }

    fn list_changed_files(
        &self,
        call_id: ToolCallId,
        fs_scope: &FsScope,
        scope_key: &ScopeKey,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let files = self
            .snapshot
            .manifest
            .changed_file_entries
            .iter()
            .filter(|file| fs_scope.allows(&file.rel_path))
            .map(|file| file.summary.clone())
            .collect::<Vec<_>>();
        let content = files.join("\n");
        let artifact_id = self.artifacts.insert(
            ArtifactKey(stable_id(&[
                &self.snapshot.snapshot_id.0,
                "list_changed_files",
                scope_key.as_str(),
            ])),
            content,
        );
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::ListChangedFiles),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: Some(artifact_id),
            cache: CacheInfo {
                status: cache_status,
                key_hash: None,
            },
            limits: LimitInfo {
                output_bytes: files.iter().map(String::len).sum(),
                ..LimitInfo::default()
            },
            data: Some(json!({
                "changedFiles": files,
            })),
            error: None,
        }
    }

    fn list_files(
        &self,
        call_id: ToolCallId,
        fs_scope: &FsScope,
        scope_key: &ScopeKey,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let mut files = self
            .snapshot
            .manifest
            .files
            .iter()
            .filter(|file| file.is_text_candidate && fs_scope.allows(&file.rel_path))
            .map(|file| file.rel_path.display())
            .collect::<Vec<_>>();
        files.sort();
        let content = files
            .iter()
            .take(300)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let artifact_id = self.artifacts.insert(
            ArtifactKey(stable_id(&[
                &self.snapshot.snapshot_id.0,
                "list_files",
                scope_key.as_str(),
            ])),
            content,
        );
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::ListFiles),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: Some(artifact_id),
            cache: CacheInfo {
                status: cache_status,
                key_hash: None,
            },
            limits: LimitInfo {
                truncated: files.len() > 300,
                output_bytes: files.iter().map(String::len).sum(),
                ..LimitInfo::default()
            },
            data: Some(json!({
                "files": files.into_iter().take(300).collect::<Vec<_>>(),
            })),
            error: None,
        }
    }

    fn base_snapshot_unavailable(
        &self,
        call_id: ToolCallId,
        path: RepoPath,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let content = format!("base snapshot unavailable for {}", path.display());
        let artifact_id = self.artifacts.insert(
            ArtifactKey(stable_id(&[
                &self.snapshot.snapshot_id.0,
                "read_base_file",
                &path.display(),
            ])),
            content.clone(),
        );
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::ReadBaseFile),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: Some(artifact_id),
            cache: CacheInfo {
                status: cache_status,
                key_hash: None,
            },
            limits: LimitInfo {
                output_bytes: content.len(),
                ..LimitInfo::default()
            },
            data: Some(json!({
                "path": path.display(),
                "available": false,
                "errorCode": "BASE_SNAPSHOT_UNAVAILABLE",
                "message": content,
            })),
            error: None,
        }
    }

    async fn read_file(
        &self,
        call_id: ToolCallId,
        tool_name: ToolName,
        path: RepoPath,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let Ok(file) = self.snapshot.lookup(&path).cloned() else {
            return self.read_unavailable_result(
                call_id,
                tool_name.into(),
                ToolErrorCode::PathDenied,
                &path,
                "path is not present in the repo manifest",
                false,
            );
        };
        let Ok(_permit) = self.read_permits.clone().acquire_owned().await else {
            return self.error_result(
                call_id,
                tool_name.into(),
                ToolErrorCode::Internal,
                "read semaphore closed",
                false,
            );
        };
        match self.read.read_file(&file).await {
            Ok(read) => {
                let content = self.redactor.redact(&read.content);
                let line_count = read.content.lines().count().max(1);
                let tool_id = ToolId::from(tool_name);
                let artifact_id = self.artifacts.insert_views(
                    ArtifactKey(stable_id(&[
                        &self.snapshot.snapshot_id.0,
                        tool_name.as_str(),
                        &file.fingerprint,
                        &path.display(),
                    ])),
                    read.content.clone(),
                    content.clone(),
                );
                ToolResultEnvelope {
                    ok: true,
                    tool_call_id: call_id,
                    tool_name: tool_id,
                    provider_id: ToolProviderId::builtin_review(),
                    snapshot_id: self.snapshot.snapshot_id.clone(),
                    artifact_id: Some(artifact_id.clone()),
                    cache: CacheInfo {
                        status: cache_status,
                        key_hash: Some(file.fingerprint.clone()),
                    },
                    limits: LimitInfo {
                        truncated: read.truncated,
                        output_bytes: content.len(),
                        ..LimitInfo::default()
                    },
                    data: Some(json!({
                        "path": path.display(),
                        "content": content,
                        "lineRange": {
                            "startLine": 1,
                            "endLine": line_count,
                        },
                        "evidenceId": stable_id(&[
                            &self.snapshot.snapshot_id.0,
                            &file.file_id.0.to_string(),
                            &file.fingerprint,
                            &artifact_id.0,
                        ]),
                    })),
                    error: None,
                }
            }
            Err(error) => self.read_error_result(call_id, tool_name.into(), &path, error),
        }
    }

    async fn read_file_range(
        &self,
        call_id: ToolCallId,
        path: RepoPath,
        start_line: usize,
        end_line: usize,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let Ok(file) = self.snapshot.lookup(&path).cloned() else {
            return self.read_unavailable_result(
                call_id,
                ToolName::ReadFileRange.into(),
                ToolErrorCode::PathDenied,
                &path,
                "path is not present in the repo manifest",
                false,
            );
        };
        let Ok(_permit) = self.read_permits.clone().acquire_owned().await else {
            return self.error_result(
                call_id,
                ToolName::ReadFileRange.into(),
                ToolErrorCode::Internal,
                "read semaphore closed",
                false,
            );
        };
        match self.read.read_file(&file).await {
            Ok(read) => {
                let line_count = read.content.lines().count().max(1);
                let normalized_start = start_line.min(line_count).max(1);
                let normalized_end = end_line.min(line_count).max(normalized_start);
                let raw_slice = read
                    .content
                    .lines()
                    .enumerate()
                    .filter(|(index, _)| {
                        let line_number = index + 1;
                        line_number >= normalized_start && line_number <= normalized_end
                    })
                    .map(|(index, line)| format!("{}: {}", index + 1, line))
                    .collect::<Vec<_>>()
                    .join("\n");
                let content = self.redactor.redact(&raw_slice);
                let artifact_id = self.artifacts.insert_views(
                    ArtifactKey(stable_id(&[
                        &self.snapshot.snapshot_id.0,
                        "read_file_range",
                        &file.fingerprint,
                        &path.display(),
                        &normalized_start.to_string(),
                        &normalized_end.to_string(),
                    ])),
                    raw_slice,
                    content.clone(),
                );
                ToolResultEnvelope {
                    ok: true,
                    tool_call_id: call_id,
                    tool_name: ToolId::from(ToolName::ReadFileRange),
                    provider_id: ToolProviderId::builtin_review(),
                    snapshot_id: self.snapshot.snapshot_id.clone(),
                    artifact_id: Some(artifact_id.clone()),
                    cache: CacheInfo {
                        status: cache_status,
                        key_hash: Some(stable_id(&[
                            &file.fingerprint,
                            &normalized_start.to_string(),
                            &normalized_end.to_string(),
                        ])),
                    },
                    limits: LimitInfo {
                        truncated: read.truncated,
                        output_bytes: content.len(),
                        ..LimitInfo::default()
                    },
                    data: Some(json!({
                        "path": path.display(),
                        "content": content,
                        "lineRange": {
                            "startLine": normalized_start,
                            "endLine": normalized_end,
                        },
                        "evidenceId": stable_id(&[
                            &self.snapshot.snapshot_id.0,
                            &file.file_id.0.to_string(),
                            &file.fingerprint,
                            &artifact_id.0,
                        ]),
                    })),
                    error: None,
                }
            }
            Err(error) => {
                self.read_error_result(call_id, ToolName::ReadFileRange.into(), &path, error)
            }
        }
    }

    fn find_files_for_path(
        &self,
        call_id: ToolCallId,
        tool_name: ToolName,
        path: RepoPath,
        fs_scope: &FsScope,
        scope_key: &ScopeKey,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let stem = path
            .as_path()
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        let mut files = self
            .snapshot
            .manifest
            .files
            .iter()
            .filter(|file| file.is_text_candidate && fs_scope.allows(&file.rel_path))
            .filter(|file| {
                if tool_name == ToolName::FindRelatedFiles {
                    file.rel_path.as_path() != path.as_path()
                        && !stem.is_empty()
                        && file.rel_path.display().contains(&stem)
                } else {
                    let text = file.rel_path.display();
                    (text.contains("test") || text.contains("spec"))
                        && (stem.is_empty() || text.contains(&stem))
                }
            })
            .map(|file| file.rel_path.display())
            .take(80)
            .collect::<Vec<_>>();
        files.sort();
        let truncated = files.len() >= 80;
        let content = files.join("\n");
        let artifact_id = self.artifacts.insert(
            ArtifactKey(stable_id(&[
                &self.snapshot.snapshot_id.0,
                tool_name.as_str(),
                scope_key.as_str(),
                &path.display(),
            ])),
            content,
        );
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(tool_name),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: Some(artifact_id),
            cache: CacheInfo {
                status: cache_status,
                key_hash: Some(stable_id(&[&path.display()])),
            },
            limits: LimitInfo {
                truncated,
                output_bytes: files.iter().map(String::len).sum(),
                ..LimitInfo::default()
            },
            data: Some(json!({
                "path": path.display(),
                "files": files,
            })),
            error: None,
        }
    }

    async fn list_imports(
        &self,
        call_id: ToolCallId,
        path: RepoPath,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let Ok(file) = self.snapshot.lookup(&path).cloned() else {
            return self.error_result(
                call_id,
                ToolName::ListImports.into(),
                ToolErrorCode::PathDenied,
                "path is not present in the repo manifest",
                false,
            );
        };
        let Ok(_permit) = self.read_permits.clone().acquire_owned().await else {
            return self.error_result(
                call_id,
                ToolName::ListImports.into(),
                ToolErrorCode::Internal,
                "read semaphore closed",
                false,
            );
        };
        match self.read.read_file(&file).await {
            Ok(read) => {
                let imports = read
                    .content
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| {
                        let trimmed = line.trim_start();
                        trimmed.starts_with("use ")
                            || trimmed.starts_with("import ")
                            || trimmed.starts_with("export ")
                            || trimmed.starts_with("require(")
                            || trimmed.starts_with("from ")
                    })
                    .map(|(index, line)| {
                        format!("{}:{}:{}", path.display(), index + 1, line.trim())
                    })
                    .collect::<Vec<_>>();
                let raw_content = imports.join("\n");
                let content = self.redactor.redact(&raw_content);
                let artifact_id = self.artifacts.insert_views(
                    ArtifactKey(stable_id(&[
                        &self.snapshot.snapshot_id.0,
                        "list_imports",
                        &file.fingerprint,
                        &path.display(),
                    ])),
                    raw_content,
                    content,
                );
                ToolResultEnvelope {
                    ok: true,
                    tool_call_id: call_id,
                    tool_name: ToolId::from(ToolName::ListImports),
                    provider_id: ToolProviderId::builtin_review(),
                    snapshot_id: self.snapshot.snapshot_id.clone(),
                    artifact_id: Some(artifact_id),
                    cache: CacheInfo {
                        status: cache_status,
                        key_hash: Some(file.fingerprint.clone()),
                    },
                    limits: LimitInfo {
                        truncated: read.truncated,
                        output_bytes: imports.iter().map(String::len).sum(),
                        ..LimitInfo::default()
                    },
                    data: Some(json!({
                        "path": path.display(),
                        "imports": imports,
                    })),
                    error: None,
                }
            }
            Err(error) => self.runtime_error_result(call_id, ToolName::ListImports.into(), error),
        }
    }

    async fn search_text(
        &self,
        call_id: ToolCallId,
        query: String,
        fs_scope: &FsScope,
        scope_key: &ScopeKey,
        cancel: CancellationToken,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        match self.search.search(query, fs_scope, scope_key, cancel).await {
            Ok(mut result) => {
                result.tool_call_id = call_id;
                result.cache.status = cache_status;
                result
            }
            Err(error) => self.runtime_error_result(call_id, ToolName::SearchText.into(), error),
        }
    }

    fn challenge_finding(
        &self,
        call_id: ToolCallId,
        finding_id: String,
        rationale: String,
    ) -> ToolResultEnvelope {
        let content = format!("{finding_id}: {rationale}");
        let artifact_id = self.artifacts.insert_views(
            ArtifactKey(stable_id(&[
                &self.snapshot.snapshot_id.0,
                "challenge_finding",
                &finding_id,
                &rationale,
            ])),
            content.clone(),
            self.redactor.redact(&content),
        );
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::ChallengeFinding),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: Some(artifact_id),
            cache: CacheInfo {
                status: CacheStatus::NotCacheable,
                key_hash: None,
            },
            limits: LimitInfo {
                output_bytes: content.len(),
                ..LimitInfo::default()
            },
            data: Some(json!({
                "findingId": finding_id,
                "rationale": rationale,
            })),
            error: None,
        }
    }

    fn record_finding(
        &self,
        call_id: ToolCallId,
        title: String,
        claim: String,
        path: RepoPath,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> ToolResultEnvelope {
        let finding_id = finding_id_for_call(&call_id, &title, &claim);
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::RecordFinding),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: None,
            cache: CacheInfo {
                status: CacheStatus::NotCacheable,
                key_hash: None,
            },
            limits: LimitInfo::default(),
            data: Some(json!({
                "findingId": finding_id,
                "title": title,
                "claim": claim,
                "path": path.display().to_string(),
                "startLine": start_line,
                "endLine": end_line,
            })),
            error: None,
        }
    }

    fn record_file_review(
        &self,
        call_id: ToolCallId,
        path: RepoPath,
        verdict: String,
        summary: String,
        finding_id: Option<String>,
        related_paths: Vec<RepoPath>,
    ) -> ToolResultEnvelope {
        let related = related_paths
            .iter()
            .map(RepoPath::display)
            .collect::<Vec<_>>();
        let content = format!(
            "{}: {}\n{}\nRelated paths: {}",
            path.display(),
            verdict,
            summary.trim(),
            if related.is_empty() {
                "none".to_string()
            } else {
                related.join(", ")
            }
        );
        let redacted = self.redactor.redact(&content);
        let artifact_id = self.artifacts.insert_views(
            ArtifactKey(stable_id(&[
                &self.snapshot.snapshot_id.0,
                "record_file_review",
                &path.display(),
                &verdict,
                finding_id.as_deref().unwrap_or(""),
                summary.trim(),
            ])),
            content,
            redacted.clone(),
        );
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::RecordFileReview),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: Some(artifact_id),
            cache: CacheInfo {
                status: CacheStatus::NotCacheable,
                key_hash: None,
            },
            limits: LimitInfo {
                output_bytes: redacted.len(),
                ..LimitInfo::default()
            },
            data: Some(json!({
                "path": path.display().to_string(),
                "verdict": verdict,
                "summary": summary,
                "findingId": finding_id,
                "relatedPaths": related,
            })),
            error: None,
        }
    }

    fn evidence_refs(
        &self,
        results: &[ToolResultEnvelope],
        revision_id: &str,
    ) -> Vec<EvidenceRefV1> {
        results
            .iter()
            .filter(|result| result.ok)
            .filter_map(|result| {
                let tool = result.tool_name.as_builtin()?;
                let kind = artifact_kind_for_tool(tool)?;
                let artifact_id = result.artifact_id.as_ref()?;
                let artifact = self.artifacts.get(artifact_id)?;
                Some(EvidenceRefV1 {
                    evidence_id: format!("evidence-{}", artifact_id.0),
                    artifact_id: artifact_id.0.clone(),
                    kind,
                    revision: evidence_revision_for_tool(tool),
                    revision_id: revision_id.to_string(),
                    location: evidence_location(result, &self.snapshot),
                    line_range: evidence_line_range(result),
                    byte_range: Some(ByteRangeV1 {
                        start_byte: 0,
                        end_byte: artifact.bytes,
                    }),
                    diff_anchor: None,
                    content_hash: artifact.content_hash,
                    redaction: redaction_none(),
                    producing_tool_call_id: result.tool_call_id.0.clone(),
                })
            })
            .collect()
    }

    fn finish(&self, call_id: ToolCallId, reason: String) -> ToolResultEnvelope {
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: ToolId::from(ToolName::Finish),
            provider_id: ToolProviderId::builtin_review(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: None,
            cache: CacheInfo {
                status: CacheStatus::NotCacheable,
                key_hash: None,
            },
            limits: LimitInfo::default(),
            data: Some(json!({ "reason": reason })),
            error: None,
        }
    }

    pub(super) async fn execute_in_process_tool(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
        cache_status: CacheStatus,
    ) -> ToolResultEnvelope {
        let call_id = invocation.call_id.clone();
        let tool_id = invocation.tool_id.clone();
        let Some(definition) = self.registry.definition(&invocation.tool_id).cloned() else {
            return self.error_result(
                call_id,
                tool_id,
                ToolErrorCode::UnknownTool,
                "tool is not registered",
                false,
            );
        };
        let provider_id = definition.provider_id.clone();
        let Some(handler) = definition.handler else {
            return self.error_result(
                call_id,
                tool_id,
                ToolErrorCode::UnknownTool,
                "tool has no handler",
                false,
            );
        };
        let ToolArgs::Raw(args) = invocation.args else {
            return self.error_result(
                call_id,
                tool_id,
                ToolErrorCode::InvalidArgs,
                "custom tool requires raw JSON arguments",
                false,
            );
        };
        let context = CustomToolContext {
            session_id: invocation.session_id,
            turn_id: invocation.turn_id,
            call_id: call_id.clone(),
            tool_id: tool_id.clone(),
            snapshot_id: self.snapshot.snapshot_id.clone(),
            provider_resources: definition.provider_resources.clone(),
        };
        let output =
            match tokio::spawn(async move { handler.execute(context, args, cancel).await }).await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => return self.runtime_error_result(call_id, tool_id, error),
                Err(error) if error.is_panic() => {
                    return self.error_result(
                        call_id,
                        tool_id,
                        ToolErrorCode::Internal,
                        "in-process tool provider panicked",
                        false,
                    )
                }
                Err(_) => {
                    return self.error_result(
                        call_id,
                        tool_id,
                        ToolErrorCode::Internal,
                        "in-process tool provider task failed",
                        false,
                    )
                }
            };
        self.provider_output_result(
            call_id,
            tool_id,
            provider_id,
            cache_status,
            &invocation.capabilities,
            output,
        )
    }

    pub(super) fn provider_output_result(
        &self,
        call_id: ToolCallId,
        tool_id: ToolId,
        provider_id: ToolProviderId,
        cache_status: CacheStatus,
        capabilities: &CapabilitySet,
        output: CustomToolOutput,
    ) -> ToolResultEnvelope {
        let mut limits = output.limits;
        let data = output.data.map(|data| self.redactor.redact_value(data));
        let data_bytes = data
            .as_ref()
            .and_then(|data| serde_json::to_vec(data).ok())
            .map_or(0, |bytes| bytes.len());
        let prepared_artifact = match output.artifact {
            Some(artifact) => {
                if !capabilities.artifact_access.write {
                    return self.error_result(
                        call_id,
                        tool_id,
                        ToolErrorCode::ToolNotAllowed,
                        "tool artifact write is not allowed for this session",
                        false,
                    );
                }
                if artifact.content.len() > self.limits.max_tool_artifact_bytes {
                    return self.error_result(
                        call_id,
                        tool_id,
                        ToolErrorCode::TooLarge,
                        "tool artifact exceeds runtime artifact limit",
                        false,
                    );
                }
                let raw_content = artifact.content;
                let redacted_content = self.redactor.redact(&raw_content);
                Some((artifact.key, raw_content, redacted_content))
            }
            None => None,
        };
        let artifact_bytes = prepared_artifact
            .as_ref()
            .map_or(0, |(_, _, redacted_content)| redacted_content.len());
        let output_bytes = data_bytes.saturating_add(artifact_bytes);
        if output_bytes > self.limits.max_tool_output_bytes {
            return self.error_result(
                call_id,
                tool_id,
                ToolErrorCode::TooLarge,
                "tool output exceeds runtime output limit",
                false,
            );
        }
        let artifact_id = prepared_artifact.map(|(key, raw_content, redacted_content)| {
            self.artifacts
                .insert_views(key, raw_content, redacted_content)
        });
        limits.output_bytes = output_bytes;
        ToolResultEnvelope {
            ok: true,
            tool_call_id: call_id,
            tool_name: tool_id,
            provider_id,
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id,
            cache: CacheInfo {
                status: cache_status,
                key_hash: None,
            },
            limits,
            data,
            error: None,
        }
    }

    pub(crate) fn error_result(
        &self,
        call_id: ToolCallId,
        tool_name: ToolId,
        code: ToolErrorCode,
        message: &str,
        retryable: bool,
    ) -> ToolResultEnvelope {
        self.counters.tool_errors.fetch_add(1, Ordering::Relaxed);
        ToolResultEnvelope {
            ok: false,
            tool_call_id: call_id,
            provider_id: self.provider_id_for_tool(&tool_name),
            tool_name,
            snapshot_id: self.snapshot.snapshot_id.clone(),
            artifact_id: None,
            cache: CacheInfo {
                status: CacheStatus::NotCacheable,
                key_hash: None,
            },
            limits: LimitInfo::default(),
            data: None,
            error: Some(ToolErrorInfo {
                code,
                message: message.to_string(),
                retryable,
                partial: false,
            }),
        }
    }

    pub(super) fn runtime_error_result(
        &self,
        call_id: ToolCallId,
        tool_name: ToolId,
        error: RuntimeError,
    ) -> ToolResultEnvelope {
        match error {
            RuntimeError::RepoAccessDenied => self.error_result(
                call_id,
                tool_name,
                ToolErrorCode::PathDenied,
                "path denied by repo policy",
                false,
            ),
            RuntimeError::LimitExceeded { .. } => self.error_result(
                call_id,
                tool_name,
                ToolErrorCode::TooLarge,
                "resource limit exceeded",
                false,
            ),
            RuntimeError::SnapshotStale { .. } => self.error_result(
                call_id,
                tool_name,
                ToolErrorCode::SnapshotStale,
                "snapshot content changed after capture",
                false,
            ),
            RuntimeError::Cancelled => self.error_result(
                call_id,
                tool_name,
                ToolErrorCode::Cancelled,
                "operation cancelled",
                true,
            ),
            RuntimeError::Timeout => self.error_result(
                call_id,
                tool_name,
                ToolErrorCode::Timeout,
                "tool provider timed out",
                true,
            ),
            RuntimeError::InvalidInput(message)
                if message.contains("not text") || message.contains("UTF-8") =>
            {
                self.error_result(call_id, tool_name, ToolErrorCode::NotText, &message, false)
            }
            RuntimeError::InvalidInput(message) => self.error_result(
                call_id,
                tool_name,
                ToolErrorCode::InvalidArgs,
                &message,
                false,
            ),
            _ => self.error_result(
                call_id,
                tool_name,
                ToolErrorCode::Internal,
                "internal tool error",
                false,
            ),
        }
    }

    fn read_error_result(
        &self,
        call_id: ToolCallId,
        tool_name: ToolId,
        path: &RepoPath,
        error: RuntimeError,
    ) -> ToolResultEnvelope {
        let mut result = self.runtime_error_result(call_id, tool_name, error);
        if matches!(
            result.error.as_ref().map(|error| error.code),
            Some(
                ToolErrorCode::NotText
                    | ToolErrorCode::TooLarge
                    | ToolErrorCode::PathDenied
                    | ToolErrorCode::NotFound
            )
        ) {
            result.data = Some(json!({
                "path": path.display(),
                "available": false,
            }));
        }
        result
    }

    fn read_unavailable_result(
        &self,
        call_id: ToolCallId,
        tool_name: ToolId,
        code: ToolErrorCode,
        path: &RepoPath,
        message: &str,
        retryable: bool,
    ) -> ToolResultEnvelope {
        let mut result = self.error_result(call_id, tool_name, code, message, retryable);
        result.data = Some(json!({
            "path": path.display(),
            "available": false,
        }));
        result
    }

    fn provider_id_for_tool(&self, tool_name: &ToolId) -> ToolProviderId {
        self.registry
            .definition(tool_name)
            .map(|definition| definition.provider_id.clone())
            .unwrap_or_else(ToolProviderId::runtime)
    }

    pub(crate) fn snapshot_counters(&self) -> ConcurrentCounters {
        self.counters.snapshot()
    }

    pub(crate) fn snapshot_tool_metrics(&self) -> BTreeMap<ToolMetricKey, ToolMetricsSnapshot> {
        self.metrics.snapshot()
    }

    pub(crate) fn snapshot_provider_health(&self) -> Vec<ToolProviderHealthSnapshot> {
        self.metrics.provider_health_snapshot()
    }

    pub(crate) fn record_tool_metrics(&self, results: &[ToolResultEnvelope]) {
        self.record_batch_metrics(results);
    }

    #[cfg(test)]
    pub(crate) fn inflight_tool_count_for_test(&self) -> usize {
        self.result_cache.inflight_count()
    }

    fn record_batch_metrics(&self, results: &[ToolResultEnvelope]) {
        for result in results {
            self.metrics.record(result);
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    (started.elapsed().as_micros().div_ceil(1000) as u64).max(1)
}

pub(super) fn elapsed_ms_allow_zero(started: Instant) -> u64 {
    started.elapsed().as_micros().div_ceil(1000) as u64
}

fn is_non_finding_claim(title: &str, claim: &str) -> bool {
    let text = format!("{title} {claim}").to_ascii_lowercase();
    let title = title.trim().to_ascii_lowercase();
    if title.starts_with("check ")
        || title.starts_with("verify ")
        || title.starts_with("consider ")
        || title.starts_with("potential ")
        || title.starts_with("possible ")
    {
        return true;
    }
    let clean_batch_phrases = [
        "no issue found",
        "no issues found",
        "no actionable",
        "no supported",
        "no concrete",
        "no security regression",
        "no correctness issue",
        "no finding",
        "no findings",
        "found no",
        "i found no",
        "does not support a specific",
        "does not support a concrete",
        "nothing actionable",
        "might be",
        "may be",
        "could be",
        "potentially",
        "correctness risk",
        "can surface as",
        "could surface as",
        "may surface as",
        "only safe if",
        "intermediate result handling",
        "intermediate consumers",
        "callers that expect",
        "verify that",
        "check that",
        "check runner",
    ];
    clean_batch_phrases
        .iter()
        .any(|phrase| text.contains(phrase))
}

fn weak_clean_review_for_risk_hinted_file(
    diff: &str,
    path: &RepoPath,
    verdict: &str,
    summary: &str,
) -> bool {
    if verdict.trim() != "clean" {
        return false;
    }
    if !strict_risk_hint_clean_gate_applies(path) {
        return false;
    }
    let risk_paths = diff_risk_hint_paths(diff);
    if !risk_paths.contains("*") && !risk_paths.contains(&path.display()) {
        return false;
    }
    let text = summary.to_ascii_lowercase();
    let weak_dismissals = [
        "pre-existing",
        "already-existing",
        "preexisting",
        "already present",
        "not modified",
        "not changed",
        "not introduce",
        "does not introduce",
        "existing async iteration",
        "existing pattern",
        "existing trust boundary",
        "existing trust boundaries",
        "existing cooked content pipeline",
        "only shows",
        "no concrete correctness regression",
        "no actionable correctness regression",
        "no actionable correctness defect",
        "no actionable regression",
        "no evidence",
        "not proven",
        "standard rails helpers",
        "framework/helpers",
        "handled by the framework",
        "surrounding controller enforces",
        "controller enforces",
        "could not tie",
        "could not connect",
        "could not link",
        "must be inspected",
        "fire-and-forget",
        "best-effort",
        "remaining foreach",
        "remaining foreach(async",
        "only changed behavior",
        "not relied upon",
        "not rely",
        "independent per-",
    ];
    weak_dismissals.iter().any(|phrase| text.contains(phrase))
}

fn strict_risk_hint_clean_gate_applies(path: &RepoPath) -> bool {
    let path = path.display();
    !(path.starts_with("spec/")
        || path.starts_with("test/")
        || path.contains("/spec/")
        || path.contains("/test/")
        || path.starts_with("db/migrate/"))
}

fn inconsistent_clean_review(verdict: &str, summary: &str) -> bool {
    if verdict.trim() != "clean" {
        return false;
    }
    let text = summary.to_ascii_lowercase();
    let issue_phrases = [
        "never awaits",
        "not awaited",
        "without awaiting",
        "not collected",
        "not aggregated",
        "can return before",
        "can continue before",
        "continues before",
        "rejection is not observed",
        "rejections are not observed",
        "not caught by the surrounding try/catch",
        "failures are not caught",
        "errors escape",
        "dropped promise",
        "dropped promises",
        "drops async",
        "drops promise",
        "drops promises",
        "loses async",
        "loses cleanup",
        "lose cleanup",
        "lost before completion",
        "fire-and-forget",
        "leaves stale",
        "remain uncleared",
    ];
    issue_phrases.iter().any(|phrase| text.contains(phrase))
}

fn inconsistent_issue_found_review(verdict: &str, summary: &str) -> bool {
    if verdict.trim() != "issue_found" {
        return false;
    }
    let text = summary.to_ascii_lowercase();
    let clean_phrases = [
        "no actionable",
        "no additional",
        "no correctness issue",
        "no concrete",
        "no issue",
        "no bug",
        "did not find",
        "didn't find",
        "not find",
        "was not found",
        "without an observable",
    ];
    clean_phrases.iter().any(|phrase| text.contains(phrase))
}

fn inconsistent_skipped_review(verdict: &str, summary: &str) -> bool {
    if verdict.trim() != "skipped" {
        return false;
    }
    let text = summary.to_ascii_lowercase();
    let inspected_phrases = [
        "inspected",
        "reviewed",
        "checked",
        "no actionable",
        "no correctness issue",
        "no issue",
        "no concrete",
        "clean",
        "best-effort",
        "does not",
    ];
    if inspected_phrases.iter().any(|phrase| text.contains(phrase)) {
        return true;
    }
    let skip_reasons = [
        "could not inspect",
        "cannot inspect",
        "unable to inspect",
        "not available",
        "deleted file",
        "binary file",
        "too large",
        "path denied",
        "missing from snapshot",
        "read failed",
    ];
    !skip_reasons.iter().any(|phrase| text.contains(phrase))
}

fn finding_primary_path(finding: &FindingV1) -> String {
    finding
        .file_refs
        .iter()
        .map(|location| match location {
            EvidenceLocationV1::SinglePath { path } => path.clone(),
            EvidenceLocationV1::Rename { new_path, .. } => new_path.clone(),
        })
        .next()
        .unwrap_or_else(|| "<unknown>".to_string())
}

fn assigned_changed_files(scope: &SessionScope) -> Vec<RepoPath> {
    scope
        .instructions
        .iter()
        .filter(|instruction| instruction.trusted && instruction.kind == "changed_file_batch")
        .flat_map(|instruction| changed_files_from_batch_instruction(&instruction.text))
        .filter_map(|path| RepoPath::parse(&path).ok())
        .collect()
}

fn changed_files_from_batch_instruction(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (prefix, path) = trimmed.split_once(". ")?;
            prefix.parse::<usize>().ok()?;
            let path = path.trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

fn scoped_diff_content(diff: &str, assigned_paths: &[RepoPath]) -> (String, Vec<String>) {
    if assigned_paths.is_empty() {
        return (diff.to_string(), Vec::new());
    }

    let assigned = assigned_paths
        .iter()
        .map(RepoPath::display)
        .collect::<BTreeSet<_>>();
    let mut selected = Vec::new();
    let mut current = Vec::new();
    let mut include_current = false;
    let mut included_paths = BTreeSet::new();

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            flush_diff_section(&mut selected, &mut current, include_current);
            include_current = diff_git_line_matches(line, &assigned, &mut included_paths);
        } else if line.starts_with("+++ b/") || line.starts_with("--- a/") {
            if let Some(path) = line.get(6..) {
                if assigned.contains(path) {
                    include_current = true;
                    included_paths.insert(path.to_string());
                }
            }
        }
        current.push(line.to_string());
    }
    flush_diff_section(&mut selected, &mut current, include_current);

    if selected.is_empty() {
        return (diff.to_string(), Vec::new());
    }

    (
        selected.join("\n") + "\n",
        included_paths.into_iter().collect(),
    )
}

fn flush_diff_section(selected: &mut Vec<String>, current: &mut Vec<String>, include: bool) {
    if include {
        selected.extend(std::mem::take(current));
    } else {
        current.clear();
    }
}

fn diff_git_line_matches(
    line: &str,
    assigned: &BTreeSet<String>,
    included_paths: &mut BTreeSet<String>,
) -> bool {
    let mut matched = false;
    for part in line.split_whitespace().skip(2) {
        let Some(path) = part.strip_prefix("a/").or_else(|| part.strip_prefix("b/")) else {
            continue;
        };
        if assigned.contains(path) {
            included_paths.insert(path.to_string());
            matched = true;
        }
    }
    matched
}

fn validation_error_message(code: ToolErrorCode) -> &'static str {
    match code {
        ToolErrorCode::ToolNotAllowed => "tool is not allowed for this session",
        ToolErrorCode::PathDenied => "path is outside the session filesystem scope",
        ToolErrorCode::TooLarge => "tool arguments exceed this session input capability",
        ToolErrorCode::UnknownTool => "tool is not registered",
        ToolErrorCode::InvalidArgs => "invalid tool invocation",
        _ => "tool invocation failed validation",
    }
}
