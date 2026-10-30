//! Symbol-level diffs (R2.15, ADR 0043): each differing file's added,
//! removed, changed and moved definitions, and the call sites in the new
//! tree of those that others may depend on.
//!
//! - **Entries**: each side of every differing file answers from one
//!   source, SCIP when every side that exists has an upload for its exact
//!   content, else tags; missing tags entries are extracted inline. At most
//!   [`RENAME_LIMIT`] files differ, or nothing is computed.
//! - **Matching** runs over the whole diff on every page
//!   (`reef_symbols::diff`), so a move from one page's file to another's is
//!   found. Line diffs, nesting, expansion and call sites are worked out
//!   for the page's own files only.
//! - **Call sites** need the new tree indexed: the handler waits for its
//!   pass between [`compute`] and [`call_sites`], holding no read permit.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use reef_lib::jj::diff::{
    self as jj_diff, ChangeKind as FileKind, DiffOptions, DiffPair, EntryKind, HunkInfo,
    RENAME_LIMIT,
};
use reef_lib::jj::files::{self, Tree, TreeFile};
use reef_symbols::diff::{self as matching, ChangeKind, Definition};
use reef_symbols::{Kind, Language, Occurrence, Range, Role};

use super::index::extract_missing;
use super::query::{Current, Listed, path_order};
use super::store::{EntryRow, Found, RefKey, SKIP_CONFLICTED, Source, Store};
use super::{IndexStatus, SymbolError, SymbolIndex};

/// Changes listed per file at most; the summary counts the rest.
pub(crate) const MAX_CHANGES_PER_FILE: usize = 1000;
/// Changes per page whose call sites are looked up at most.
pub(crate) const MAX_LOOKUPS_PER_PAGE: usize = 1000;
/// Candidate call sites counted per change at most.
pub(crate) const MAX_CANDIDATES: usize = 10_000;
/// Call sites listed per change when the request does not say.
pub(crate) const DEFAULT_CALL_SITES: u32 = 10;
/// Call sites listed per change at most.
pub(crate) const MAX_CALL_SITES: u32 = 100;
/// `expand_symbols` entries at most.
pub(crate) const MAX_EXPAND: usize = 100;

/// What a symbol diff asks for.
#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolRequest {
    /// Call sites listed per change; 0 looks none up.
    pub call_sites: usize,
    /// `expand_symbols`: empty for none.
    pub expand: Vec<String>,
    /// The source every pair answers from; `None`: per pair.
    pub source: Option<Source>,
}

/// Whether the index could answer for a file. The values are the API's
/// `SymbolCoverage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coverage {
    Covered = 1,
    Unsupported = 2,
    Skipped = 3,
    NoScip = 4,
    DiffTooLarge = 5,
}

/// Whether a change's call sites were looked up. The values are the API's
/// `CallSiteStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CallStatus {
    #[default]
    NotLooked = 0,
    Found = 1,
    Capped = 2,
    NotTracked = 3,
    IndexIncomplete = 4,
    PageLimit = 5,
}

/// What a change's call sites are looked up by.
#[derive(Debug, Clone)]
struct Lookup {
    /// Exact, in files answering from SCIP: a SCIP change's symbol.
    symbol: Option<String>,
    /// By name, in files answering from tags (and, for a tags change, in
    /// every file), with the reference and definition kinds that match.
    name: Option<(String, &'static [Kind], &'static [Kind])>,
    /// The change's language family.
    family: String,
    /// The changed definition's own range, whose references are not call
    /// sites.
    exclude: Option<(String, Range)>,
}

/// One definition that differs.
#[derive(Debug, Clone)]
pub(crate) struct SymbolChange {
    pub kind: ChangeKind,
    pub symbol: String,
    pub old_symbol: Option<String>,
    pub name: String,
    pub symbol_kind: Kind,
    pub enclosing_symbol: Option<String>,
    pub old_path: Option<String>,
    pub old_range: Option<Range>,
    pub new_range: Option<Range>,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub nested_only: bool,
    pub call_status: CallStatus,
    pub call_count: u32,
    pub call_sites: Vec<Listed>,
    pub name_definitions: u32,
    lookup: Option<Lookup>,
    /// The pair holding the old definition.
    old_pair: Option<usize>,
}

/// One page file's symbol changes.
#[derive(Debug, Clone)]
pub(crate) struct FileSymbols {
    pub coverage: Coverage,
    pub skip_reason: Option<u8>,
    pub source: Option<Source>,
    pub parse_errors: bool,
    pub changes: Vec<SymbolChange>,
    pub truncated: bool,
    /// With `expand_symbols`: the expanded symbols' lines; `None` when none
    /// of this file's changes is expanded.
    pub hunks: Option<Expanded>,
}

/// A file's expanded symbols' lines.
#[derive(Debug, Clone, Default)]
pub(crate) struct Expanded {
    /// In line order, numbered in file lines.
    pub hunks: Vec<HunkInfo>,
    /// Cut at the file's line or byte limit.
    pub truncated: bool,
    /// As `FileDiff`'s flags, for these hunks.
    pub old_no_final_newline: bool,
    pub new_no_final_newline: bool,
}

/// The changes of the whole diff.
#[derive(Debug, Clone, Default)]
pub(crate) struct Summary {
    pub added: u32,
    pub removed: u32,
    pub changed: u32,
    pub moved: u32,
    pub files_covered: u32,
    pub files_unsupported: u32,
    pub files_skipped: u32,
    pub diff_too_large: bool,
    /// The new tree's status as the call sites saw it.
    pub to_index: Option<IndexStatus>,
}

/// A page's symbol diff.
#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolDiff {
    /// By the page file's path.
    pub files: HashMap<String, FileSymbols>,
    pub summary: Summary,
}

impl SymbolDiff {
    /// Whether any change of the page waits for call sites.
    pub(crate) fn needs_lookups(&self) -> bool {
        self.files
            .values()
            .flat_map(|f| &f.changes)
            .any(|c| c.lookup.is_some())
    }

    /// Marks every change that waits for call sites `status`.
    pub(crate) fn mark_lookups(&mut self, status: CallStatus) {
        for change in self.files.values_mut().flat_map(|f| &mut f.changes) {
            if change.lookup.take().is_some() {
                change.call_status = status;
            }
        }
    }
}

/// The language family call sites are matched within: TypeScript, TSX and
/// JavaScript are one.
fn family(language: &str) -> &str {
    match language {
        "typescript" | "tsx" | "javascript" => "typescript",
        other => other,
    }
}

/// The reference kinds and the definition kinds a changed definition's
/// name is matched against, or `None` when tags record no references to
/// it.
fn kinds_for(kind: Kind) -> Option<(&'static [Kind], &'static [Kind])> {
    match kind {
        Kind::Function | Kind::Method => Some((&[Kind::Function], &[Kind::Function, Kind::Method])),
        Kind::Class | Kind::Struct | Kind::Interface | Kind::Enum | Kind::Type => Some((
            &[Kind::Class, Kind::Type, Kind::Interface],
            &[
                Kind::Class,
                Kind::Struct,
                Kind::Interface,
                Kind::Enum,
                Kind::Type,
            ],
        )),
        Kind::Macro => Some((&[Kind::Macro], &[Kind::Macro])),
        _ => None,
    }
}

/// A definition range's lines, 0-based, end exclusive.
fn lines_of(range: &Range) -> std::ops::Range<u32> {
    range.start_line..range.end_line + u32::from(range.end_col > 0)
}

/// How many lines a definition range spans.
fn line_count(range: &Range) -> u32 {
    let lines = lines_of(range);
    lines.end - lines.start
}

/// A file side by path and content id.
type FileKey = (String, Vec<u8>);

/// Each file side's tags and SCIP entries.
type Entries = HashMap<FileKey, (Option<EntryRow>, Option<EntryRow>)>;

/// One side of a pair as the index sees it.
#[derive(Debug, Clone)]
enum SideState {
    /// Conflicted.
    Conflicted,
    /// Not a file, or a file no source reads: no definitions.
    NoDefinitions,
    /// A file without an entry from the pair's source.
    NoEntry,
    /// A file's entry, which may be skipped.
    Entry(EntryRow),
}

/// What [`compute`] knows of one pair.
#[derive(Debug)]
struct PairState {
    coverage: Coverage,
    skip_reason: Option<u8>,
    source: Option<Source>,
    parse_errors: bool,
    /// Each side's occurrences, when covered.
    old: Vec<Occurrence>,
    new: Vec<Occurrence>,
    language: String,
}

fn is_file(file: &TreeFile) -> bool {
    matches!(file.kind, EntryKind::File | EntryKind::Executable)
}

fn supported(file: &TreeFile) -> bool {
    Language::from_path(&file.path).is_some()
}

/// The sides of `pair` whose definitions count: a copy's old side is
/// another file's, so only its new side does.
fn counted_old(pair: &DiffPair) -> Option<&TreeFile> {
    match pair.kind {
        FileKind::Copied => None,
        _ => pair.old.as_ref(),
    }
}

/// Each pair's entries: tags and SCIP rows of every file side, after
/// extracting missing tags entries.
fn entries(
    index: &SymbolIndex,
    store: &Store,
    tree: &Tree,
    pairs: &[DiffPair],
    source: Option<Source>,
) -> Result<Entries, SymbolError> {
    let mut sides: BTreeSet<FileKey> = BTreeSet::new();
    for pair in pairs {
        for file in [counted_old(pair), pair.new.as_ref()].into_iter().flatten() {
            if is_file(file) {
                sides.insert((file.path.clone(), file.file_id.clone()));
            }
        }
    }
    let keys: Vec<(&str, &[u8])> = sides
        .iter()
        .map(|(p, id)| (p.as_str(), id.as_slice()))
        .collect();
    let lookup = |keys: &[(&str, &[u8])]| -> Result<_, SymbolError> {
        Ok((
            store.entries(keys, Source::Tags, &SymbolIndex::extractor_for)?,
            store.entries(keys, Source::Scip, &|_| None)?,
        ))
    };
    let (mut tags, mut scip) = lookup(&keys)?;
    if source != Some(Source::Scip) {
        let missing: Vec<TreeFile> = keys
            .iter()
            .zip(&tags)
            .filter(|((path, _), row)| row.is_none() && Language::from_path(path).is_some())
            .map(|((path, id), _)| TreeFile {
                path: (*path).to_owned(),
                kind: EntryKind::File,
                file_id: id.to_vec(),
            })
            .collect();
        if !missing.is_empty() {
            extract_missing(index, store, tree, &missing, |_| {})?;
            (tags, scip) = lookup(&keys)?;
        }
    }
    Ok(sides.into_iter().zip(tags.into_iter().zip(scip)).collect())
}

/// One side's state under `source`.
fn side_state(file: &TreeFile, source: Source, entries: &Entries) -> SideState {
    if file.kind == EntryKind::Conflict {
        return SideState::Conflicted;
    }
    if !is_file(file) {
        return SideState::NoDefinitions;
    }
    let (tags, scip) = entries
        .get(&(file.path.clone(), file.file_id.clone()))
        .cloned()
        .unwrap_or((None, None));
    match source {
        Source::Scip => scip.map_or(SideState::NoEntry, SideState::Entry),
        Source::Tags if supported(file) => tags.map_or(SideState::NoEntry, SideState::Entry),
        Source::Tags => SideState::NoDefinitions,
    }
}

/// A pair's coverage and, when covered, both sides' occurrences.
fn pair_state(
    store: &Store,
    pair: &DiffPair,
    forced: Option<Source>,
    entries: &Entries,
) -> Result<PairState, SymbolError> {
    let sides: Vec<&TreeFile> = [counted_old(pair), pair.new.as_ref()]
        .into_iter()
        .flatten()
        .collect();
    let has_scip = |file: &TreeFile| {
        is_file(file)
            && entries
                .get(&(file.path.clone(), file.file_id.clone()))
                .is_some_and(|(_, scip)| scip.is_some())
    };
    let source = forced.unwrap_or_else(|| {
        if sides.iter().any(|f| is_file(f)) && sides.iter().all(|f| has_scip(f)) {
            Source::Scip
        } else {
            Source::Tags
        }
    });
    let mut state = PairState {
        coverage: Coverage::Covered,
        skip_reason: None,
        source: Some(source),
        parse_errors: false,
        old: Vec::new(),
        new: Vec::new(),
        language: String::new(),
    };
    let readable = sides
        .iter()
        .any(|f| (is_file(f) && supported(f)) || has_scip(f) || f.kind == EntryKind::Conflict);
    let uncovered = |coverage, skip_reason| PairState {
        coverage,
        skip_reason,
        source: None,
        parse_errors: false,
        old: Vec::new(),
        new: Vec::new(),
        language: String::new(),
    };
    let conflicted_supported = sides
        .iter()
        .any(|f| f.kind == EntryKind::Conflict && supported(f));
    if conflicted_supported {
        return Ok(uncovered(Coverage::Skipped, Some(SKIP_CONFLICTED)));
    }
    if !readable || (source == Source::Tags && !sides.iter().any(|f| is_file(f) && supported(f))) {
        return Ok(uncovered(Coverage::Unsupported, None));
    }
    let mut states = Vec::new();
    for (i, file) in [counted_old(pair), pair.new.as_ref()]
        .into_iter()
        .enumerate()
    {
        let Some(file) = file else {
            continue;
        };
        let side = side_state(file, source, entries);
        match &side {
            SideState::Conflicted => return Ok(uncovered(Coverage::Unsupported, None)),
            SideState::NoEntry if source == Source::Scip => {
                return Ok(uncovered(Coverage::NoScip, None));
            }
            SideState::NoEntry => return Ok(uncovered(Coverage::Skipped, None)),
            SideState::Entry(row) if row.skip.is_some() => {
                return Ok(uncovered(Coverage::Skipped, row.skip));
            }
            _ => {}
        }
        states.push((i, file, side));
    }
    for (i, file, side) in states {
        if !matches!(side, SideState::Entry(_)) {
            continue;
        }
        let extractor = match source {
            Source::Tags => SymbolIndex::extractor_for(&file.path),
            Source::Scip => None,
        };
        // A writer replacing the entry in between (GC, an upload) leaves
        // the file skipped for this request, not the diff failed.
        let Some((row, occurrences)) = store.entry_with_occurrences(
            &file.path,
            &file.file_id,
            source,
            extractor.as_deref(),
        )?
        else {
            return Ok(uncovered(Coverage::Skipped, None));
        };
        state.parse_errors |= row.has_errors;
        state.language.clone_from(&row.language);
        if i == 0 {
            state.old = occurrences;
        } else {
            state.new = occurrences;
        }
    }
    Ok(state)
}

/// One definition's place: its pair and side, and its occurrence.
#[derive(Debug, Clone, Copy)]
struct Place {
    pair: usize,
    seq: u32,
}

/// Both sides' bytes of the page's pairs, read once.
struct Blobs<'a> {
    tree: &'a Tree,
    max: u64,
    read: HashMap<FileKey, Option<Arc<[u8]>>>,
}

impl Blobs<'_> {
    /// A side's bytes, or none for an absent side or one past the cap.
    fn get_or_empty(&mut self, file: Option<&TreeFile>) -> Result<Arc<[u8]>, SymbolError> {
        Ok(self.get(file)?.unwrap_or_else(|| Arc::from([])))
    }

    fn get(&mut self, file: Option<&TreeFile>) -> Result<Option<Arc<[u8]>>, SymbolError> {
        let Some(file) = file.filter(|f| is_file(f)) else {
            return Ok(None);
        };
        let key = (file.path.clone(), file.file_id.clone());
        if let Some(bytes) = self.read.get(&key) {
            return Ok(bytes.clone());
        }
        let bytes = pollster::block_on(files::read_file_id(
            self.tree,
            &file.path,
            &file.file_id,
            self.max,
        ))?;
        self.read.insert(key, bytes.clone());
        Ok(bytes)
    }
}

/// Whether line `line` of a file split into `lines` is only whitespace.
fn blank(lines: &[&[u8]], line: u32) -> bool {
    lines
        .get(line as usize)
        .is_none_or(|text| text.iter().all(u8::is_ascii_whitespace))
}

/// Whether every non-blank line of `changed` lies inside a definition
/// nested directly in occurrence `seq`.
fn inside_children(occurrences: &[Occurrence], seq: u32, lines: &[&[u8]], changed: &[u32]) -> bool {
    let children: Vec<std::ops::Range<u32>> = occurrences
        .iter()
        .filter(|o| o.role == Role::Definition && o.enclosing == Some(seq))
        .filter_map(|o| o.definition_range.as_ref().map(lines_of))
        .collect();
    changed
        .iter()
        .filter(|&&line| !blank(lines, line))
        .all(|line| children.iter().any(|c| c.contains(line)))
}

/// The whole diff's definitions, matched.
struct Matched<'a> {
    pairs: &'a [DiffPair],
    states: Vec<PairState>,
    old: Vec<Definition>,
    new: Vec<Definition>,
    old_at: Vec<Place>,
    new_at: Vec<Place>,
}

impl Matched<'_> {
    /// Every covered pair's definitions, on both sides.
    fn new(pairs: &[DiffPair], states: Vec<PairState>) -> Matched<'_> {
        let mut matched = Matched {
            pairs,
            states,
            old: Vec::new(),
            new: Vec::new(),
            old_at: Vec::new(),
            new_at: Vec::new(),
        };
        for (i, pair) in pairs.iter().enumerate() {
            let state = &matched.states[i];
            if state.coverage != Coverage::Covered {
                continue;
            }
            if let Some(file) = counted_old(pair) {
                for d in matching::definitions(&file.path, &state.old) {
                    matched.old_at.push(Place {
                        pair: i,
                        seq: d.seq,
                    });
                    matched.old.push(d);
                }
            }
            if let Some(file) = &pair.new {
                for d in matching::definitions(&file.path, &state.new) {
                    matched.new_at.push(Place {
                        pair: i,
                        seq: d.seq,
                    });
                    matched.new.push(d);
                }
            }
        }
        matched
    }

    /// The occurrence of definition `at` on one side.
    fn occurrence(&self, at: Place, new_side: bool) -> (&[Occurrence], &Occurrence) {
        let state = &self.states[at.pair];
        let list = if new_side { &state.new } else { &state.old };
        (list, &list[at.seq as usize])
    }

    /// The definitions that differ. A pair without both digests in one
    /// file is kept only when a changed line of its file touches its name.
    fn changes(
        &self,
        blobs: &mut Blobs<'_>,
        options: &DiffOptions,
    ) -> Result<Vec<matching::Change>, SymbolError> {
        let renames: BTreeMap<String, String> = self
            .pairs
            .iter()
            .filter(|p| p.kind == FileKind::Renamed)
            .filter_map(|p| Some((p.old.as_ref()?.path.clone(), p.path.clone())))
            .collect();
        let mut file_lines: HashMap<usize, (Vec<u32>, Vec<u32>)> = HashMap::new();
        let mut kept = Vec::new();
        for change in matching::match_definitions(&self.old, &self.new, &renames) {
            if let (true, Some(o), Some(n)) = (change.undigested, change.old, change.new) {
                let (oat, nat) = (self.old_at[o], self.new_at[n]);
                if let std::collections::hash_map::Entry::Vacant(slot) = file_lines.entry(nat.pair)
                {
                    let pair = &self.pairs[nat.pair];
                    let before = blobs.get_or_empty(counted_old(pair))?;
                    let after = blobs.get_or_empty(pair.new.as_ref())?;
                    let all = |b: &[u8]| 0..u32::try_from(b.len() + 1).unwrap_or(u32::MAX);
                    let diff =
                        jj_diff::range_diff(&before, all(&before), &after, all(&after), options);
                    slot.insert((diff.changed_old, diff.changed_new));
                }
                let (changed_old, changed_new) = &file_lines[&nat.pair];
                let old_line = self.occurrence(oat, false).1.range.start_line;
                let new_line = self.occurrence(nat, true).1.range.start_line;
                if !changed_old.contains(&old_line) && !changed_new.contains(&new_line) {
                    continue;
                }
            }
            kept.push(change);
        }
        Ok(kept)
    }

    /// `change` in full, with its home pair (where it is listed): line
    /// counts and nesting from its definitions' lines, and what its call
    /// sites are looked up by.
    fn detail(
        &self,
        change: &matching::Change,
        blobs: &mut Blobs<'_>,
        request: &SymbolRequest,
        unbounded: &DiffOptions,
    ) -> Result<Option<(usize, SymbolChange)>, SymbolError> {
        let o = change.old.map(|o| (&self.old[o], self.old_at[o]));
        let n = change.new.map(|n| (&self.new[n], self.new_at[n]));
        let ((main, at), new_side) = match (n, o) {
            (Some(n), _) => (n, true),
            (None, Some(o)) => (o, false),
            (None, None) => return Ok(None),
        };
        let home = at.pair;
        let pair = &self.pairs[home];
        let (list, here) = self.occurrence(at, new_side);
        let old_range = o.and_then(|(_, at)| self.occurrence(at, false).1.definition_range);
        let new_range = n.and_then(|(_, at)| self.occurrence(at, true).1.definition_range);
        let mut entry = SymbolChange {
            kind: change.kind,
            symbol: main.symbol.clone(),
            old_symbol: o
                .filter(|(od, _)| n.is_some_and(|(nd, _)| nd.symbol != od.symbol))
                .map(|(od, _)| od.symbol.clone()),
            name: main.name.clone(),
            symbol_kind: main.kind,
            enclosing_symbol: here
                .enclosing
                .and_then(|e| list.get(e as usize))
                .and_then(|e| e.symbol.clone()),
            old_path: o
                .map(|(od, _)| od.path.clone())
                .filter(|p| *p != pair.path && pair.old.as_ref().is_none_or(|f| f.path != *p)),
            old_range,
            new_range,
            lines_added: 0,
            lines_removed: 0,
            nested_only: false,
            call_status: CallStatus::NotLooked,
            call_count: 0,
            call_sites: Vec::new(),
            name_definitions: 0,
            lookup: None,
            old_pair: o.map(|(_, at)| at.pair),
        };
        match (change.kind, o, n, old_range, new_range) {
            (ChangeKind::Changed, Some((_, oat)), Some((_, nat)), Some(before), Some(after)) => {
                self.lines_and_nesting(&mut entry, (oat, before), (nat, after), blobs, unbounded)?;
            }
            (ChangeKind::Added, .., Some(after)) => entry.lines_added = line_count(&after),
            (ChangeKind::Removed, _, _, Some(before), _) => {
                entry.lines_removed = line_count(&before);
            }
            _ => {}
        }
        let looked_up = match change.kind {
            ChangeKind::Changed => !entry.nested_only,
            ChangeKind::Removed | ChangeKind::Moved => true,
            ChangeKind::Added => false,
        };
        if looked_up && request.call_sites > 0 {
            let state = &self.states[home];
            let scip = state.source == Some(Source::Scip);
            match kinds_for(entry.symbol_kind) {
                None if !scip => entry.call_status = CallStatus::NotTracked,
                kinds => {
                    entry.lookup = Some(Lookup {
                        symbol: scip.then(|| entry.symbol.clone()),
                        name: kinds.map(|(refs, defs)| (entry.name.clone(), refs, defs)),
                        family: family(&state.language).to_owned(),
                        exclude: n.zip(new_range).map(|((nd, _), r)| (nd.path.clone(), r)),
                    });
                }
            }
        }
        Ok(Some((home, entry)))
    }

    /// A changed pair's line counts, and whether it changed only inside
    /// its nested definitions, from the line diff of the two definitions.
    fn lines_and_nesting(
        &self,
        entry: &mut SymbolChange,
        (oat, before): (Place, Range),
        (nat, after): (Place, Range),
        blobs: &mut Blobs<'_>,
        unbounded: &DiffOptions,
    ) -> Result<(), SymbolError> {
        let old_bytes = blobs.get_or_empty(counted_old(&self.pairs[oat.pair]))?;
        let new_bytes = blobs.get_or_empty(self.pairs[nat.pair].new.as_ref())?;
        let diff = jj_diff::range_diff(
            &old_bytes,
            lines_of(&before),
            &new_bytes,
            lines_of(&after),
            unbounded,
        );
        entry.lines_added = diff.added;
        entry.lines_removed = diff.removed;
        let old_lines: Vec<&[u8]> = old_bytes.split(|b| *b == b'\n').collect();
        let new_lines: Vec<&[u8]> = new_bytes.split(|b| *b == b'\n').collect();
        let has_lines = diff.changed_old.iter().any(|&l| !blank(&old_lines, l))
            || diff.changed_new.iter().any(|&l| !blank(&new_lines, l));
        entry.nested_only = has_lines
            && inside_children(
                &self.states[oat.pair].old,
                oat.seq,
                &old_lines,
                &diff.changed_old,
            )
            && inside_children(
                &self.states[nat.pair].new,
                nat.seq,
                &new_lines,
                &diff.changed_new,
            );
        Ok(())
    }
}

/// A page whose files are all `coverage`, with `summary`.
fn uniform(page: &[String], coverage: Coverage, summary: Summary) -> SymbolDiff {
    let file = FileSymbols {
        coverage,
        skip_reason: None,
        source: None,
        parse_errors: false,
        changes: Vec::new(),
        truncated: false,
        hunks: None,
    };
    SymbolDiff {
        files: page.iter().map(|p| (p.clone(), file.clone())).collect(),
        summary,
    }
}

/// A page whose symbol diff failed: every file skipped, for no reason.
pub(crate) fn skipped(page: &[String]) -> SymbolDiff {
    let summary = Summary {
        files_skipped: u32::try_from(page.len()).unwrap_or(u32::MAX),
        ..Summary::default()
    };
    uniform(page, Coverage::Skipped, summary)
}

/// Computes the symbol diff of the differing files `pairs` (`None`: more
/// than [`RENAME_LIMIT`] differ) for the page's files `page`, reading blobs
/// through `tree`. Call sites are prepared, not looked up: see
/// [`call_sites`].
pub(crate) fn compute(
    index: &SymbolIndex,
    store: &Store,
    tree: &Tree,
    pairs: Option<&[DiffPair]>,
    page: &[String],
    request: &SymbolRequest,
    options: &DiffOptions,
) -> Result<SymbolDiff, SymbolError> {
    let Some(pairs) = pairs.filter(|p| p.len() <= RENAME_LIMIT) else {
        let summary = Summary {
            diff_too_large: true,
            ..Summary::default()
        };
        return Ok(uniform(page, Coverage::DiffTooLarge, summary));
    };
    let found = entries(index, store, tree, pairs, request.source)?;
    let mut states = Vec::with_capacity(pairs.len());
    let mut summary = Summary::default();
    for pair in pairs {
        let state = pair_state(store, pair, request.source, &found)?;
        match state.coverage {
            Coverage::Covered => summary.files_covered += 1,
            Coverage::Unsupported => summary.files_unsupported += 1,
            Coverage::Skipped | Coverage::NoScip => summary.files_skipped += 1,
            Coverage::DiffTooLarge => {}
        }
        states.push(state);
    }
    let matched = Matched::new(pairs, states);
    let mut blobs = Blobs {
        tree,
        max: index.limits().max_file_size,
        read: HashMap::new(),
    };
    let changes = matched.changes(&mut blobs, options)?;
    for change in &changes {
        match change.kind {
            ChangeKind::Added => summary.added += 1,
            ChangeKind::Removed => summary.removed += 1,
            ChangeKind::Changed => summary.changed += 1,
            ChangeKind::Moved => summary.moved += 1,
        }
    }

    // The page's files, in detail.
    let on_page: HashMap<&str, usize> = pairs
        .iter()
        .enumerate()
        .filter(|(_, p)| page.contains(&p.path))
        .map(|(i, p)| (p.path.as_str(), i))
        .collect();
    let mut files: HashMap<String, FileSymbols> = on_page
        .iter()
        .map(|(path, &i)| {
            let state = &matched.states[i];
            let file = FileSymbols {
                coverage: state.coverage,
                skip_reason: state.skip_reason,
                source: state.source.filter(|_| state.coverage == Coverage::Covered),
                parse_errors: state.parse_errors,
                changes: Vec::new(),
                truncated: false,
                hunks: None,
            };
            ((*path).to_owned(), file)
        })
        .collect();
    let unbounded = DiffOptions {
        max_lines: usize::MAX,
        max_hunk_bytes: usize::MAX,
        ..options.clone()
    };
    for change in &changes {
        let home = match (change.new, change.old) {
            (Some(n), _) => matched.new_at[n].pair,
            (None, Some(o)) => matched.old_at[o].pair,
            (None, None) => continue,
        };
        let Some(file) = files.get_mut(&pairs[home].path) else {
            continue;
        };
        if file.changes.len() == MAX_CHANGES_PER_FILE {
            file.truncated = true;
            continue;
        }
        if let Some((_, entry)) = matched.detail(change, &mut blobs, request, &unbounded)? {
            file.changes.push(entry);
        }
    }
    if !request.expand.is_empty() {
        expand(pairs, &on_page, &mut files, &mut blobs, request, options)?;
        // A file with nothing expanded leaves the response: no call sites.
        for file in files.values_mut().filter(|f| f.hunks.is_none()) {
            for change in &mut file.changes {
                change.lookup = None;
            }
        }
    }
    Ok(SymbolDiff { files, summary })
}

/// Fills each page file's `hunks` with its expanded symbols' lines.
fn expand(
    pairs: &[DiffPair],
    on_page: &HashMap<&str, usize>,
    files: &mut HashMap<String, FileSymbols>,
    blobs: &mut Blobs<'_>,
    request: &SymbolRequest,
    options: &DiffOptions,
) -> Result<(), SymbolError> {
    for (path, &i) in on_page {
        let Some(file) = files.get_mut(*path) else {
            continue;
        };
        let expanded: Vec<&SymbolChange> = file
            .changes
            .iter()
            .filter(|c| {
                request.expand.iter().any(|e| {
                    matching::matches_expand(e, &c.symbol, c.old_symbol.as_deref(), &c.name)
                })
            })
            .collect();
        if expanded.is_empty() {
            continue;
        }
        // An expanded change inside another expanded one is shown with it:
        // by new ranges, or by old ranges in the same old file.
        let inner = |c: &SymbolChange| {
            expanded.iter().any(|other| {
                let contains = |a: Range, b: Range| a != b && a.contains(&b);
                !std::ptr::eq(*other, c)
                    && match (other.new_range, c.new_range) {
                        (Some(a), Some(b)) => contains(a, b),
                        _ => match (other.old_range, c.old_range) {
                            (Some(a), Some(b)) if other.old_pair == c.old_pair => contains(a, b),
                            _ => false,
                        },
                    }
            })
        };
        let pair = &pairs[i];
        let mut out = Expanded::default();
        // The whole file's changed lines, for where an added or removed
        // symbol's absent side sits.
        let mut whole: Option<Vec<HunkInfo>> = None;
        let mut found = Vec::new();
        for change in expanded.iter().copied().filter(|c| !inner(c)) {
            if change.kind == ChangeKind::Moved {
                continue;
            }
            let old_file = change
                .old_pair
                .filter(|_| change.old_range.is_some())
                .and_then(|i| counted_old(&pairs[i]));
            let old_bytes = blobs.get(old_file)?;
            let new_bytes = blobs.get(pair.new.as_ref().filter(|_| change.new_range.is_some()))?;
            let old_lines = change
                .old_range
                .filter(|_| old_bytes.is_some())
                .map(|r| lines_of(&r));
            let new_lines = change.new_range.map(|r| lines_of(&r));
            if old_lines.is_none() && new_lines.is_none() {
                continue;
            }
            let diff = jj_diff::range_diff(
                old_bytes.as_deref().unwrap_or_default(),
                old_lines.clone().unwrap_or(0..0),
                new_bytes.as_deref().unwrap_or_default(),
                new_lines.clone().unwrap_or(0..0),
                options,
            );
            out.truncated |= diff.truncated;
            out.old_no_final_newline |= diff.old_no_final_newline;
            out.new_no_final_newline |= diff.new_no_final_newline;
            let mut hunks = diff.hunks;
            if old_lines.is_none() || new_lines.is_none() {
                if whole.is_none() {
                    let before = blobs.get_or_empty(counted_old(pair))?;
                    let after = blobs.get_or_empty(pair.new.as_ref())?;
                    let all = |b: &[u8]| 0..u32::try_from(b.len() + 1).unwrap_or(u32::MAX);
                    let zero = DiffOptions {
                        context: 0,
                        max_lines: usize::MAX,
                        max_hunk_bytes: usize::MAX,
                        ..options.clone()
                    };
                    whole = Some(
                        jj_diff::range_diff(&before, all(&before), &after, all(&after), &zero)
                            .hunks,
                    );
                }
                let file_hunks = whole.as_deref().unwrap_or_default();
                for hunk in &mut hunks {
                    anchor(hunk, file_hunks, new_lines.is_some());
                }
            }
            found.extend(hunks);
        }
        found.sort_by_key(|h| (h.new_start, h.old_start));
        let (mut lines_left, mut bytes_left) = (options.max_lines, options.max_hunk_bytes);
        for hunk in found {
            let count = hunk.lines.lines().count();
            if count > lines_left || hunk.lines.len() > bytes_left {
                out.truncated = true;
                continue;
            }
            lines_left -= count;
            bytes_left -= hunk.lines.len();
            out.hunks.push(hunk);
        }
        file.hunks = Some(out);
    }
    Ok(())
}

/// Places the absent side of an added (`new_side`) or removed symbol's
/// hunk where the file's own diff has it: beside the file hunk that holds
/// the symbol's lines, as git writes an empty side (the line before).
fn anchor(hunk: &mut HunkInfo, file_hunks: &[HunkInfo], new_side: bool) {
    let side = |h: &HunkInfo| {
        if new_side {
            (h.new_start, h.new_lines, h.old_start, h.old_lines)
        } else {
            (h.old_start, h.old_lines, h.new_start, h.new_lines)
        }
    };
    let (at, ..) = side(hunk);
    let Some((_, _, other_start, other_lines)) = file_hunks
        .iter()
        .map(side)
        .find(|&(start, lines, ..)| start <= at && at < start + lines.max(1))
    else {
        return;
    };
    let place = if other_lines == 0 {
        other_start
    } else {
        other_start + other_lines - 1
    };
    if new_side {
        hunk.old_start = place;
    } else {
        hunk.new_start = place;
    }
}

/// `rows`, one per key, by key.
fn keyed(keys: &BTreeSet<String>, rows: Vec<Vec<Found>>) -> HashMap<&str, Vec<Found>> {
    keys.iter().map(String::as_str).zip(rows).collect()
}

/// The stored rows a page's lookups read, by key.
struct Rows<'a> {
    references: HashMap<&'a str, Vec<Found>>,
    definitions: HashMap<&'a str, Vec<Found>>,
    by_symbol: HashMap<&'a str, Vec<Found>>,
}

/// Looks up the call sites of every change of `diff` that waits for them,
/// in `tree`, which is indexed: up to [`MAX_LOOKUPS_PER_PAGE`] changes,
/// with the rest [`CallStatus::PageLimit`].
pub(crate) fn call_sites(
    index: &SymbolIndex,
    store: &Store,
    tree: &Tree,
    diff: &mut SymbolDiff,
    request: &SymbolRequest,
) -> Result<(), SymbolError> {
    let mut paths: Vec<String> = diff.files.keys().cloned().collect();
    paths.sort_by(|a, b| path_order(a, b));
    let mut waiting = 0;
    let mut names = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    for path in &paths {
        let Some(file) = diff.files.get_mut(path) else {
            continue;
        };
        for change in &mut file.changes {
            let Some(lookup) = &change.lookup else {
                continue;
            };
            if waiting == MAX_LOOKUPS_PER_PAGE {
                change.lookup = None;
                change.call_status = CallStatus::PageLimit;
                continue;
            }
            waiting += 1;
            if let Some((name, ..)) = &lookup.name {
                names.insert(name.clone());
            }
            if let Some(symbol) = &lookup.symbol {
                symbols.insert(symbol.clone());
            }
        }
    }
    if waiting == 0 {
        return Ok(());
    }
    index.count_lookup();
    let name_keys: Vec<RefKey<'_>> = names.iter().map(|n| RefKey::Name(n)).collect();
    let symbol_keys: Vec<RefKey<'_>> = symbols.iter().map(|s| RefKey::Symbol(s)).collect();
    let limit = Some(MAX_CANDIDATES + 1);
    let rows = Rows {
        references: keyed(
            &names,
            store.find_rows(&name_keys, Some(Role::Reference), limit)?,
        ),
        definitions: keyed(
            &names,
            store.find_rows(&name_keys, Some(Role::Definition), limit)?,
        ),
        by_symbol: keyed(
            &symbols,
            store.find_rows(&symbol_keys, Some(Role::Reference), limit)?,
        ),
    };
    let mut current = Current::new(store, tree);
    for path in &paths {
        let Some(file) = diff.files.get_mut(path) else {
            continue;
        };
        for change in &mut file.changes {
            if let Some(lookup) = change.lookup.take() {
                look_up(change, &lookup, &rows, &mut current, request.call_sites)?;
            }
        }
    }
    Ok(())
}

/// One change's call sites from `rows`.
fn look_up(
    change: &mut SymbolChange,
    lookup: &Lookup,
    rows: &Rows<'_>,
    current: &mut Current<'_>,
    listed: usize,
) -> Result<(), SymbolError> {
    let empty = Vec::new();
    let mut capped = false;
    let mut sites: Vec<Listed> = Vec::new();
    let excluded = |hit: &Found| {
        lookup.exclude.as_ref().is_some_and(|(path, range)| {
            hit.entry.path == *path && range.contains(&hit.occurrence.range)
        })
    };
    let site = |hit: &Found| Listed {
        path: hit.entry.path.clone(),
        seq: hit.seq,
        occurrence: hit.occurrence.clone(),
        enclosing_symbol: hit.enclosing_symbol.clone(),
        source: hit.entry.source,
        language: hit.entry.language.clone(),
    };
    if let Some(symbol) = &lookup.symbol {
        let found = rows.by_symbol.get(symbol.as_str()).unwrap_or(&empty);
        capped |= found.len() > MAX_CANDIDATES;
        for hit in found.iter().take(MAX_CANDIDATES) {
            if hit.entry.source == Source::Scip && !excluded(hit) && current.keeps(hit, None)? {
                sites.push(site(hit));
            }
        }
    }
    if let Some((name, refs, defs)) = &lookup.name {
        let in_family = |hit: &Found, kinds: &[Kind]| {
            kinds.contains(&hit.occurrence.kind) && family(&hit.entry.language) == lookup.family
        };
        // A tags reference's kind is the use's (a call is a function); a
        // SCIP reference's is the referenced symbol's (a method, a struct).
        let reference = |hit: &Found| {
            // A SCIP change's SCIP files answer by symbol, above.
            match hit.entry.source {
                Source::Tags => in_family(hit, refs),
                Source::Scip => lookup.symbol.is_none() && in_family(hit, defs),
            }
        };
        let found = rows.references.get(name.as_str()).unwrap_or(&empty);
        capped |= found.len() > MAX_CANDIDATES;
        let mut used = lookup.symbol.is_none();
        for hit in found.iter().take(MAX_CANDIDATES) {
            if reference(hit) && !excluded(hit) && current.keeps(hit, None)? {
                used = true;
                sites.push(site(hit));
            }
        }
        if used {
            // Definitions of the name from every source, the changed one
            // included.
            let mut count = 0;
            for hit in rows.definitions.get(name.as_str()).unwrap_or(&empty) {
                if in_family(hit, defs) && current.keeps(hit, None)? {
                    count += 1;
                }
            }
            change.name_definitions = count;
        }
    }
    sites.sort_by(|a, b| {
        path_order(&a.path, &b.path).then(a.occurrence.range.cmp(&b.occurrence.range))
    });
    sites.dedup_by(|a, b| a.path == b.path && a.seq == b.seq);
    change.call_count = u32::try_from(sites.len()).unwrap_or(u32::MAX);
    change.call_status = if capped {
        CallStatus::Capped
    } else {
        CallStatus::Found
    };
    sites.truncate(listed);
    change.call_sites = sites;
    Ok(())
}
