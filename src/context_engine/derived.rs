//! Content-hash keyed derived-data cache (R9).
//!
//! Everything the index build derives from one file's bytes — AST
//! chunks, skeleton views, parsed symbols — is a pure function of
//! (path, content, chunk budget) and a derivation version. The cache
//! stores that work keyed by content hash so re-indexing recomputes
//! only changed files; embedding vectors are cached per input text so
//! warm re-index spends nothing on providers.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::runtime::contracts::{stable_id, RuntimeError, RuntimeResult};

use super::chunking::{FileChunk, SkeletonView};
use super::index::CONTEXT_ENGINE_VERSION;
use super::syntax::ParsedSymbols;

/// Version of the chunk / skeleton / symbol derivation. Bumping it (or
/// `CONTEXT_ENGINE_VERSION`) invalidates all cached derived data.
pub const CONTEXT_CHUNKER_VERSION: &str = "1";

pub const CONTEXT_DERIVED_CACHE_SCHEMA_VERSION: &str = "muzen.context_derived_cache.v1";

pub fn derived_version_tag() -> String {
    format!("{CONTEXT_ENGINE_VERSION}/{CONTEXT_CHUNKER_VERSION}")
}

/// Per-file derived data: a pure function of (path, content, chunk
/// budget) under one derivation version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DerivedFileData {
    pub chunks: Vec<FileChunk>,
    /// Skeleton view per chunk, parallel to `chunks`. `None` when the
    /// chunk elides nothing or the view would not save tokens.
    pub skeletons: Vec<Option<SkeletonView>>,
    /// Body term counts per chunk (the chunk's lexical postings
    /// contribution), parallel to `chunks`.
    pub chunk_terms: Vec<BTreeMap<String, f32>>,
    /// Body term counts of the whole file, for files indexed as one
    /// whole-file evidence item (empty for chunked files).
    pub file_terms: BTreeMap<String, f32>,
    pub parsed: ParsedSymbols,
}

pub fn derived_file_key(path: &str, content_hash: &str, chunk_max_tokens: usize) -> String {
    stable_id(&[path, content_hash, &chunk_max_tokens.to_string()])
}

/// Key for one embedding vector: the provider/model identity plus the
/// exact embedded text (which already encodes path, summary, content).
pub fn derived_vector_key(provider_tag: &str, text: &str) -> String {
    stable_id(&[provider_tag, text])
}

pub trait ContextDerivedCache: Send + Sync + std::fmt::Debug {
    fn get_file(&self, key: &str) -> Option<DerivedFileData>;
    fn put_file(&self, key: &str, data: DerivedFileData);
    fn get_vector(&self, key: &str) -> Option<Vec<f32>>;
    fn put_vector(&self, key: &str, vector: Vec<f32>);
    /// Persist accumulated entries. In-memory backends: no-op.
    fn flush(&self) -> RuntimeResult<()> {
        Ok(())
    }
    /// True when a durable backend found unreadable data at open and
    /// recovered by starting empty (the build degrades to a full
    /// recompute and records a warning).
    fn recovered_from_corruption(&self) -> bool {
        false
    }
}

#[derive(Debug, Default)]
pub struct InMemoryContextDerivedCache {
    files: Mutex<HashMap<String, DerivedFileData>>,
    vectors: Mutex<HashMap<String, Vec<f32>>>,
}

impl InMemoryContextDerivedCache {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContextDerivedCache for InMemoryContextDerivedCache {
    fn get_file(&self, key: &str) -> Option<DerivedFileData> {
        self.files
            .lock()
            .expect("derived cache poisoned")
            .get(key)
            .cloned()
    }

    fn put_file(&self, key: &str, data: DerivedFileData) {
        self.files
            .lock()
            .expect("derived cache poisoned")
            .insert(key.to_string(), data);
    }

    fn get_vector(&self, key: &str) -> Option<Vec<f32>> {
        self.vectors
            .lock()
            .expect("derived cache poisoned")
            .get(key)
            .cloned()
    }

    fn put_vector(&self, key: &str, vector: Vec<f32>) {
        self.vectors
            .lock()
            .expect("derived cache poisoned")
            .insert(key.to_string(), vector);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DerivedCacheEntry<T> {
    used_at_unix: u64,
    value: T,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextDerivedCacheFile {
    schema_version: String,
    /// `CONTEXT_ENGINE_VERSION/CONTEXT_CHUNKER_VERSION` at write time.
    /// A mismatch at open discards every entry: bumping either version
    /// invalidates cleanly.
    version_tag: String,
    files: HashMap<String, DerivedCacheEntry<DerivedFileData>>,
    vectors: HashMap<String, DerivedCacheEntry<Vec<f32>>>,
}

/// Durable derived-data cache: one JSON file, loaded at open, persisted
/// on `flush`. Unreadable or corrupt content degrades to an empty cache
/// (full rebuild) instead of failing the build.
#[derive(Debug)]
pub struct FileContextDerivedCache {
    path: PathBuf,
    max_entries: usize,
    recovered: bool,
    /// Set on put (and at open after corruption) so a fully-warm build
    /// skips the persist entirely.
    dirty: AtomicBool,
    files: Mutex<HashMap<String, DerivedCacheEntry<DerivedFileData>>>,
    vectors: Mutex<HashMap<String, DerivedCacheEntry<Vec<f32>>>>,
}

impl FileContextDerivedCache {
    pub fn open(path: impl AsRef<Path>, max_entries: usize) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut recovered = false;
        let mut files = HashMap::new();
        let mut vectors = HashMap::new();
        if let Ok(contents) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<ContextDerivedCacheFile>(&contents) {
                Ok(stored) if stored.version_tag == derived_version_tag() => {
                    files = stored.files;
                    vectors = stored.vectors;
                }
                // Version mismatch: clean invalidation, not corruption.
                Ok(_) => {}
                Err(_) => recovered = true,
            }
        }
        Self {
            path,
            max_entries,
            recovered,
            dirty: AtomicBool::new(recovered),
            files: Mutex::new(files),
            vectors: Mutex::new(vectors),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Keep the `max_entries` most recently used entries; ties break on key
/// so pruning is deterministic for a given set of timestamps.
fn prune_to_cap<T: Clone>(
    entries: &HashMap<String, DerivedCacheEntry<T>>,
    max_entries: usize,
) -> HashMap<String, DerivedCacheEntry<T>> {
    if entries.len() <= max_entries {
        return entries.clone();
    }
    let mut ordered: Vec<(&String, &DerivedCacheEntry<T>)> = entries.iter().collect();
    ordered.sort_by(|(left_key, left), (right_key, right)| {
        right
            .used_at_unix
            .cmp(&left.used_at_unix)
            .then_with(|| left_key.cmp(right_key))
    });
    ordered
        .into_iter()
        .take(max_entries)
        .map(|(key, entry)| (key.clone(), entry.clone()))
        .collect()
}

impl ContextDerivedCache for FileContextDerivedCache {
    fn get_file(&self, key: &str) -> Option<DerivedFileData> {
        let mut files = self.files.lock().expect("derived cache poisoned");
        let entry = files.get_mut(key)?;
        entry.used_at_unix = now_unix();
        Some(entry.value.clone())
    }

    fn put_file(&self, key: &str, data: DerivedFileData) {
        self.dirty.store(true, Ordering::Relaxed);
        self.files.lock().expect("derived cache poisoned").insert(
            key.to_string(),
            DerivedCacheEntry {
                used_at_unix: now_unix(),
                value: data,
            },
        );
    }

    fn get_vector(&self, key: &str) -> Option<Vec<f32>> {
        let mut vectors = self.vectors.lock().expect("derived cache poisoned");
        let entry = vectors.get_mut(key)?;
        entry.used_at_unix = now_unix();
        Some(entry.value.clone())
    }

    fn put_vector(&self, key: &str, vector: Vec<f32>) {
        self.dirty.store(true, Ordering::Relaxed);
        self.vectors.lock().expect("derived cache poisoned").insert(
            key.to_string(),
            DerivedCacheEntry {
                used_at_unix: now_unix(),
                value: vector,
            },
        );
    }

    fn flush(&self) -> RuntimeResult<()> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let files = prune_to_cap(
            &*self.files.lock().expect("derived cache poisoned"),
            self.max_entries,
        );
        let vectors = prune_to_cap(
            &*self.vectors.lock().expect("derived cache poisoned"),
            self.max_entries,
        );
        let file = ContextDerivedCacheFile {
            schema_version: CONTEXT_DERIVED_CACHE_SCHEMA_VERSION.to_string(),
            version_tag: derived_version_tag(),
            files,
            vectors,
        };
        let contents = serde_json::to_string(&file).map_err(|error| {
            RuntimeError::InvalidInput(format!("failed to encode context derived cache: {error}"))
        })?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "failed to create context derived cache dir {}: {error}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&self.path, contents).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "failed to write context derived cache {}: {error}",
                self.path.display()
            ))
        })
    }

    fn recovered_from_corruption(&self) -> bool {
        self.recovered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data(marker: &str) -> DerivedFileData {
        DerivedFileData {
            chunks: vec![FileChunk {
                start_line: 1,
                end_line: 2,
                text: format!("fn {marker}() {{}}"),
                symbol_path: Some(format!("fn {marker}")),
                node_kind: "function_item".to_string(),
            }],
            skeletons: vec![None],
            chunk_terms: vec![BTreeMap::from([(marker.to_string(), 1.0)])],
            file_terms: BTreeMap::new(),
            parsed: ParsedSymbols::default(),
        }
    }

    #[test]
    fn file_cache_round_trips_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.json");
        let cache = FileContextDerivedCache::open(&path, 100);
        cache.put_file("key", sample_data("a"));
        cache.put_vector("vec", vec![0.5, -1.0]);
        cache.flush().unwrap();

        let reopened = FileContextDerivedCache::open(&path, 100);
        assert!(!reopened.recovered_from_corruption());
        assert_eq!(reopened.get_file("key"), Some(sample_data("a")));
        assert_eq!(reopened.get_vector("vec"), Some(vec![0.5, -1.0]));
    }

    #[test]
    fn version_tag_mismatch_discards_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.json");
        let cache = FileContextDerivedCache::open(&path, 100);
        cache.put_file("key", sample_data("a"));
        cache.flush().unwrap();
        let rewritten = std::fs::read_to_string(&path)
            .unwrap()
            .replace(&derived_version_tag(), "0.0.0/stale");
        std::fs::write(&path, rewritten).unwrap();

        let reopened = FileContextDerivedCache::open(&path, 100);
        assert!(!reopened.recovered_from_corruption());
        assert_eq!(reopened.get_file("key"), None);
    }

    #[test]
    fn corrupt_cache_file_recovers_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.json");
        std::fs::write(&path, "{not json").unwrap();

        let cache = FileContextDerivedCache::open(&path, 100);
        assert!(cache.recovered_from_corruption());
        assert_eq!(cache.get_file("key"), None);
        // The next flush replaces the corrupt file.
        cache.put_file("key", sample_data("a"));
        cache.flush().unwrap();
        let reopened = FileContextDerivedCache::open(&path, 100);
        assert!(!reopened.recovered_from_corruption());
        assert!(reopened.get_file("key").is_some());
    }

    #[test]
    fn flush_prunes_least_recently_used_beyond_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.json");
        let cache = FileContextDerivedCache::open(&path, 2);
        cache.put_file("a", sample_data("a"));
        cache.put_file("b", sample_data("b"));
        cache.put_file("c", sample_data("c"));
        // Same-second timestamps: the key tiebreak keeps `a` and `b`.
        cache.flush().unwrap();

        let reopened = FileContextDerivedCache::open(&path, 2);
        let kept = ["a", "b", "c"]
            .iter()
            .filter(|key| reopened.get_file(key).is_some())
            .count();
        assert_eq!(kept, 2);
    }

    #[test]
    fn derived_keys_separate_content_and_budget() {
        let base = derived_file_key("src/lib.rs", "hash1", 400);
        assert_ne!(base, derived_file_key("src/lib.rs", "hash2", 400));
        assert_ne!(base, derived_file_key("src/lib.rs", "hash1", 800));
        assert_ne!(base, derived_file_key("src/other.rs", "hash1", 400));
        assert_ne!(
            derived_vector_key("local_hash_256", "text"),
            derived_vector_key("hosted:model", "text")
        );
    }
}
