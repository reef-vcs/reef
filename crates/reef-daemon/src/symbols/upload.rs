//! SCIP uploads: each document checked against the file at its path in the
//! revision's tree, then normalized and stored as that file's SCIP entry.
//!
//! The checks, in order, each a rejection of that document alone: a
//! repository path; unique in the request; present in the tree; a file or
//! an executable; within `[symbols] max_file_size`; blake3 equal to its
//! `content_digest`; then [`reef_symbols::scip::normalize`]'s own. The
//! digest is what makes an upload trustworthy: a document carries no hash
//! of the bytes it describes, and an index run before an edit would
//! otherwise land on the edited file with plausible ranges.

use std::collections::HashSet;

use reef_lib::jj::files::{self, Tree};
use reef_symbols::Language;
use reef_symbols::scip::Rejection;

use super::store::{NewEntry, Source, Store};
use super::{SymbolError, SymbolIndex};

/// Documents per request at most.
pub(crate) const MAX_DOCUMENTS: usize = 1000;

/// One document as uploaded.
#[derive(Debug, Clone)]
pub(crate) struct Document {
    pub path: String,
    pub content_digest: String,
    pub bytes: Vec<u8>,
}

/// Why a document was not stored. The values are the API's
/// `ScipRejection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    InvalidPath = 1,
    DuplicatePath = 2,
    PathNotFound = 3,
    NotAFile = 4,
    FileTooLarge = 5,
    ContentMismatch = 6,
    InvalidDocument = 7,
    TooManyOccurrences = 8,
    RangeOutOfBounds = 9,
}

/// One rejection.
#[derive(Debug, Clone)]
pub(crate) struct Rejected {
    pub path: String,
    pub reason: Reason,
    pub message: String,
}

/// What an upload did.
#[derive(Debug, Clone, Default)]
pub(crate) struct Outcome {
    pub accepted: u64,
    /// Accepted documents for files no earlier upload covered.
    pub newly_covered: u64,
    pub rejected: Vec<Rejected>,
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Checks and stores `documents` for files of `tree`, written by `tool`.
#[expect(
    clippy::too_many_lines,
    reason = "the checks in their order, one after another"
)]
pub(crate) fn run(
    index: &SymbolIndex,
    store: &Store,
    tree: &Tree,
    tool: (&str, &str),
    documents: Vec<Document>,
) -> Result<Outcome, SymbolError> {
    let (tool_name, tool_version) = tool;
    let limits = *index.limits();
    let mut outcome = Outcome::default();
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for document in documents {
        let reject = |reason: Reason, message: String| Rejected {
            path: document.path.clone(),
            reason,
            message,
        };
        // A tree or blob that cannot be read fails the request; it says
        // nothing about the document.
        let mut failed: Option<SymbolError> = None;
        let checked = (|| {
            if let Err(error) = files::check_path(&document.path, false) {
                return Err(reject(Reason::InvalidPath, error.to_string()));
            }
            if !seen.insert(document.path.clone()) {
                return Err(reject(
                    Reason::DuplicatePath,
                    "another document has this path".into(),
                ));
            }
            let file = pollster::block_on(files::tree_file(tree, &document.path))
                .map_err(|error| {
                    failed = Some(error.into());
                    reject(Reason::PathNotFound, String::new())
                })?
                .ok_or_else(|| reject(Reason::PathNotFound, "nothing is at the path".into()))?;
            if !file.is_file() {
                return Err(reject(
                    Reason::NotAFile,
                    format!("a {} is at the path", file.kind),
                ));
            }
            let bytes = pollster::block_on(files::read_file_id(
                tree,
                &document.path,
                &file.file_id,
                limits.max_file_size,
            ))
            .map_err(|error| {
                failed = Some(error.into());
                reject(Reason::PathNotFound, String::new())
            })?
            .ok_or_else(|| {
                reject(
                    Reason::FileTooLarge,
                    format!("larger than {} bytes", limits.max_file_size),
                )
            })?;
            if !is_digest(&document.content_digest)
                || reef_symbols::content_digest(&bytes) != document.content_digest
            {
                return Err(reject(
                    Reason::ContentMismatch,
                    "the file's blake3 is not content_digest: the index describes other bytes"
                        .into(),
                ));
            }
            let language = Language::from_path(&document.path);
            let tags = language
                .and_then(|l| reef_symbols::extract(l, &document.path, &bytes, &limits).ok());
            let symbols = reef_symbols::scip::normalize(
                &document.bytes,
                tool_name,
                &bytes,
                tags.as_ref(),
                &limits,
            )
            .map_err(|rejection| {
                let reason = match rejection {
                    Rejection::InvalidDocument => Reason::InvalidDocument,
                    Rejection::TooManyOccurrences => Reason::TooManyOccurrences,
                    Rejection::RangeOutOfBounds => Reason::RangeOutOfBounds,
                };
                reject(reason, rejection.to_string())
            })?;
            Ok((file, language, symbols))
        })();
        if let Some(error) = failed {
            return Err(error);
        }
        match checked {
            Err(rejected) => outcome.rejected.push(rejected),
            Ok((file, language, symbols)) => {
                if store
                    .entry(&document.path, &file.file_id, Source::Scip, None)?
                    .is_none()
                {
                    outcome.newly_covered += 1;
                }
                outcome.accepted += 1;
                entries.push(NewEntry {
                    path: document.path.clone(),
                    file_id: file.file_id,
                    source: Source::Scip,
                    extractor: format!("{tool_name}/{tool_version}"),
                    language: language.map_or_else(String::new, |l| l.name().to_owned()),
                    skip: None,
                    has_errors: false,
                    occurrences: symbols.occurrences,
                });
            }
        }
    }
    store.put(&entries)?;
    Ok(outcome)
}
