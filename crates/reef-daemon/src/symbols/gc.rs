//! GC's symbols step (R2.14): the fifth per repository, after the four of
//! `maintenance/gc.rs`.
//!
//! 1. **Mark**: the live trees are every workspace's working-copy commit's
//!    in the view, deduplicated. The first is walked and every entry it
//!    uses marked; the others are diffed from it, so only what differs is
//!    walked again.
//! 2. **Sweep**: tree rows and entries last used before `keep_newer` that
//!    are not live, and every tags entry of an extractor that is not
//!    current, with the names and symbols no occurrence uses afterwards.
//! 3. **Cap**: while the file is larger than `[symbols] max_size`, unmarked
//!    entries go, least recently used first, 1,000 at a time; then the
//!    freed pages are returned. Marked entries are never evicted: a
//!    repository whose live trees alone pass the cap keeps them and logs a
//!    warning.
//!
//! A sweep or a cap can leave a `COMPLETE` tree incomplete; the next read
//! extracts what it needs.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use reef_lib::jj::files::{self, TreeFile};
use reef_symbols::Language;

use super::store::now_ms;
use super::{SymbolError, SymbolIndex};
use crate::daemon::AppState;
use crate::model::RepoId;

/// Entries the cap removes per transaction.
const EVICT_BATCH: usize = 1000;

/// What the step did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SymbolsGc {
    /// The file's size before.
    pub bytes_before: u64,
    /// And after.
    pub bytes_after: u64,
    /// Entries the sweep deleted.
    pub swept: u64,
    /// Entries the cap evicted.
    pub evicted: u64,
}

fn supported(path: &str) -> bool {
    Language::from_path(path).is_some()
}

/// Runs the step on `repo`, deleting nothing used at or after
/// `keep_newer`. Does nothing when the index is off or the repository has
/// no cache file.
pub(crate) async fn run(
    state: &AppState,
    repo: RepoId,
    keep_newer: SystemTime,
    dry_run: bool,
) -> Result<Option<SymbolsGc>, SymbolError> {
    let index: &Arc<SymbolIndex> = &state.symbols;
    if !index.enabled() || dry_run || !index.path_of(repo).exists() {
        return Ok(None);
    }
    let store = index.store(repo)?;
    let handle = state.repos.get(repo).ok_or(SymbolError::NotOpen(repo))?;
    let loaded = state.reads.current(&handle).await?;
    let max_size = state.config.file.symbols.max_size.0;
    let before = keep_newer
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    tokio::task::spawn_blocking(move || {
        let marked = now_ms();
        let bytes_before = store.bytes();
        let commits = files::working_copy_commits(&loaded);
        let mut keys = Vec::new();
        let mut used: Vec<(String, Vec<u8>)> = Vec::new();
        let mut first = None;
        for commit in &commits {
            let tree = pollster::block_on(files::commit_tree(&loaded, commit))?;
            keys.push(files::tree_key(&tree));
            let files: Vec<TreeFile> = match &first {
                None => files::walk_files(&tree, &supported)?,
                Some(first) => pollster::block_on(files::changed_files(first, &tree, &supported))?
                    .into_iter()
                    .filter_map(|c| c.after)
                    .collect(),
            };
            used.extend(
                files
                    .into_iter()
                    .filter(TreeFile::is_file)
                    .map(|f| (f.path, f.file_id)),
            );
            if first.is_none() {
                first = Some(tree);
            }
        }
        store.mark(&used, &keys, marked)?;
        let current = |language: &str| Language::from_name(language).map(reef_symbols::extractor);
        let swept = store.sweep(before.min(marked), &keys, &current)?;
        let (evicted, over) = store.cap(max_size, marked, &keys, EVICT_BATCH)?;
        if over {
            tracing::warn!(
                repo = %repo,
                max_size,
                bytes = store.bytes(),
                "the symbol index's live trees alone pass [symbols] max_size"
            );
        }
        let report = SymbolsGc {
            bytes_before,
            bytes_after: store.bytes(),
            swept,
            evicted,
        };
        tracing::info!(
            repo = %repo,
            bytes_before = report.bytes_before,
            bytes_after = report.bytes_after,
            swept_count = report.swept,
            evicted_count = report.evicted,
            entry_count = store.entry_count().unwrap_or(0),
            "symbol index collected"
        );
        Ok(Some(report))
    })
    .await
    .map_err(|error| SymbolError::Pass(error.to_string()))?
}
