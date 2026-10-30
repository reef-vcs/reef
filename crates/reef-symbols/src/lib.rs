//! Symbols for Reef's symbol index (R2.14, ADR 0042): definitions and
//! references with their ranges, extracted from a file's bytes.
//!
//! Two sources produce the same [`FileSymbols`]:
//!
//! - [`extract`] runs Reef's own tree-sitter tag queries (`queries/*.scm`)
//!   for Python, TypeScript, TSX, JavaScript, Go and Rust. A definition gets
//!   a symbol string in SCIP's grammar under the `reef-tags` scheme; a
//!   reference carries only its name.
//! - [`scip::normalize`] reads one encoded `scip.Document` an indexer wrote
//!   for the same bytes, converting its positions to UTF-8 byte columns.
//!
//! Positions are 0-based lines and UTF-8 byte columns, ends exclusive. Every
//! occurrence names its enclosing definition (the innermost one whose
//! definition range contains it), and every definition with a range carries
//! a digest of its bytes, so a caller can tell a changed body from an
//! unchanged one without reading either side.
//!
//! The crate has no Reef, jj-lib, gRPC, SQLite or tokio dependency: bytes
//! in, symbols out. The daemon indexes with it today; the phase-four store
//! and phase five's structural merge reuse it.

use std::time::Duration;

pub mod diff;
pub mod scip;
pub mod symbol;
mod tags;

pub use tags::extract;

/// The version of Reef's tag queries and of how [`extract`] reads them.
/// Bump it whenever a query, a grammar crate, or the extraction changes:
/// entries made by another version are never read, and GC sweeps them.
pub const TAGS_VERSION: u32 = 1;

/// A language [`extract`] supports, by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// `.py`, `.pyi`.
    Python,
    /// `.ts`, `.mts`, `.cts`, `.d.ts`.
    TypeScript,
    /// `.tsx`.
    Tsx,
    /// `.js`, `.mjs`, `.cjs`, `.jsx`, parsed with the TSX grammar, which
    /// accepts JavaScript and JSX.
    JavaScript,
    /// `.go`.
    Go,
    /// `.rs`.
    Rust,
}

impl Language {
    /// Every supported language.
    pub const ALL: [Self; 6] = [
        Self::Python,
        Self::TypeScript,
        Self::Tsx,
        Self::JavaScript,
        Self::Go,
        Self::Rust,
    ];

    /// The language of a repository path, by its extension, or `None` when
    /// it is not one [`extract`] reads.
    pub fn from_path(path: &str) -> Option<Self> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let (_, extension) = name.rsplit_once('.')?;
        Some(match extension {
            "py" | "pyi" => Self::Python,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "go" => Self::Go,
            "rs" => Self::Rust,
            _ => return None,
        })
    }

    /// The language for its [`name`](Self::name).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.name() == name)
    }

    /// The language's name in the API and the index: `python`,
    /// `typescript`, `tsx`, `javascript`, `go` or `rust`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Go => "go",
            Self::Rust => "rust",
        }
    }

    /// The version of the grammar crate that parses it. Checked against
    /// `Cargo.lock` by a test, so a grammar update cannot leave it stale.
    pub const fn grammar_version(self) -> &'static str {
        match self {
            Self::Python | Self::Go => "0.25.0",
            Self::TypeScript | Self::Tsx | Self::JavaScript => "0.23.2",
            Self::Rust => "0.24.2",
        }
    }
}

/// The extractor string of [`extract`]'s entries for `language`, such as
/// `tags/1/rust/0.24.2`: [`TAGS_VERSION`], the language, and its grammar
/// crate's version. An index looks up only the current one.
pub fn extractor(language: Language) -> String {
    format!(
        "tags/{TAGS_VERSION}/{}/{}",
        language.name(),
        language.grammar_version()
    )
}

/// What a symbol is. The values are the API's `SymbolKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Kind {
    /// Not known.
    Unspecified = 0,
    /// A module, namespace or package.
    Module = 1,
    /// A class.
    Class = 2,
    /// A struct or union.
    Struct = 3,
    /// An interface, trait or protocol.
    Interface = 4,
    /// An enum.
    Enum = 5,
    /// A type alias or other named type.
    Type = 6,
    /// A free function.
    Function = 7,
    /// A function that belongs to a type.
    Method = 8,
    /// A field or property.
    Field = 9,
    /// A variable.
    Variable = 10,
    /// A constant or static.
    Constant = 11,
    /// A macro.
    Macro = 12,
}

impl Kind {
    /// The kind for an API value, or `None` for an unknown one.
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Unspecified,
            1 => Self::Module,
            2 => Self::Class,
            3 => Self::Struct,
            4 => Self::Interface,
            5 => Self::Enum,
            6 => Self::Type,
            7 => Self::Function,
            8 => Self::Method,
            9 => Self::Field,
            10 => Self::Variable,
            11 => Self::Constant,
            12 => Self::Macro,
            _ => return None,
        })
    }
}

/// Whether an occurrence defines its symbol or refers to it. The values are
/// the API's `SymbolRole`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Role {
    /// The symbol is defined here.
    Definition = 1,
    /// The symbol is used here.
    Reference = 2,
}

impl Role {
    /// The role for an API value, or `None` for an unknown one.
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Definition),
            2 => Some(Self::Reference),
            _ => None,
        }
    }
}

/// Bits of [`Occurrence::flags`], from SCIP's roles. Tags set none.
pub mod flags {
    /// The symbol is imported here.
    pub const IMPORT: u8 = 1;
    /// The symbol is written here.
    pub const WRITE: u8 = 2;
    /// The occurrence is in test code.
    pub const TEST: u8 = 4;
    /// The occurrence is in generated code.
    pub const GENERATED: u8 = 8;
}

/// A range in a file: 0-based lines and UTF-8 byte columns, end exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Range {
    /// The first line.
    pub start_line: u32,
    /// The byte column on the first line.
    pub start_col: u32,
    /// The last line.
    pub end_line: u32,
    /// The byte column on the last line, exclusive.
    pub end_col: u32,
}

impl Range {
    /// Whether `other` lies within this range, ends included.
    pub fn contains(&self, other: &Self) -> bool {
        (self.start_line, self.start_col) <= (other.start_line, other.start_col)
            && (other.end_line, other.end_col) <= (self.end_line, self.end_col)
    }
}

/// One definition of, or reference to, a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// The symbol string in SCIP's grammar. `None` for a tags reference,
    /// which knows only its name.
    pub symbol: Option<String>,
    /// The display name.
    pub name: String,
    /// What the symbol is.
    pub kind: Kind,
    /// Definition or reference.
    pub role: Role,
    /// [`flags`] bits.
    pub flags: u8,
    /// The name's range.
    pub range: Range,
    /// A definition's whole range: its node, from any decorator to its end.
    /// `None` for references, and for SCIP definitions with none.
    pub definition_range: Option<Range>,
    /// The index in [`FileSymbols::occurrences`] of the innermost definition
    /// whose definition range contains this occurrence, other than itself.
    pub enclosing: Option<u32>,
    /// The first 8 bytes of blake3 over the definition range's bytes, read
    /// as little-endian. Definitions with a range only.
    pub digest: Option<u64>,
}

/// The symbols of one file, in source order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSymbols {
    /// Occurrences sorted by their range.
    pub occurrences: Vec<Occurrence>,
    /// The parse recovered from syntax errors. Most of the file's tags
    /// survive, so they are kept.
    pub has_errors: bool,
}

/// Why a file was not extracted. The values are the API's
/// `SymbolSkipReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[repr(u8)]
pub enum Skip {
    /// Larger than [`Limits::max_file_size`].
    #[error("the file is larger than the size limit")]
    TooLarge = 1,
    /// A NUL byte in its first 8 KiB.
    #[error("the file is binary")]
    Binary = 2,
    /// Parsing and querying it took longer than [`Limits::parse_timeout`].
    #[error("parsing the file took longer than the limit")]
    ParseTimeout = 3,
    /// More than [`Limits::max_occurrences`] occurrences.
    #[error("the file has more occurrences than the limit")]
    TooManyOccurrences = 4,
}

impl Skip {
    /// The skip for an API value, or `None` for another (including the
    /// index's own `CONFLICTED`, which no extraction returns).
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::TooLarge),
            2 => Some(Self::Binary),
            3 => Some(Self::ParseTimeout),
            4 => Some(Self::TooManyOccurrences),
            _ => None,
        }
    }
}

/// Bounds on one file's extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Larger files are [`Skip::TooLarge`].
    pub max_file_size: u64,
    /// Parsing and querying past it is [`Skip::ParseTimeout`].
    pub parse_timeout: Duration,
    /// More is [`Skip::TooManyOccurrences`], or a SCIP document's
    /// `TOO_MANY_OCCURRENCES`.
    pub max_occurrences: usize,
}

impl Default for Limits {
    /// 1 MiB, 1 s, 100,000.
    fn default() -> Self {
        Self {
            max_file_size: 1 << 20,
            parse_timeout: Duration::from_secs(1),
            max_occurrences: 100_000,
        }
    }
}

/// The bytes a NUL check reads.
const BINARY_PREFIX: usize = 8 << 10;

/// Whether `bytes` look binary: a NUL in the first 8 KiB, as git decides.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_PREFIX)].contains(&0)
}

/// blake3 of `bytes` in lowercase hex: the content digest an upload names
/// for the file an indexer read.
pub fn content_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The start offset of every line in `bytes`.
pub(crate) fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i + 1),
    );
    starts
}

/// The bytes of line `line`, without its newline.
pub(crate) fn line_bytes<'a>(bytes: &'a [u8], starts: &[usize], line: usize) -> Option<&'a [u8]> {
    let start = *starts.get(line)?;
    let end = starts.get(line + 1).map_or(bytes.len(), |next| next - 1);
    bytes.get(start..end)
}

/// The byte offset of a line and byte column, if it lies in `bytes`.
pub(crate) fn offset(bytes: &[u8], starts: &[usize], line: u32, col: u32) -> Option<usize> {
    let line = usize::try_from(line).ok()?;
    let col = usize::try_from(col).ok()?;
    let text = line_bytes(bytes, starts, line)?;
    (col <= text.len()).then(|| starts[line] + col)
}

/// Sorts `occurrences` by range, then fills each one's `enclosing` by
/// containment and each definition's `digest` from `bytes`.
pub(crate) fn finish(mut occurrences: Vec<Occurrence>, bytes: &[u8]) -> Vec<Occurrence> {
    occurrences.sort_by_key(|o| (o.range, o.role));
    let starts = line_starts(bytes);
    // A definition is placed by its definition range, so a decorator or a
    // signature before its name is inside it; everything else by its range.
    let outer = |o: &Occurrence| o.definition_range.unwrap_or(o.range);
    let mut order: Vec<usize> = (0..occurrences.len()).collect();
    order.sort_by(|&a, &b| {
        let (ra, rb) = (outer(&occurrences[a]), outer(&occurrences[b]));
        (ra.start_line, ra.start_col)
            .cmp(&(rb.start_line, rb.start_col))
            .then((rb.end_line, rb.end_col).cmp(&(ra.end_line, ra.end_col)))
            .then(a.cmp(&b))
    });
    let mut open: Vec<usize> = Vec::new();
    for i in order {
        let range = outer(&occurrences[i]);
        while let Some(&top) = open.last() {
            let top_range = outer(&occurrences[top]);
            if top_range.contains(&range) {
                break;
            }
            open.pop();
        }
        occurrences[i].enclosing = open.last().map(|&top| index(top));
        if occurrences[i].definition_range.is_some() {
            open.push(i);
        }
    }
    for occurrence in &mut occurrences {
        occurrence.digest = occurrence.definition_range.and_then(|r| {
            let start = offset(bytes, &starts, r.start_line, r.start_col)?;
            let end = offset(bytes, &starts, r.end_line, r.end_col)?;
            let hash = blake3::hash(bytes.get(start..end)?);
            let mut first = [0; 8];
            first.copy_from_slice(&hash.as_bytes()[..8]);
            Some(u64::from_le_bytes(first))
        });
    }
    occurrences
}

/// An occurrence's index as stored: files hold at most
/// [`Limits::max_occurrences`], far below `u32::MAX`.
fn index(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_by_extension() {
        assert_eq!(Language::from_path("a/b.py"), Some(Language::Python));
        assert_eq!(Language::from_path("x.d.ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_path("x.jsx"), Some(Language::JavaScript));
        assert_eq!(Language::from_path("README.md"), None);
        assert_eq!(Language::from_path("Makefile"), None);
        assert_eq!(Language::from_path("dir.rs/file"), None);
        assert_eq!(extractor(Language::Rust), "tags/1/rust/0.24.2");
        for language in Language::ALL {
            assert_eq!(Language::from_name(language.name()), Some(language));
        }
    }

    /// `grammar_version` is part of every entry's extractor string, so it
    /// must move with the grammar crate.
    #[test]
    fn grammar_versions_match_the_lockfile() {
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
                .expect("Cargo.lock at the workspace root");
        for (language, krate) in [
            (Language::Python, "tree-sitter-python"),
            (Language::TypeScript, "tree-sitter-typescript"),
            (Language::Go, "tree-sitter-go"),
            (Language::Rust, "tree-sitter-rust"),
        ] {
            let entry = format!(
                "name = \"{krate}\"\nversion = \"{}\"",
                language.grammar_version()
            );
            assert!(
                lock.contains(&entry),
                "{krate} is not {}",
                language.grammar_version()
            );
        }
    }

    #[test]
    fn offsets_stay_within_lines() {
        let bytes = b"ab\ncd\n";
        let starts = line_starts(bytes);
        assert_eq!(offset(bytes, &starts, 0, 2), Some(2));
        assert_eq!(offset(bytes, &starts, 0, 3), None);
        assert_eq!(offset(bytes, &starts, 1, 1), Some(4));
        assert_eq!(offset(bytes, &starts, 2, 0), Some(6));
        assert_eq!(offset(bytes, &starts, 3, 0), None);
    }
}
