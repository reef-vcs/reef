//! SCIP documents, normalized to [`FileSymbols`].
//!
//! [`normalize`] decodes one encoded `scip.Document` that an indexer wrote
//! for `bytes`, which the caller has already checked against its digest,
//! and converts it:
//!
//! - positions to UTF-8 byte columns, from the document's
//!   `position_encoding`, or, when it is unspecified, UTF-16 for
//!   `scip-typescript` and `scip-python` (both run on TypeScript) and UTF-8
//!   for any other tool;
//! - a typed range wins over the deprecated `repeated int32 range`, whose
//!   three- and four-element forms are both read;
//! - `local` symbols, diagnostics, documentation, syntax kinds and
//!   relationships are dropped;
//! - kinds from the document's `SymbolInformation`, else from the last
//!   descriptor's suffix; names from `display_name`, else the last
//!   descriptor's name;
//! - Definition is [`Role::Definition`] and anything else a reference, with
//!   `Import`, `WriteAccess`, `Test` and `Generated` kept as [`flags`];
//! - a definition's range is its enclosing range, else the definition
//!   range of the tags definition whose name range equals its range.

use std::cell::RefCell;
use std::collections::HashMap;

use prost::Message as _;

use crate::symbol::{self, Suffix, Symbol};
use crate::{FileSymbols, Kind, Limits, Occurrence, Range, Role, flags};

/// SCIP's messages, generated from the vendored `proto/scip.proto`.
#[allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    unreachable_pub,
    unused_qualifications,
    reason = "generated code"
)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/scip.rs"));
}

/// Why a document was rejected. The values are the API's `ScipRejection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[repr(u8)]
pub enum Rejection {
    /// The bytes are not a `scip.Document`, or a range or symbol in it is
    /// malformed.
    #[error("not a valid SCIP document")]
    InvalidDocument = 7,
    /// More occurrences than [`Limits::max_occurrences`], locals dropped.
    #[error("more occurrences than the limit")]
    TooManyOccurrences = 8,
    /// A range lies outside the file.
    #[error("a range lies outside the file")]
    RangeOutOfBounds = 9,
}

/// How a document counts columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    fn of(document: &proto::Document, tool: &str) -> Self {
        match proto::PositionEncoding::try_from(document.position_encoding) {
            Ok(proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart) => Self::Utf8,
            Ok(proto::PositionEncoding::Utf16CodeUnitOffsetFromLineStart) => Self::Utf16,
            Ok(proto::PositionEncoding::Utf32CodeUnitOffsetFromLineStart) => Self::Utf32,
            _ if matches!(tool, "scip-typescript" | "scip-python") => Self::Utf16,
            _ => Self::Utf8,
        }
    }
}

/// A range before conversion: lines and code-unit columns.
type Raw = (i32, i32, i32, i32);

fn single(r: &proto::SingleLineRange) -> Raw {
    (r.line, r.start_character, r.line, r.end_character)
}

fn multi(r: &proto::MultiLineRange) -> Raw {
    (r.start_line, r.start_character, r.end_line, r.end_character)
}

/// The deprecated `[line, start, end]` or `[start line, start, end line, end]`.
fn packed(range: &[i32]) -> Result<Option<Raw>, Rejection> {
    match *range {
        [] => Ok(None),
        [line, start, end] => Ok(Some((line, start, line, end))),
        [start_line, start, end_line, end] => Ok(Some((start_line, start, end_line, end))),
        _ => Err(Rejection::InvalidDocument),
    }
}

#[expect(deprecated, reason = "older indexers write only the packed range")]
fn occurrence_range(o: &proto::Occurrence) -> Result<Raw, Rejection> {
    match &o.typed_range {
        Some(proto::occurrence::TypedRange::SingleLineRange(r)) => Ok(single(r)),
        Some(proto::occurrence::TypedRange::MultiLineRange(r)) => Ok(multi(r)),
        None => packed(&o.range)?.ok_or(Rejection::InvalidDocument),
    }
}

#[expect(deprecated, reason = "older indexers write only the packed range")]
fn enclosing_range(o: &proto::Occurrence) -> Result<Option<Raw>, Rejection> {
    match &o.typed_enclosing_range {
        Some(proto::occurrence::TypedEnclosingRange::SingleLineEnclosingRange(r)) => {
            Ok(Some(single(r)))
        }
        Some(proto::occurrence::TypedEnclosingRange::MultiLineEnclosingRange(r)) => {
            Ok(Some(multi(r)))
        }
        None => packed(&o.enclosing_range),
    }
}

/// Converts code-unit columns to byte columns against the file's lines.
struct Lines<'a> {
    bytes: &'a [u8],
    starts: Vec<usize>,
    encoding: Encoding,
    /// Per line, the byte offset of each code-unit column (`u32::MAX`
    /// inside a surrogate pair), built once: a minified file's one line is
    /// read once, not once per occurrence.
    columns: RefCell<HashMap<usize, Vec<u32>>>,
}

/// Columns of one line: entry `n` is the byte offset after `n` code units.
fn unit_offsets(text: &str, utf16: bool) -> Vec<u32> {
    let mut offsets = vec![0];
    for (offset, c) in text.char_indices() {
        let end = u32::try_from(offset + c.len_utf8()).unwrap_or(u32::MAX);
        if utf16 && c.len_utf16() == 2 {
            offsets.push(u32::MAX);
        }
        offsets.push(end);
    }
    offsets
}

impl Lines<'_> {
    fn column(&self, line: i32, units: i32) -> Result<u32, Rejection> {
        let (Ok(line), Ok(units)) = (usize::try_from(line), usize::try_from(units)) else {
            return Err(Rejection::RangeOutOfBounds);
        };
        let text =
            crate::line_bytes(self.bytes, &self.starts, line).ok_or(Rejection::RangeOutOfBounds)?;
        let at = match self.encoding {
            Encoding::Utf8 => (units <= text.len()).then_some(units),
            // Zero units is the line's start, whatever its bytes.
            Encoding::Utf16 | Encoding::Utf32 if units == 0 => Some(0),
            Encoding::Utf16 | Encoding::Utf32 => {
                let mut columns = self.columns.borrow_mut();
                let offsets = match columns.entry(line) {
                    std::collections::hash_map::Entry::Occupied(found) => found.into_mut(),
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        // A line that is not UTF-8 has no code units to count.
                        let text =
                            std::str::from_utf8(text).map_err(|_| Rejection::RangeOutOfBounds)?;
                        slot.insert(unit_offsets(text, self.encoding == Encoding::Utf16))
                    }
                };
                offsets
                    .get(units)
                    .copied()
                    .filter(|&b| b != u32::MAX)
                    .map(|b| b as usize)
            }
        };
        at.and_then(|b| u32::try_from(b).ok())
            .ok_or(Rejection::RangeOutOfBounds)
    }

    fn range(&self, (start_line, start, end_line, end): Raw) -> Result<Range, Rejection> {
        let range = Range {
            start_line: u32::try_from(start_line).map_err(|_| Rejection::RangeOutOfBounds)?,
            start_col: self.column(start_line, start)?,
            end_line: u32::try_from(end_line).map_err(|_| Rejection::RangeOutOfBounds)?,
            end_col: self.column(end_line, end)?,
        };
        if (range.end_line, range.end_col) < (range.start_line, range.start_col) {
            return Err(Rejection::RangeOutOfBounds);
        }
        Ok(range)
    }
}

/// A SCIP `SymbolInformation.Kind`, as [`Kind`].
fn kind_of_information(kind: i32) -> Kind {
    use proto::symbol_information::Kind as K;
    let Ok(kind) = K::try_from(kind) else {
        return Kind::Unspecified;
    };
    match kind {
        K::Module | K::Namespace | K::Package | K::PackageObject | K::Library => Kind::Module,
        K::Class | K::SingletonClass | K::Object => Kind::Class,
        K::Struct | K::Union => Kind::Struct,
        K::Interface | K::Trait | K::Protocol | K::TypeClass => Kind::Interface,
        K::Enum => Kind::Enum,
        K::Type | K::TypeAlias | K::AssociatedType | K::TypeFamily => Kind::Type,
        K::Function => Kind::Function,
        K::Macro => Kind::Macro,
        K::Method
        | K::AbstractMethod
        | K::StaticMethod
        | K::TraitMethod
        | K::TypeClassMethod
        | K::SingletonMethod
        | K::Constructor
        | K::MethodAlias
        | K::MethodReceiver
        | K::MethodSpecification
        | K::Getter
        | K::Setter
        | K::Accessor => Kind::Method,
        K::Field | K::Property | K::StaticField | K::StaticProperty | K::StaticDataMember => {
            Kind::Field
        }
        K::Variable | K::StaticVariable => Kind::Variable,
        K::Constant | K::EnumMember => Kind::Constant,
        _ => Kind::Unspecified,
    }
}

/// A kind from a symbol's last descriptor, when its indexer gave none.
fn kind_of_descriptors(symbol: &Symbol) -> Kind {
    let Symbol::Global { descriptors, .. } = symbol else {
        return Kind::Unspecified;
    };
    let parent_is_type = descriptors
        .len()
        .checked_sub(2)
        .is_some_and(|i| descriptors[i].suffix == Suffix::Type);
    match descriptors.last().map(|d| &d.suffix) {
        Some(Suffix::Namespace) => Kind::Module,
        Some(Suffix::Type) => Kind::Type,
        Some(Suffix::Method(_)) if parent_is_type => Kind::Method,
        Some(Suffix::Method(_)) => Kind::Function,
        Some(Suffix::Term) if parent_is_type => Kind::Field,
        Some(Suffix::Term) => Kind::Variable,
        Some(Suffix::Macro) => Kind::Macro,
        _ => Kind::Unspecified,
    }
}

fn flags_of(roles: i32) -> u8 {
    use proto::SymbolRole as R;
    let mut out = 0;
    for (role, flag) in [
        (R::Import, flags::IMPORT),
        (R::WriteAccess, flags::WRITE),
        (R::Test, flags::TEST),
        (R::Generated, flags::GENERATED),
    ] {
        if roles & role as i32 != 0 {
            out |= flag;
        }
    }
    out
}

/// Normalizes one encoded `scip.Document` for `bytes`, written by `tool`
/// (its `ToolInfo.name`). `tags` are the same bytes' tags, when the
/// language is one [`crate::extract`] reads; they fill definition ranges the
/// document leaves out.
pub fn normalize(
    document: &[u8],
    tool: &str,
    bytes: &[u8],
    tags: Option<&FileSymbols>,
    limits: &Limits,
) -> Result<FileSymbols, Rejection> {
    let document = proto::Document::decode(document).map_err(|_| Rejection::InvalidDocument)?;
    let lines = Lines {
        bytes,
        starts: crate::line_starts(bytes),
        encoding: Encoding::of(&document, tool),
        columns: RefCell::new(HashMap::new()),
    };
    let information: HashMap<&str, &proto::SymbolInformation> = document
        .symbols
        .iter()
        .map(|info| (info.symbol.as_str(), info))
        .collect();
    let tag_definitions: HashMap<Range, Range> = tags
        .into_iter()
        .flat_map(|t| &t.occurrences)
        .filter_map(|o| Some((o.range, o.definition_range?)))
        .collect();

    let mut parsed: HashMap<&str, Symbol> = HashMap::new();
    let mut occurrences = Vec::new();
    for o in &document.occurrences {
        if o.symbol.is_empty() || o.symbol.starts_with("local ") {
            continue;
        }
        if occurrences.len() == limits.max_occurrences {
            return Err(Rejection::TooManyOccurrences);
        }
        if !parsed.contains_key(o.symbol.as_str()) {
            let symbol = symbol::parse(&o.symbol).map_err(|_| Rejection::InvalidDocument)?;
            parsed.insert(&o.symbol, symbol);
        }
        let symbol = &parsed[o.symbol.as_str()];
        let info = information.get(o.symbol.as_str());
        let range = lines.range(occurrence_range(o)?)?;
        let role = if o.symbol_roles & proto::SymbolRole::Definition as i32 != 0 {
            Role::Definition
        } else {
            Role::Reference
        };
        let definition_range = match (role, enclosing_range(o)?) {
            (Role::Reference, _) => None,
            (Role::Definition, Some(raw)) => Some(lines.range(raw)?),
            (Role::Definition, None) => tag_definitions.get(&range).copied(),
        };
        let kind = match info.map(|i| kind_of_information(i.kind)) {
            Some(kind) if kind != Kind::Unspecified => kind,
            _ => kind_of_descriptors(symbol),
        };
        let name = info
            .map(|i| i.display_name.as_str())
            .filter(|n| !n.is_empty())
            .or_else(|| symbol.last_name())
            .unwrap_or_default()
            .to_owned();
        occurrences.push(Occurrence {
            symbol: Some(o.symbol.clone()),
            name,
            kind,
            role,
            flags: flags_of(o.symbol_roles),
            range,
            definition_range,
            enclosing: None,
            digest: None,
        });
    }
    Ok(FileSymbols {
        occurrences: crate::finish(occurrences, bytes),
        has_errors: false,
    })
}
