//! Lookups for `ListSymbols`, and for R2.15, R2.16 and R3.38: a path's
//! occurrences, and a name's or symbol's across a tree.
//!
//! Each file answers from one source: its SCIP entry when an upload covers
//! its exact content, else its tags, unless the caller forces one. A tags
//! entry a path query needs and does not find is extracted on the spot, up
//! to [`MAX_INLINE`] files. A name or symbol query needs the tree complete,
//! and keeps a stored occurrence only when the tree holds that file id at
//! that path, so older versions of a file never leak into a tree's results.
//! The store reads its rows filtered and in path order, so that check runs
//! only as far as the page (R2.16).

use std::cmp::Ordering;
use std::collections::HashMap;

use reef_lib::jj::files::{self, Tree, TreeFile};
use reef_symbols::{Kind, Language, Occurrence, Role};

use super::index::extract_missing;
use super::store::{EntryRow, Found, Ordered, RefKey, Source, Store};
use super::{IndexStatus, State, SymbolError, SymbolIndex};

/// Files a path query extracts inline at most.
pub(crate) const MAX_INLINE: usize = 1000;

/// What to keep.
#[derive(Debug, Clone, Default)]
pub(crate) struct Filters {
    /// Empty: both.
    pub roles: Vec<Role>,
    /// Empty: all.
    pub kinds: Vec<Kind>,
    /// `None`: per file.
    pub source: Option<Source>,
}

impl Filters {
    fn keeps(&self, o: &Occurrence) -> bool {
        (self.roles.is_empty() || self.roles.contains(&o.role))
            && (self.kinds.is_empty() || self.kinds.contains(&o.kind))
    }
}

/// One occurrence as listed.
#[derive(Debug, Clone)]
pub(crate) struct Listed {
    pub path: String,
    pub seq: u32,
    pub occurrence: Occurrence,
    pub enclosing_symbol: Option<String>,
    pub source: Source,
    pub language: String,
}

/// A page of occurrences.
#[derive(Debug, Clone, Default)]
pub(crate) struct Page {
    pub items: Vec<Listed>,
    /// More follow the last item.
    pub more: bool,
}

/// jj's path order: by component, so `a/b` sorts before `a.rs`.
pub(crate) fn path_order(a: &str, b: &str) -> Ordering {
    a.split('/').cmp(b.split('/'))
}

/// Whether `path` is `dir` or below it, by whole components; every path is
/// under the empty one.
pub(crate) fn under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The entry a file answers from: SCIP when one covers it and `source`
/// allows it, else its current tags entry (`Ok(None)`: none yet).
fn entry_for(
    store: &Store,
    file: &TreeFile,
    source: Option<Source>,
) -> Result<Option<EntryRow>, SymbolError> {
    if source != Some(Source::Tags)
        && let Some(row) = store.entry(&file.path, &file.file_id, Source::Scip, None)?
    {
        return Ok(Some(row));
    }
    if source == Some(Source::Scip) {
        return Ok(None);
    }
    match SymbolIndex::extractor_for(&file.path) {
        Some(extractor) => {
            Ok(store.entry(&file.path, &file.file_id, Source::Tags, Some(&extractor))?)
        }
        None => Ok(None),
    }
}

/// Where a page starts: after this path's occurrence `seq`.
pub(crate) type Cursor<'a> = Option<(&'a str, u32)>;

/// The occurrences of the files at or under `dir` in `tree`, after
/// `after`, at most `limit`.
#[expect(clippy::too_many_arguments, reason = "one per request field")]
pub(crate) fn by_path(
    index: &SymbolIndex,
    store: &Store,
    tree: &Tree,
    commit_id: &str,
    dir: &str,
    filters: &Filters,
    after: Cursor<'_>,
    limit: usize,
) -> Result<Page, SymbolError> {
    let in_scope = |path: &str| {
        under(path, dir)
            && after.is_none_or(|(cursor, _)| path_order(path, cursor) != Ordering::Less)
    };
    let scope: Vec<TreeFile> = files::walk_files_in(tree, dir, &in_scope)?
        .into_iter()
        .filter(TreeFile::is_file)
        .collect();
    let mut missing = Vec::new();
    let mut entries = Vec::with_capacity(scope.len());
    for file in &scope {
        let entry = entry_for(store, file, filters.source)?;
        if entry.is_none()
            && filters.source != Some(Source::Scip)
            && Language::from_path(&file.path).is_some()
        {
            missing.push(file.clone());
        }
        entries.push(entry);
    }
    if missing.len() > MAX_INLINE {
        return Err(SymbolError::NotIndexed {
            commit_id: commit_id.to_owned(),
            missing: u64::try_from(missing.len()).unwrap_or(u64::MAX),
        });
    }
    if !missing.is_empty() {
        extract_missing(index, store, tree, &missing, |_| {})?;
        for (file, entry) in scope.iter().zip(entries.iter_mut()) {
            if entry.is_none() {
                *entry = entry_for(store, file, filters.source)?;
            }
        }
    }

    let mut page = Page::default();
    let mut touched = Vec::new();
    for (file, entry) in scope.iter().zip(entries) {
        let Some(entry) = entry else {
            continue;
        };
        // The entry and its occurrences from one read: an upload replacing
        // it in between cannot make the file look empty.
        let extractor = match entry.source {
            Source::Tags => SymbolIndex::extractor_for(&file.path),
            Source::Scip => None,
        };
        let Some((entry, occurrences)) = store.entry_with_occurrences(
            &file.path,
            &file.file_id,
            entry.source,
            extractor.as_deref(),
        )?
        else {
            continue;
        };
        touched.push(entry.id);
        let start = match after {
            Some((cursor, seq)) if cursor == file.path => seq as usize + 1,
            _ => 0,
        };
        for (seq, o) in occurrences.iter().enumerate().skip(start) {
            if !filters.keeps(o) {
                continue;
            }
            if page.items.len() == limit {
                page.more = true;
                break;
            }
            page.items.push(Listed {
                path: file.path.clone(),
                seq: u32::try_from(seq).unwrap_or(u32::MAX),
                occurrence: o.clone(),
                enclosing_symbol: o
                    .enclosing
                    .and_then(|e| occurrences.get(e as usize))
                    .and_then(|e| e.symbol.clone()),
                source: entry.source,
                language: entry.language.clone(),
            });
        }
        if page.more {
            break;
        }
    }
    store.touch(&touched)?;
    Ok(page)
}

/// Supported files of `tree` without a current tags entry.
fn missing(store: &Store, tree: &Tree) -> Result<u64, SymbolError> {
    let supported = |path: &str| Language::from_path(path).is_some();
    let files: Vec<TreeFile> = files::walk_files(tree, &supported)?
        .into_iter()
        .filter(TreeFile::is_file)
        .collect();
    let keys: Vec<(&str, &[u8])> = files
        .iter()
        .map(|f| (f.path.as_str(), f.file_id.as_slice()))
        .collect();
    let entries = store.entries(&keys, Source::Tags, &SymbolIndex::extractor_for)?;
    Ok(entries.iter().filter(|e| e.is_none()).count() as u64)
}

/// What a name or symbol query looks for.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Lookup<'a> {
    Name(&'a str),
    Symbol(&'a str),
}

/// The occurrences of `lookup` in `tree`, under `dir`, after `after`, at
/// most `limit`. Needs the tree complete (`status`).
///
/// The store filters the rows and reads them in path order, starting after
/// the cursor; each is checked against the tree, and the read stops at the
/// first kept row past the page. A page costs one statement and a tree
/// lookup per path it meets, not one per stored hit.
#[expect(clippy::too_many_arguments, reason = "one per request field")]
pub(crate) fn by_lookup(
    store: &Store,
    tree: &Tree,
    status: &IndexStatus,
    lookup: Lookup<'_>,
    dir: &str,
    filters: &Filters,
    after: Cursor<'_>,
    limit: usize,
) -> Result<Page, SymbolError> {
    if status.state != State::Complete {
        return Err(SymbolError::NotIndexed {
            commit_id: status.commit_id.clone(),
            missing: missing(store, tree)?,
        });
    }
    let key = match lookup {
        Lookup::Name(name) => RefKey::Name(name),
        Lookup::Symbol(symbol) => RefKey::Symbol(symbol),
    };
    #[cfg(any(test, feature = "testing"))]
    if testing::REFERENCE.load(std::sync::atomic::Ordering::Relaxed) {
        return testing::by_lookup(store, tree, key, dir, filters, after, limit);
    }
    let scope = Ordered {
        dir,
        roles: &filters.roles,
        kinds: &filters.kinds,
        source: filters.source,
        after,
    };
    let mut current = Current::new(store, tree);
    let mut page = Page::default();
    store.find_ordered(key, &scope, |hit| {
        // The store kept only rows of the file's own source and current
        // extractor; the tree decides the rest.
        if !current.holds(&hit)? {
            return Ok::<_, SymbolError>(true);
        }
        if page.items.len() == limit {
            page.more = true;
            return Ok(false);
        }
        page.items.push(Listed {
            path: hit.entry.path,
            seq: hit.seq,
            occurrence: hit.occurrence,
            enclosing_symbol: hit.enclosing_symbol,
            source: hit.entry.source,
            language: hit.entry.language,
        });
        Ok(true)
    })?;
    Ok(page)
}

/// Which stored occurrences belong to a tree: the file id at each path,
/// and whether an upload covers it, read once per path per request.
pub(crate) struct Current<'a> {
    store: &'a Store,
    tree: &'a Tree,
    present: HashMap<String, Option<Vec<u8>>>,
    covered: HashMap<String, bool>,
}

impl<'a> Current<'a> {
    pub(crate) fn new(store: &'a Store, tree: &'a Tree) -> Self {
        Current {
            store,
            tree,
            present: HashMap::new(),
            covered: HashMap::new(),
        }
    }

    /// Whether the tree holds `hit`'s file id at its path, and the file
    /// answers from `hit`'s source: `source` when set, else SCIP when an
    /// upload covers the file and tags otherwise, by the current extractor.
    pub(crate) fn keeps(
        &mut self,
        hit: &Found,
        source: Option<Source>,
    ) -> Result<bool, SymbolError> {
        if !self.holds(hit)? {
            return Ok(false);
        }
        let path = &hit.entry.path;
        match (source, hit.entry.source) {
            (Some(want), got) if want != got => return Ok(false),
            (None, Source::Tags) => {
                if !self.covered.contains_key(path) {
                    let scip = self
                        .store
                        .entry(path, &hit.file_id, Source::Scip, None)?
                        .is_some();
                    self.covered.insert(path.clone(), scip);
                }
                if self.covered[path] {
                    return Ok(false);
                }
            }
            _ => {}
        }
        Ok(hit.entry.source != Source::Tags
            || SymbolIndex::extractor_for(path).as_deref() == Some(hit.entry.extractor.as_str()))
    }

    /// Whether the tree holds `hit`'s file id at its path.
    fn holds(&mut self, hit: &Found) -> Result<bool, SymbolError> {
        let path = &hit.entry.path;
        if !self.present.contains_key(path) {
            #[cfg(any(test, feature = "testing"))]
            testing::TREE_LOOKUPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let file = pollster::block_on(files::tree_file(self.tree, path))?;
            self.present.insert(
                path.clone(),
                file.filter(TreeFile::is_file).map(|f| f.file_id),
            );
        }
        Ok(self.present[path].as_deref() == Some(hit.file_id.as_slice()))
    }
}

/// Test hooks, for this process: integration tests run one per process.
#[cfg(any(test, feature = "testing"))]
mod testing {
    use std::cmp::Ordering;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use reef_lib::jj::files::Tree;

    use super::{Current, Cursor, Filters, Listed, Page, path_order, under};
    use crate::symbols::store::{RefKey, Store};
    use crate::symbols::{SymbolError, SymbolIndex};

    /// Name and symbol queries answer from [`by_lookup`].
    pub(super) static REFERENCE: AtomicBool = AtomicBool::new(false);
    /// Paths [`Current`] has looked up in a tree.
    pub(super) static TREE_LOOKUPS: AtomicU64 = AtomicU64::new(0);

    impl SymbolIndex {
        /// Paths name and symbol queries have looked up in a tree, in this
        /// process.
        pub fn tree_lookups() -> u64 {
            TREE_LOOKUPS.load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Answers name and symbol queries in this process as R2.14 did,
        /// reading and checking every stored row before sorting: the
        /// reference the push-down is compared against.
        pub fn answer_lookups_by_reference(on: bool) {
            REFERENCE.store(on, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// R2.14's `by_lookup`: every stored row of the key, filtered and
    /// tree-checked in Rust, then sorted by path and cut to the page.
    pub(super) fn by_lookup(
        store: &Store,
        tree: &Tree,
        key: RefKey<'_>,
        dir: &str,
        filters: &Filters,
        after: Cursor<'_>,
        limit: usize,
    ) -> Result<Page, SymbolError> {
        let found = store
            .find_rows(&[key], None, None)?
            .pop()
            .unwrap_or_default();
        let mut current = Current::new(store, tree);
        let mut items = Vec::new();
        for hit in found {
            let path = hit.entry.path.clone();
            if !under(&path, dir) || !filters.keeps(&hit.occurrence) {
                continue;
            }
            if after.is_some_and(|(cursor, seq)| match path_order(&path, cursor) {
                Ordering::Less => true,
                Ordering::Equal => hit.seq <= seq,
                Ordering::Greater => false,
            }) {
                continue;
            }
            if !current.keeps(&hit, filters.source)? {
                continue;
            }
            items.push(Listed {
                path,
                seq: hit.seq,
                occurrence: hit.occurrence,
                enclosing_symbol: hit.enclosing_symbol,
                source: hit.entry.source,
                language: hit.entry.language,
            });
        }
        items.sort_by(|a, b| path_order(&a.path, &b.path).then(a.seq.cmp(&b.seq)));
        let more = items.len() > limit;
        items.truncate(limit);
        Ok(Page { items, more })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_order_and_nest_by_component() {
        assert_eq!(path_order("a/b", "a.rs"), Ordering::Less);
        assert_eq!(path_order("a", "a/b"), Ordering::Less);
        assert!(under("src/a.py", "src"));
        assert!(under("src", "src"));
        assert!(!under("srcx/a.py", "src"));
        assert!(under("anything", ""));
    }
}
