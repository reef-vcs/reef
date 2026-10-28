//! A pass over one tree: every supported file without an entry is
//! extracted, and the tree's counts are recorded as complete.
//!
//! When the first parent's tree is complete, the pass diffs from it,
//! starting from its counts and looking only at the files that changed;
//! otherwise it walks the whole tree. Either way a file whose entry exists
//! is never parsed again. Blobs are read and parsed on the index's own
//! threads, and written in batches of 64 files outside any lock held while
//! parsing.

use std::sync::{Arc, Mutex, PoisonError};

use rayon::prelude::*;
use reef_lib::jj::diff::EntryKind;
use reef_lib::jj::files::{self, Tree, TreeFile};
use reef_lib::jj::{CommitIdHex, ReadonlyRepo};
use reef_symbols::Language;

use super::store::{Contribution, Counts, NewEntry, SKIP_CONFLICTED, Source, Store};
use super::{IndexStatus, State, SymbolError, SymbolIndex};

/// Files per write.
const BATCH: usize = 64;

fn all(_: &str) -> bool {
    true
}

/// A file's tags entry, as its tree's counts see it.
#[derive(Debug, Clone, Copy)]
enum Tags {
    /// None yet.
    Missing,
    /// One, skipped for this reason or extracted.
    Entry(Option<u8>),
}

/// What `file` adds to its tree's counts, given its tags entry and whether
/// it has a SCIP one.
fn contribution(file: &TreeFile, tags: Tags, scip: bool) -> Contribution {
    let language = Language::from_path(&file.path).map(|l| l.name().to_owned());
    match file.kind {
        EntryKind::Conflict if language.is_some() => Contribution {
            language,
            indexed: false,
            skip: Some(SKIP_CONFLICTED),
            scip: false,
        },
        EntryKind::File | EntryKind::Executable => Contribution {
            indexed: language.is_some() && matches!(tags, Tags::Entry(_)),
            skip: match tags {
                Tags::Entry(reason) if language.is_some() => reason,
                _ => None,
            },
            language,
            scip,
        },
        _ => Contribution::default(),
    }
}

/// Extracts `missing` (supported files without a tags entry) on the
/// index's threads and writes their entries, calling `written` after each
/// batch with what it added. Returns each file's skip, in order.
pub(crate) fn extract_missing(
    index: &SymbolIndex,
    store: &Store,
    tree: &Tree,
    missing: &[TreeFile],
    mut written: impl FnMut(&[NewEntry]),
) -> Result<(), SymbolError> {
    for batch in missing.chunks(BATCH) {
        if index.stopping() {
            return Err(SymbolError::ShuttingDown);
        }
        let entries: Vec<Result<NewEntry, SymbolError>> = index.install(|| {
            batch
                .par_iter()
                .map(|file| extract_one(index, tree, file))
                .collect()
        });
        let entries: Vec<NewEntry> = entries.into_iter().collect::<Result<_, _>>()?;
        store.put(&entries)?;
        index.count_extracted(u64::try_from(entries.len()).unwrap_or(0));
        written(&entries);
    }
    Ok(())
}

/// Reads and extracts one supported file.
fn extract_one(index: &SymbolIndex, tree: &Tree, file: &TreeFile) -> Result<NewEntry, SymbolError> {
    let language = Language::from_path(&file.path)
        .ok_or_else(|| SymbolError::Pass(format!("{} is not a supported file", file.path)))?;
    let limits = index.limits();
    let bytes = pollster::block_on(files::read_file_id(
        tree,
        &file.path,
        &file.file_id,
        limits.max_file_size,
    ))?;
    let (skip, has_errors, occurrences) = match bytes {
        None => (Some(reef_symbols::Skip::TooLarge as u8), false, Vec::new()),
        Some(bytes) => match reef_symbols::extract(language, &file.path, &bytes, limits) {
            Ok(symbols) => (None, symbols.has_errors, symbols.occurrences),
            Err(skip) => (Some(skip as u8), false, Vec::new()),
        },
    };
    Ok(NewEntry {
        path: file.path.clone(),
        file_id: file.file_id.clone(),
        source: Source::Tags,
        extractor: reef_symbols::extractor(language),
        language: language.name().to_owned(),
        skip,
        has_errors,
        occurrences,
    })
}

/// One file's contribution as the index holds it now, for the side of a
/// diff that is going away.
fn current(store: &Store, file: &TreeFile) -> Result<Contribution, SymbolError> {
    if !file.is_file() {
        return Ok(contribution(file, Tags::Missing, false));
    }
    let extractor = SymbolIndex::extractor_for(&file.path);
    let tags = match &extractor {
        Some(extractor) => store
            .entry(&file.path, &file.file_id, Source::Tags, Some(extractor))?
            .map(|row| row.skip),
        None => None,
    };
    let scip = store
        .entry(&file.path, &file.file_id, Source::Scip, None)?
        .is_some();
    // An entry GC removed counts as indexed: the parent's pass found one.
    let tags = if extractor.is_some() {
        Tags::Entry(tags.flatten())
    } else {
        Tags::Missing
    };
    Ok(contribution(file, tags, scip))
}

/// Runs a pass over `commit`'s tree in `repo` and records it complete.
pub(crate) fn run(
    index: &SymbolIndex,
    store: &Store,
    repo: &Arc<ReadonlyRepo>,
    commit: &CommitIdHex,
    progress: &Mutex<Counts>,
) -> Result<IndexStatus, SymbolError> {
    let (tree, parent) = pollster::block_on(files::commit_and_parent_trees(repo, commit))?;
    let key = files::tree_key(&tree);
    let base = match &parent {
        Some(parent) => store
            .tree(&files::tree_key(parent))?
            .filter(|row| row.complete),
        None => None,
    };
    let mut counts = Counts::default();
    let considered: Vec<TreeFile> = match (&parent, base) {
        (Some(parent), Some(base)) => {
            counts = base.counts;
            let changed = pollster::block_on(files::changed_files(parent, &tree, &all))?;
            for before in changed.iter().filter_map(|c| c.before.as_ref()) {
                counts.add(&current(store, before)?, -1);
            }
            changed.into_iter().filter_map(|c| c.after).collect()
        }
        _ => files::walk_files(&tree, &all)?,
    };

    let mut missing = Vec::new();
    {
        let files: Vec<(&str, &[u8])> = considered
            .iter()
            .map(|f| (f.path.as_str(), f.file_id.as_slice()))
            .collect();
        let tags = store.entries(&files, Source::Tags, &SymbolIndex::extractor_for)?;
        let scip = store.entries(&files, Source::Scip, &|_| None)?;
        for ((file, tags), scip) in considered.iter().zip(tags).zip(scip) {
            let supported = Language::from_path(&file.path).is_some();
            if supported && file.is_file() && tags.is_none() {
                missing.push(file.clone());
                // Counted when written.
                if scip.is_some() {
                    counts.scip_files += 1;
                }
                continue;
            }
            let tags = tags.map_or(Tags::Missing, |row| Tags::Entry(row.skip));
            counts.add(&contribution(file, tags, scip.is_some()), 1);
        }
    }
    *progress.lock().unwrap_or_else(PoisonError::into_inner) = Counts {
        files: counts.files + u64::try_from(missing.len()).unwrap_or(0),
        ..counts.clone()
    };
    extract_missing(index, store, &tree, &missing, |entries| {
        for entry in entries {
            let file = TreeFile {
                path: entry.path.clone(),
                kind: EntryKind::File,
                file_id: entry.file_id.clone(),
            };
            counts.add(&contribution(&file, Tags::Entry(entry.skip), false), 1);
        }
        let mut shown = progress.lock().unwrap_or_else(PoisonError::into_inner);
        shown.indexed = counts.indexed;
        shown.skipped = counts.skipped.clone();
    })?;
    store.put_tree(&key, commit.as_str(), &counts)?;
    let row = store.tree(&key)?;
    Ok(IndexStatus {
        commit_id: commit.to_string(),
        state: State::Complete,
        counts,
        indexed_at: row.and_then(|row| row.indexed_at),
    })
}
