//! Definitions matched across the two sides of a diff (R2.15, ADR 0043):
//! which were added, removed, changed or moved.
//!
//! Matching is over the whole diff, by symbol string, so a Go method moved
//! between files of one package, or a SCIP symbol whose identity is the
//! package's, pairs across files. A definition on the old side of a renamed
//! file has its path's components rewritten to the new path's first
//! ([`rewrite_for_rename`]). Within one symbol, old and new definitions pair
//! by equal digest first, then in source order. What is left pairs by
//! `(name, kind, digest)` when exactly one of each remains: a move under an
//! identity that spells the file.
//!
//! No line diffing here: that needs jj-lib's diff, which this crate does not
//! link. A pair whose digests are not both known, in one file, is returned
//! [`Change::undigested`] for the caller to decide from the lines.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::symbol::{self, Descriptor, Suffix, Symbol, TAGS_SCHEME};
use crate::{Kind, Language};

/// One definition on one side of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The file on its side.
    pub path: String,
    /// Its symbol string.
    pub symbol: String,
    /// Its display name.
    pub name: String,
    /// What it is.
    pub kind: Kind,
    /// The digest of its definition range; `None` without one.
    pub digest: Option<u64>,
    /// Its position among its file's occurrences: source order.
    pub seq: u32,
}

/// How a definition differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangeKind {
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Removed,
    /// On both sides, with other bytes.
    Changed,
    /// On both sides with the same bytes, in another file, or under another
    /// symbol with the same name, kind and bytes.
    Moved,
}

/// One definition that differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// How.
    pub kind: ChangeKind,
    /// Its index in the old definitions; `None` when added.
    pub old: Option<usize>,
    /// Its index in the new definitions; `None` when removed.
    pub new: Option<usize>,
    /// A pair in one file whose digests are not both known: `kind` is
    /// [`ChangeKind::Changed`], and the caller keeps it only when a changed
    /// line touches it.
    pub undigested: bool,
}

/// A path's components that a tags symbol spells: all of them, or for Go
/// its directory's.
fn tags_prefix(path: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if Language::from_path(path) == Some(Language::Go) {
        parts.pop();
    }
    parts
}

fn namespaces(parts: &[&str]) -> Vec<Descriptor> {
    parts
        .iter()
        .map(|&p| Descriptor::new(p, Suffix::Namespace))
        .collect()
}

fn is_run(descriptors: &[Descriptor], parts: &[&str]) -> bool {
    descriptors.len() == parts.len()
        && descriptors
            .iter()
            .zip(parts)
            .all(|(d, p)| d.suffix == Suffix::Namespace && d.name == *p)
}

/// `symbol` as the old side of a file renamed from `old_path` to `new_path`
/// would have spelled it at the new path, or `None` when it does not spell
/// the old path.
///
/// A tags symbol's leading namespaces are its file's path components (its
/// directory's for Go). A SCIP symbol is rewritten when its descriptors hold
/// a run of namespaces equal to the old path with some leading components
/// `p` removed (an indexer run in a subdirectory), and the new path also
/// starts with `p`; package-scoped symbols spell no path and stay.
pub fn rewrite_for_rename(symbol: &str, old_path: &str, new_path: &str) -> Option<String> {
    let Ok(Symbol::Global {
        scheme,
        package,
        mut descriptors,
    }) = symbol::parse(symbol)
    else {
        return None;
    };
    if scheme == TAGS_SCHEME {
        let (old, new) = (tags_prefix(old_path), tags_prefix(new_path));
        if old == new || descriptors.len() < old.len() || !is_run(&descriptors[..old.len()], &old) {
            return None;
        }
        descriptors.splice(..old.len(), namespaces(&new));
    } else {
        let old: Vec<&str> = old_path.split('/').collect();
        let new: Vec<&str> = new_path.split('/').collect();
        let (start, k) = (0..old.len())
            .filter(|&k| k <= new.len() && old[..k] == new[..k])
            .find_map(|k| {
                let run = old.len() - k;
                // `windows` yields nothing when the symbol is shorter than
                // the run: a module symbol in a deep file spells no path.
                descriptors
                    .windows(run)
                    .position(|window| is_run(window, &old[k..]))
                    .map(|i| (i, k))
            })?;
        descriptors.splice(start..start + old.len() - k, namespaces(&new[k..]));
    }
    let package = [
        package[0].as_str(),
        package[1].as_str(),
        package[2].as_str(),
    ];
    Some(symbol::format(&scheme, package, &descriptors))
}

/// A symbol string in one spelling, so two indexers' escaping cannot split
/// one symbol in two.
fn canonical(symbol: &str) -> String {
    match symbol::parse(symbol) {
        Ok(Symbol::Global {
            scheme,
            package,
            descriptors,
        }) => symbol::format(
            &scheme,
            [&package[0], &package[1], &package[2]],
            &descriptors,
        ),
        _ => symbol.to_owned(),
    }
}

/// Matches `old`'s definitions with `new`'s. `renames` maps each renamed
/// file's old path to its new one (copies are not renames: a copy's
/// definitions are new). Returns only the definitions that differ: those
/// with a new side in new order, then removed ones in old order.
pub fn match_definitions(
    old: &[Definition],
    new: &[Definition],
    renames: &BTreeMap<String, String>,
) -> Vec<Change> {
    let mut keys: BTreeMap<String, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (i, d) in old.iter().enumerate() {
        let key = renames
            .get(&d.path)
            .and_then(|to| rewrite_for_rename(&d.symbol, &d.path, to))
            .unwrap_or_else(|| d.symbol.clone());
        keys.entry(canonical(&key)).or_default().0.push(i);
    }
    for (i, d) in new.iter().enumerate() {
        keys.entry(canonical(&d.symbol)).or_default().1.push(i);
    }
    let mut changes = Vec::new();
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    let same_file = |od: &Definition, nd: &Definition| {
        od.path == nd.path || renames.get(&od.path) == Some(&nd.path)
    };
    for (olds, news) in keys.into_values() {
        let sides = Sides {
            old,
            new,
            same_file: &same_file,
        };
        for (o, n) in pair_within_key(&sides, olds, news, &mut removed, &mut added) {
            let (od, nd) = (&old[o], &new[n]);
            let same_file = same_file(od, nd);
            let (kind, undigested) = match (od.digest, nd.digest) {
                (Some(a), Some(b)) if a == b && same_file => continue,
                (Some(a), Some(b)) if a == b => (ChangeKind::Moved, false),
                (Some(_), Some(_)) => (ChangeKind::Changed, false),
                _ if same_file => (ChangeKind::Changed, true),
                _ => (ChangeKind::Moved, false),
            };
            changes.push(Change {
                kind,
                old: Some(o),
                new: Some(n),
                undigested,
            });
        }
    }
    changes.extend(moves_by_bytes(old, new, &mut removed, &mut added));
    changes.extend(removed.into_iter().map(|o| Change {
        kind: ChangeKind::Removed,
        old: Some(o),
        new: None,
        undigested: false,
    }));
    changes.extend(added.into_iter().map(|n| Change {
        kind: ChangeKind::Added,
        old: None,
        new: Some(n),
        undigested: false,
    }));
    let key = |c: &Change| match (c.new, c.old) {
        (Some(n), _) => (0, new[n].path.clone(), new[n].seq),
        (None, Some(o)) => (1, old[o].path.clone(), old[o].seq),
        (None, None) => (2, String::new(), 0),
    };
    changes.sort_by_key(key);
    changes
}

/// Both sides' definitions, and whether two are in one file (its own, or
/// renamed).
struct Sides<'a> {
    old: &'a [Definition],
    new: &'a [Definition],
    same_file: &'a dyn Fn(&Definition, &Definition) -> bool,
}

/// Pairs one symbol's old and new definitions: equal digests in one file
/// first, then equal digests anywhere, then the rest, each in source order.
/// Unpaired ones go to `removed` and `added`.
fn pair_within_key(
    sides: &Sides<'_>,
    mut olds: Vec<usize>,
    mut news: Vec<usize>,
    removed: &mut Vec<usize>,
    added: &mut Vec<usize>,
) -> Vec<(usize, usize)> {
    let (old, new) = (sides.old, sides.new);
    olds.sort_by(|a, b| (&old[*a].path, old[*a].seq).cmp(&(&old[*b].path, old[*b].seq)));
    news.sort_by(|a, b| (&new[*a].path, new[*a].seq).cmp(&(&new[*b].path, new[*b].seq)));
    let mut taken = vec![false; news.len()];
    let mut pairs = Vec::new();
    let mut paired = vec![false; olds.len()];
    for in_file in [true, false] {
        for (i, &o) in olds.iter().enumerate() {
            if paired[i] {
                continue;
            }
            let found = news.iter().enumerate().position(|(j, &n)| {
                !taken[j]
                    && old[o].digest.is_some()
                    && old[o].digest == new[n].digest
                    && (!in_file || (sides.same_file)(&old[o], &new[n]))
            });
            if let Some(j) = found {
                taken[j] = true;
                paired[i] = true;
                pairs.push((o, news[j]));
            }
        }
    }
    let left: Vec<usize> = olds
        .iter()
        .zip(&paired)
        .filter(|&(_, &p)| !p)
        .map(|(&o, _)| o)
        .collect();
    let mut rest = news
        .iter()
        .zip(&taken)
        .filter(|&(_, &t)| !t)
        .map(|(&n, _)| n);
    for o in left {
        match rest.next() {
            Some(n) => pairs.push((o, n)),
            None => removed.push(o),
        }
    }
    added.extend(rest);
    pairs
}

/// One group's old and new definitions.
type Group = (Vec<usize>, Vec<usize>);

/// Moves under identities that spell the file: one removed and one added
/// definition with the same name, kind and bytes. Takes them out of
/// `removed` and `added`.
fn moves_by_bytes(
    old: &[Definition],
    new: &[Definition],
    removed: &mut Vec<usize>,
    added: &mut Vec<usize>,
) -> Vec<Change> {
    let mut groups: HashMap<(&str, Kind, u64), Group> = HashMap::new();
    for &o in removed.iter() {
        if let Some(digest) = old[o].digest {
            let key = (old[o].name.as_str(), old[o].kind, digest);
            groups.entry(key).or_default().0.push(o);
        }
    }
    for &n in added.iter() {
        if let Some(digest) = new[n].digest {
            let key = (new[n].name.as_str(), new[n].kind, digest);
            groups.entry(key).or_default().1.push(n);
        }
    }
    let mut moves = Vec::new();
    for (olds, news) in groups.into_values() {
        if let ([o], [n]) = (olds.as_slice(), news.as_slice()) {
            moves.push(Change {
                kind: ChangeKind::Moved,
                old: Some(*o),
                new: Some(*n),
                undigested: false,
            });
        }
    }
    let moved_old: HashSet<usize> = moves.iter().filter_map(|m| m.old).collect();
    let moved_new: HashSet<usize> = moves.iter().filter_map(|m| m.new).collect();
    removed.retain(|o| !moved_old.contains(o));
    added.retain(|n| !moved_new.contains(n));
    moves
}

/// Bytes after which a suffix of a symbol string may begin.
const BOUNDARIES: &[u8] = b" /#.`";

/// Whether an `expand_symbols` entry names a change: it equals the change's
/// symbol, old symbol or name, or is a suffix of its symbol beginning after
/// a descriptor boundary, with or without the final `.`.
pub fn matches_expand(entry: &str, symbol: &str, old_symbol: Option<&str>, name: &str) -> bool {
    if entry.is_empty() {
        return false;
    }
    if entry == symbol || entry == name || old_symbol == Some(entry) {
        return true;
    }
    let dotted = format!("{entry}.");
    [entry, dotted.as_str()].into_iter().any(|suffix| {
        symbol.len() > suffix.len()
            && symbol.ends_with(suffix)
            && BOUNDARIES.contains(&symbol.as_bytes()[symbol.len() - suffix.len() - 1])
    })
}

/// The definitions among `occurrences` of the file at `path`, for
/// [`match_definitions`].
pub fn definitions(path: &str, occurrences: &[crate::Occurrence]) -> Vec<Definition> {
    occurrences
        .iter()
        .enumerate()
        .filter(|(_, o)| o.role == crate::Role::Definition)
        .filter_map(|(seq, o)| {
            Some(Definition {
                path: path.to_owned(),
                symbol: o.symbol.clone()?,
                name: o.name.clone(),
                kind: o.kind,
                digest: o.digest,
                seq: u32::try_from(seq).unwrap_or(u32::MAX),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_symbols_rewrite_by_path() {
        assert_eq!(
            rewrite_for_rename("reef-tags . . . `a.py`/f().", "a.py", "b/a.py").as_deref(),
            Some("reef-tags . . . b/`a.py`/f().")
        );
        assert_eq!(
            rewrite_for_rename("reef-tags . . . x/T#M().", "x/a.go", "y/z/a.go").as_deref(),
            Some("reef-tags . . . y/z/T#M().")
        );
        // A Go file renamed within its directory spells the same package.
        assert_eq!(
            rewrite_for_rename("reef-tags . . . x/T#M().", "x/a.go", "x/b.go"),
            None
        );
        assert_eq!(
            rewrite_for_rename("reef-tags . . . `other.py`/f().", "a.py", "b.py"),
            None
        );
    }

    #[test]
    fn expand_entries_match_at_boundaries() {
        let symbol = "reef-tags . . . src/`parser.py`/Parser#parse().";
        assert!(matches_expand(symbol, symbol, None, "parse"));
        assert!(matches_expand("Parser#parse().", symbol, None, "parse"));
        assert!(matches_expand("Parser#parse()", symbol, None, "parse"));
        assert!(matches_expand("parse", symbol, None, "parse"));
        assert!(!matches_expand("arse#parse()", symbol, None, "parse"));
        assert!(!matches_expand("", symbol, None, "parse"));
    }
}
