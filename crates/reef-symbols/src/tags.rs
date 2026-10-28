//! Tag extraction with Reef's queries (`queries/*.scm`).
//!
//! A query names an occurrence's node `@definition.<kind>` or
//! `@reference.<kind>`, its name `@name`, and for Go's methods and Rust's
//! `impl` items the type the definition belongs to `@scope`. Extraction then:
//!
//! 1. keeps one occurrence per name: of two definitions that capture one
//!    name, the wider wins and, at equal width, the earlier pattern; a
//!    reference whose name a definition captured is dropped;
//! 2. finds each one's enclosing definition by containment
//!    ([`crate::finish`]); a Python function directly in a class becomes a
//!    method;
//! 3. gives each definition its symbol: the file's path components as
//!    namespaces (for Go, its directory's: a package is a directory), the
//!    enclosing definitions' descriptors, the scope's type, then its own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::OnceLock;
use std::time::Instant;

use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Node, ParseOptions, Parser, Query, QueryCursor, QueryCursorOptions, Tree};

use crate::symbol::{Descriptor, Suffix, tags_symbol};
use crate::{FileSymbols, Kind, Language, Limits, Occurrence, Range, Role, Skip};

/// The compiled query and capture ids of one grammar.
struct Compiled {
    query: Query,
    name: u32,
    scope: Option<u32>,
    /// `(capture id, role, kind)` for every occurrence capture.
    occurrences: Vec<(u32, Role, Kind)>,
}

/// Grammars, by which the TypeScript query runs twice.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Grammar {
    Python,
    TypeScript,
    Tsx,
    Go,
    Rust,
}

impl Grammar {
    const fn of(language: Language) -> Self {
        match language {
            Language::Python => Self::Python,
            Language::TypeScript => Self::TypeScript,
            Language::Tsx | Language::JavaScript => Self::Tsx,
            Language::Go => Self::Go,
            Language::Rust => Self::Rust,
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
        }
    }

    const fn source(self) -> &'static str {
        match self {
            Self::Python => include_str!("../queries/python.scm"),
            Self::TypeScript | Self::Tsx => include_str!("../queries/typescript.scm"),
            Self::Go => include_str!("../queries/go.scm"),
            Self::Rust => include_str!("../queries/rust.scm"),
        }
    }

    /// The compiled query, once per process: compiling TypeScript's takes
    /// milliseconds, so never per file.
    fn compiled(self) -> &'static Compiled {
        static CELLS: [OnceLock<Compiled>; 5] = [const { OnceLock::new() }; 5];
        let cell = &CELLS[self as usize];
        cell.get_or_init(|| compile(&self.language(), self.source()))
    }
}

fn kind_of(word: &str) -> Kind {
    match word {
        "module" => Kind::Module,
        "class" => Kind::Class,
        "struct" => Kind::Struct,
        "interface" | "implementation" => Kind::Interface,
        "enum" => Kind::Enum,
        "type" => Kind::Type,
        "function" | "call" => Kind::Function,
        "method" => Kind::Method,
        "field" => Kind::Field,
        "variable" => Kind::Variable,
        "constant" => Kind::Constant,
        "macro" => Kind::Macro,
        _ => Kind::Unspecified,
    }
}

#[expect(
    clippy::expect_used,
    reason = "the queries are constants, compiled against their own grammars in every test run"
)]
fn compile(language: &tree_sitter::Language, source: &str) -> Compiled {
    let query = Query::new(language, source).expect("Reef's tag queries compile");
    let mut name = None;
    let mut scope = None;
    let mut occurrences = Vec::new();
    for (id, capture) in query.capture_names().iter().enumerate() {
        let id = u32::try_from(id).expect("a query has few captures");
        if *capture == "name" {
            name = Some(id);
        } else if *capture == "scope" {
            scope = Some(id);
        } else if let Some(kind) = capture.strip_prefix("definition.") {
            occurrences.push((id, Role::Definition, kind_of(kind)));
        } else if let Some(kind) = capture.strip_prefix("reference.") {
            occurrences.push((id, Role::Reference, kind_of(kind)));
        }
    }
    Compiled {
        query,
        name: name.expect("every tag query captures @name"),
        scope,
        occurrences,
    }
}

thread_local! {
    /// One parser per thread and grammar: a parser holds allocations a
    /// worker reuses across files.
    static PARSERS: RefCell<HashMap<Grammar, Parser>> = RefCell::new(HashMap::new());
}

fn range_of(node: Node<'_>) -> Range {
    let (start, end) = (node.start_position(), node.end_position());
    let clamp = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    Range {
        start_line: clamp(start.row),
        start_col: clamp(start.column),
        end_line: clamp(end.row),
        end_col: clamp(end.column),
    }
}

/// One capture before deduplication.
struct Tag {
    pattern: usize,
    role: Role,
    kind: Kind,
    name: String,
    range: Range,
    definition_range: Range,
    scope: Option<String>,
}

/// The base name of a scope's type: `Foo` for `Foo<T>`, `a::Foo`, `*Foo`,
/// `&Foo` or `pkg.Foo`.
fn scope_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let inner = match node.kind() {
        "type_identifier" | "identifier" | "field_identifier" | "primitive_type" => {
            return node.utf8_text(bytes).ok().map(str::to_owned);
        }
        "generic_type" | "reference_type" => node.child_by_field_name("type"),
        "scoped_type_identifier" | "qualified_type" => node.child_by_field_name("name"),
        "pointer_type" | "parenthesized_type" => node.named_child(0),
        _ => None,
    };
    if let Some(inner) = inner {
        return scope_name(inner, bytes);
    }
    let text = node.utf8_text(bytes).ok()?;
    let base = text.split(['<', '[', ' ']).next()?.rsplit("::").next()?;
    (!base.is_empty()).then(|| base.to_owned())
}

/// Extracts `bytes`' tags as `language` at repository path `path`.
pub fn extract(
    language: Language,
    path: &str,
    bytes: &[u8],
    limits: &Limits,
) -> Result<FileSymbols, Skip> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limits.max_file_size {
        return Err(Skip::TooLarge);
    }
    if crate::is_binary(bytes) {
        return Err(Skip::Binary);
    }
    let grammar = Grammar::of(language);
    let deadline = Instant::now() + limits.parse_timeout;
    let tree = parse(grammar, bytes, deadline)?;
    let tags = run_query(grammar.compiled(), &tree, bytes, deadline)?;
    let occurrences = dedupe(tags);
    if occurrences.len() > limits.max_occurrences {
        return Err(Skip::TooManyOccurrences);
    }
    let mut occurrences = crate::finish(occurrences, bytes);
    if language == Language::Python {
        for i in 0..occurrences.len() {
            let parent = occurrences[i]
                .enclosing
                .map(|e| occurrences[e as usize].kind);
            if occurrences[i].kind == Kind::Function
                && occurrences[i].role == Role::Definition
                && parent == Some(Kind::Class)
            {
                occurrences[i].kind = Kind::Method;
            }
        }
    }
    name_symbols(&mut occurrences, language, path);
    Ok(FileSymbols {
        occurrences,
        has_errors: tree.root_node().has_error(),
    })
}

/// Parses `bytes`, stopping at `deadline`.
fn parse(grammar: Grammar, bytes: &[u8], deadline: Instant) -> Result<Tree, Skip> {
    let mut late = false;
    let mut check = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            late = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = PARSERS.with_borrow_mut(|parsers| {
        let parser = parsers.entry(grammar).or_insert_with(|| {
            let mut parser = Parser::new();
            #[expect(
                clippy::expect_used,
                reason = "the grammar crates are built for this tree-sitter"
            )]
            parser
                .set_language(&grammar.language())
                .expect("the grammar matches the tree-sitter version");
            parser
        });
        let len = bytes.len();
        let tree = parser.parse_with_options(
            &mut |at, _| if at < len { &bytes[at..] } else { &[] },
            None,
            Some(ParseOptions::new().progress_callback(&mut check)),
        );
        // A stopped parse keeps its state for a resume; this one never resumes.
        parser.reset();
        tree
    });
    match tree {
        Some(tree) if !late && Instant::now() < deadline => Ok(tree),
        _ => Err(Skip::ParseTimeout),
    }
}

/// Runs the grammar's query over `tree`, stopping at `deadline`.
fn run_query(
    compiled: &Compiled,
    tree: &Tree,
    bytes: &[u8],
    deadline: Instant,
) -> Result<Vec<Tag>, Skip> {
    let mut tags = Vec::new();
    let mut late = false;
    let mut check = |_: &tree_sitter::QueryCursorState| {
        if Instant::now() >= deadline {
            late = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches_with_options(
        &compiled.query,
        tree.root_node(),
        bytes,
        QueryCursorOptions::new().progress_callback(&mut check),
    );
    while let Some(m) = matches.next() {
        let mut name = None;
        let mut scope = None;
        let mut occurrence = None;
        for capture in m.captures() {
            if capture.index == compiled.name {
                name = Some(capture.node);
            } else if Some(capture.index) == compiled.scope {
                scope = Some(capture.node);
            } else if let Some(&(_, role, kind)) = compiled
                .occurrences
                .iter()
                .find(|(id, ..)| *id == capture.index)
            {
                occurrence = Some((capture.node, role, kind));
            }
        }
        let (Some(name), Some((node, role, kind))) = (name, occurrence) else {
            continue;
        };
        let Ok(text) = name.utf8_text(bytes) else {
            continue;
        };
        tags.push(Tag {
            pattern: m.pattern_index,
            role,
            kind,
            name: text.to_owned(),
            range: range_of(name),
            definition_range: range_of(node),
            scope: scope.and_then(|s| scope_name(s, bytes)),
        });
    }
    drop(matches);
    if late {
        Err(Skip::ParseTimeout)
    } else {
        Ok(tags)
    }
}

/// One occurrence per name range (see the module comment). The scope rides
/// in `symbol` until [`name_symbols`] replaces it.
fn dedupe(tags: Vec<Tag>) -> Vec<Occurrence> {
    let mut definitions: HashMap<Range, Tag> = HashMap::new();
    let mut references: HashMap<Range, Tag> = HashMap::new();
    for tag in tags {
        let map = match tag.role {
            Role::Definition => &mut definitions,
            Role::Reference => &mut references,
        };
        match map.get(&tag.range) {
            Some(held) if !better(&tag, held) => {}
            _ => {
                map.insert(tag.range, tag);
            }
        }
    }
    references.retain(|range, _| !definitions.contains_key(range));
    definitions
        .into_values()
        .chain(references.into_values())
        .map(|tag| Occurrence {
            symbol: tag.scope,
            name: tag.name,
            kind: tag.kind,
            role: tag.role,
            flags: 0,
            range: tag.range,
            definition_range: (tag.role == Role::Definition).then_some(tag.definition_range),
            enclosing: None,
            digest: None,
        })
        .collect()
}

/// Whether `tag` beats `held` for one name.
fn better(tag: &Tag, held: &Tag) -> bool {
    let width = |t: &Tag| t.definition_range;
    let wider = width(tag).contains(&width(held)) && width(tag) != width(held);
    let narrower = width(held).contains(&width(tag)) && width(tag) != width(held);
    match tag.role {
        Role::Definition if wider => true,
        Role::Definition if narrower => false,
        _ => tag.pattern < held.pattern,
    }
}

fn suffix(kind: Kind) -> Suffix {
    match kind {
        Kind::Module => Suffix::Namespace,
        Kind::Class | Kind::Struct | Kind::Interface | Kind::Enum | Kind::Type => Suffix::Type,
        Kind::Function | Kind::Method => Suffix::Method(None),
        Kind::Macro => Suffix::Macro,
        Kind::Field | Kind::Variable | Kind::Constant | Kind::Unspecified => Suffix::Term,
    }
}

/// Replaces each definition's carried scope with its symbol, and clears
/// references'.
fn name_symbols(occurrences: &mut [Occurrence], language: Language, path: &str) {
    let mut prefix: Vec<Descriptor> = path
        .split('/')
        .filter(|c| !c.is_empty())
        .map(|c| Descriptor::new(c, Suffix::Namespace))
        .collect();
    if language == Language::Go {
        prefix.pop();
    }
    // Outermost first, so one pass sees every parent's descriptors before
    // its children's, even when a child's name comes before its parent's.
    let mut order: Vec<usize> = (0..occurrences.len()).collect();
    order.sort_by_key(|&i| {
        let r = occurrences[i]
            .definition_range
            .unwrap_or(occurrences[i].range);
        (
            r.start_line,
            r.start_col,
            std::cmp::Reverse((r.end_line, r.end_col)),
        )
    });
    let mut chains: Vec<Option<Vec<Descriptor>>> = vec![None; occurrences.len()];
    for i in order {
        let occurrence = &occurrences[i];
        if occurrence.role != Role::Definition {
            occurrences[i].symbol = None;
            continue;
        }
        let mut chain = occurrence
            .enclosing
            .and_then(|e| chains[e as usize].clone())
            .unwrap_or_else(|| prefix.clone());
        if let Some(scope) = &occurrence.symbol {
            chain.push(Descriptor::new(scope.clone(), Suffix::Type));
        }
        chain.push(Descriptor::new(
            occurrence.name.clone(),
            suffix(occurrence.kind),
        ));
        occurrences[i].symbol = Some(tags_symbol(&chain));
        chains[i] = Some(chain);
    }
}
