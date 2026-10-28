//! SCIP's symbol-string grammar (`scip.proto`, message `Symbol`): format
//! and parse.
//!
//! ```text
//! <symbol>  ::= <scheme> ' ' <manager> ' ' <package-name> ' ' <version> ' ' (<descriptor>)+
//!             | 'local ' <local-id>
//! ```
//!
//! Spaces in the scheme and package fields are doubled, `.` stands for an
//! empty package field, and a name that is not all identifier characters
//! (`_ + - $`, ASCII letters and digits) is wrapped in backticks, with
//! backticks doubled. Tags definitions use the scheme [`TAGS_SCHEME`] and an
//! empty package.

use std::borrow::Cow;
use std::fmt::Write as _;

/// The scheme of the symbols [`crate::extract`] makes.
pub const TAGS_SCHEME: &str = "reef-tags";

/// What a descriptor names, by its suffix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Suffix {
    /// `name/`: a namespace, package or module.
    Namespace,
    /// `name#`: a type.
    Type,
    /// `name.`: a term: a field, variable or constant.
    Term,
    /// `name(disambiguator).`: a method or function.
    Method(Option<String>),
    /// `[name]`: a type parameter.
    TypeParameter,
    /// `(name)`: a parameter.
    Parameter,
    /// `name:`: meta.
    Meta,
    /// `name!`: a macro.
    Macro,
}

/// One descriptor of a global symbol.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Descriptor {
    /// Its name, unescaped.
    pub name: String,
    /// What it names.
    pub suffix: Suffix,
}

impl Descriptor {
    /// A descriptor of `name` with `suffix`.
    pub fn new(name: impl Into<String>, suffix: Suffix) -> Self {
        Self {
            name: name.into(),
            suffix,
        }
    }
}

/// A parsed symbol string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Symbol {
    /// A symbol local to one document: `local <id>`.
    Local(String),
    /// A global symbol.
    Global {
        /// Its scheme, such as `scip-typescript` or [`TAGS_SCHEME`].
        scheme: String,
        /// Package manager, name and version; empty for `.`.
        package: [String; 3],
        /// Its descriptors, outermost first, never empty.
        descriptors: Vec<Descriptor>,
    },
}

impl Symbol {
    /// The last descriptor's name, which is a symbol's display name when its
    /// indexer gives none. `None` for a local.
    pub fn last_name(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Global { descriptors, .. } => descriptors.last().map(|d| d.name.as_str()),
        }
    }
}

/// Why a symbol string does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a SCIP symbol: {reason} at byte {at}")]
pub struct ParseError {
    /// What was wrong.
    pub reason: &'static str,
    /// Where.
    pub at: usize,
}

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '$')
}

/// `name` as SCIP writes it: bare when all identifier characters, else in
/// backticks with backticks doubled.
pub fn escape_name(name: &str) -> Cow<'_, str> {
    if !name.is_empty() && name.chars().all(is_identifier_char) {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("`{}`", name.replace('`', "``")))
    }
}

fn escape_field(field: &str) -> Cow<'_, str> {
    if field.is_empty() {
        Cow::Borrowed(".")
    } else {
        Cow::Owned(field.replace(' ', "  "))
    }
}

/// Formats a global symbol. `package` is manager, name and version, each
/// empty for none.
pub fn format(scheme: &str, package: [&str; 3], descriptors: &[Descriptor]) -> String {
    let mut out = String::new();
    out.push_str(&escape_field(scheme));
    for field in package {
        out.push(' ');
        out.push_str(&escape_field(field));
    }
    out.push(' ');
    for descriptor in descriptors {
        push_descriptor(&mut out, descriptor);
    }
    out
}

/// Appends one descriptor in SCIP's form.
pub fn push_descriptor(out: &mut String, descriptor: &Descriptor) {
    let name = escape_name(&descriptor.name);
    let _ = match &descriptor.suffix {
        Suffix::Namespace => write!(out, "{name}/"),
        Suffix::Type => write!(out, "{name}#"),
        Suffix::Term => write!(out, "{name}."),
        Suffix::Method(None) => write!(out, "{name}()."),
        Suffix::Method(Some(d)) => write!(out, "{name}({d})."),
        Suffix::TypeParameter => write!(out, "[{name}]"),
        Suffix::Parameter => write!(out, "({name})"),
        Suffix::Meta => write!(out, "{name}:"),
        Suffix::Macro => write!(out, "{name}!"),
    };
}

/// A tags symbol: [`TAGS_SCHEME`], an empty package, then `descriptors`.
pub fn tags_symbol(descriptors: &[Descriptor]) -> String {
    format(TAGS_SCHEME, ["", "", ""], descriptors)
}

struct Parser<'a> {
    s: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn fail<T>(&self, reason: &'static str) -> Result<T, ParseError> {
        Err(ParseError {
            reason,
            at: self.at,
        })
    }

    fn peek(&self) -> Option<char> {
        self.s[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.at += c.len_utf8();
            true
        } else {
            false
        }
    }

    /// A space-terminated field, with doubled spaces as literal ones.
    fn field(&mut self) -> Result<String, ParseError> {
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return self.fail("missing a space-separated field"),
                Some(' ') if self.eat(' ') => out.push(' '),
                Some(' ') => break,
                Some(c) => out.push(c),
            }
        }
        if out.is_empty() {
            return self.fail("empty field");
        }
        Ok(if out == "." { String::new() } else { out })
    }

    fn name(&mut self) -> Result<String, ParseError> {
        if self.eat('`') {
            let mut out = String::new();
            loop {
                match self.bump() {
                    None => return self.fail("unterminated backtick name"),
                    Some('`') if self.eat('`') => out.push('`'),
                    Some('`') => break,
                    Some(c) => out.push(c),
                }
            }
            if out.is_empty() {
                return self.fail("empty name");
            }
            Ok(out)
        } else {
            let start = self.at;
            while self.peek().is_some_and(is_identifier_char) {
                self.at += 1;
            }
            if self.at == start {
                return self.fail("expected a name");
            }
            Ok(self.s[start..self.at].to_owned())
        }
    }

    fn descriptor(&mut self) -> Result<Descriptor, ParseError> {
        if self.eat('[') {
            let name = self.name()?;
            if !self.eat(']') {
                return self.fail("expected `]`");
            }
            return Ok(Descriptor::new(name, Suffix::TypeParameter));
        }
        if self.eat('(') {
            let name = self.name()?;
            if !self.eat(')') {
                return self.fail("expected `)`");
            }
            return Ok(Descriptor::new(name, Suffix::Parameter));
        }
        let name = self.name()?;
        let suffix = match self.bump() {
            Some('/') => Suffix::Namespace,
            Some('#') => Suffix::Type,
            Some('.') => Suffix::Term,
            Some(':') => Suffix::Meta,
            Some('!') => Suffix::Macro,
            Some('(') => {
                let start = self.at;
                while self.peek().is_some_and(is_identifier_char) {
                    self.at += 1;
                }
                let disambiguator = (self.at > start).then(|| self.s[start..self.at].to_owned());
                if !(self.eat(')') && self.eat('.')) {
                    return self.fail("expected `).`");
                }
                Suffix::Method(disambiguator)
            }
            _ => return self.fail("expected a descriptor suffix"),
        };
        Ok(Descriptor::new(name, suffix))
    }
}

/// Parses a symbol string.
pub fn parse(s: &str) -> Result<Symbol, ParseError> {
    if let Some(id) = s.strip_prefix("local ") {
        if id.is_empty() || !id.chars().all(is_identifier_char) {
            return Err(ParseError {
                reason: "a local id is identifier characters",
                at: 6,
            });
        }
        return Ok(Symbol::Local(id.to_owned()));
    }
    let mut p = Parser { s, at: 0 };
    let scheme = p.field()?;
    if scheme.is_empty() || scheme.starts_with("local") {
        return Err(ParseError {
            reason: "a scheme is not empty and does not start with `local`",
            at: 0,
        });
    }
    let package = [p.field()?, p.field()?, p.field()?];
    let mut descriptors = Vec::new();
    while p.at < s.len() {
        descriptors.push(p.descriptor()?);
    }
    if descriptors.is_empty() {
        return p.fail("a global symbol has a descriptor");
    }
    Ok(Symbol::Global {
        scheme,
        package,
        descriptors,
    })
}
