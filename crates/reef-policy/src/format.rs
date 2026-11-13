//! `reef-policy.toml`, format version 1: strict TOML whose tables declare
//! what a change needs, with CEL conditions, loaded into a [`Policy`].
//!
//! Loading parses the file, checks every reference between its tables,
//! compiles every pattern, and parses and statically checks every
//! expression ([`crate::check`]). Every problem it finds is a
//! [`Diagnostic`] with its line and column; a TOML syntax or schema error
//! stops at the first, since the parser cannot go on past it.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use toml::Spanned;

use crate::check::{self, Scope};
use crate::env::{self, Parsed};
use crate::glob::{PathFilter, Patterns};

/// The format version this crate reads.
pub const VERSION: u32 = 1;

/// The largest policy file, in bytes.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// The most `[[require]]`, `[[forbid]]`, `[[signoff]]`,
/// `[[conflicts.rule]]` and `[[rule]]` entries a file may have together.
pub const MAX_ENTRIES: usize = 256;

/// The most groups a file may name.
pub const MAX_GROUPS: usize = 64;

/// The most members of one group.
pub const MAX_GROUP_MEMBERS: usize = 256;

/// The most attestor sets a file may name.
pub const MAX_ATTESTOR_SETS: usize = 64;

/// The most patterns, identities or other entries in one list.
pub const MAX_LIST: usize = 256;

/// The longest CEL expression, in bytes of source.
pub const MAX_EXPRESSION_BYTES: usize = 8 * 1024;

/// The most sign-offs a `count` may ask for.
pub const MAX_COUNT: i64 = 64;

/// The fewest and most conflict resolution attempts a policy may allow.
pub const ATTEMPTS: std::ops::RangeInclusive<i64> = 1..=20;

/// `[landing] batch_max`'s range (R3.14).
pub const BATCH_MAX: std::ops::RangeInclusive<i64> = 1..=32;

/// `[landing] batch_max`'s default.
pub const DEFAULT_BATCH_MAX: u32 = 8;

/// `[landing] batch_wait`'s longest value.
pub const MAX_BATCH_WAIT: std::time::Duration = std::time::Duration::from_mins(10);

/// The files whose changes `builtin:owners` covers: the policy itself and
/// R3.06's check declarations, since a change to either changes how a tree
/// is judged.
pub const POLICY_FILES: [&str; 2] = [crate::POLICY_PATH, "reef-checks.toml"];

/// One problem with a policy file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Diagnostic")]
pub struct Diagnostic {
    /// The rule or table it is in (`tests`, `attestors.ci`, `limits`), or
    /// empty for the file.
    pub rule: String,
    /// The key it is about (`when`, `paths`), or empty.
    pub field: String,
    /// The line, from 1; 0 when not known.
    pub line: u32,
    /// The column, from 1, in characters; 0 when not known.
    pub column: u32,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: ", self.line, self.column)?;
        match (self.rule.is_empty(), self.field.is_empty()) {
            (false, false) => write!(f, "{}.{}: ", self.rule, self.field)?,
            (false, true) => write!(f, "{}: ", self.rule)?,
            (true, false) => write!(f, "{}: ", self.field)?,
            (true, true) => {}
        }
        f.write_str(&self.message)
    }
}

// ---------------------------------------------------------------------------
// The file as TOML.

type Text = Spanned<String>;
type List = Vec<Text>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[allow(dead_code)]
    version: i64,
    owners: Option<Spanned<RawOwners>>,
    #[serde(default)]
    groups: BTreeMap<String, Spanned<List>>,
    #[serde(default)]
    attestors: BTreeMap<String, Spanned<RawAttestor>>,
    #[serde(default)]
    require: Vec<Spanned<RawRequire>>,
    limits: Option<Spanned<RawLimits>>,
    #[serde(default)]
    forbid: Vec<Spanned<RawForbid>>,
    #[serde(default)]
    signoff: Vec<Spanned<RawSignoff>>,
    conflicts: Option<Spanned<RawConflicts>>,
    landing: Option<Spanned<RawLanding>>,
    history: Option<Spanned<RawHistory>>,
    #[serde(default)]
    rule: Vec<Spanned<RawRule>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHistory {
    message: Option<Text>,
    trailers: Option<Spanned<List>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLanding {
    checks: Option<Text>,
    batch_max: Option<Spanned<i64>>,
    batch_wait: Option<Text>,
    #[serde(default)]
    alone: List,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOwners {
    #[serde(default)]
    groups: List,
    #[serde(default)]
    identities: List,
    count: Option<Spanned<i64>>,
    presence: Option<Text>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAttestor {
    #[serde(default)]
    identities: List,
    #[serde(default)]
    groups: List,
    #[serde(default)]
    identity_kinds: List,
    actors: Option<Spanned<List>>,
    #[serde(default)]
    authorities: List,
    #[serde(default)]
    issuer_kinds: List,
    presence: Option<Text>,
    #[serde(rename = "self")]
    self_: Option<Text>,
    when: Option<Text>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequire {
    name: Text,
    #[serde(default)]
    paths: List,
    #[serde(default)]
    except: List,
    kinds: Spanned<List>,
    #[serde(default)]
    targets: List,
    attestors: Spanned<List>,
    when: Option<Text>,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_files: Option<Spanned<i64>>,
    max_lines: Option<Spanned<i64>>,
    max_file_size: Option<Spanned<toml::Value>>,
    #[serde(default)]
    except: List,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForbid {
    name: Text,
    paths: Spanned<List>,
    #[serde(default)]
    except: List,
    when: Option<Text>,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSignoff {
    name: Text,
    #[serde(default)]
    paths: List,
    #[serde(default)]
    except: List,
    #[serde(default)]
    groups: List,
    #[serde(default)]
    identities: List,
    count: Option<Spanned<i64>>,
    presence: Option<Text>,
    kinds: Option<Spanned<List>>,
    when: Option<Text>,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConflicts {
    tier: Option<Text>,
    attempts: Option<Spanned<i64>>,
    #[serde(default)]
    rule: Vec<Spanned<RawConflictRule>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConflictRule {
    #[serde(default)]
    paths: List,
    #[serde(default)]
    except: List,
    tier: Option<Text>,
    attempts: Option<Spanned<i64>>,
    when: Option<Text>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: Text,
    when: Option<Text>,
    expr: Text,
    message: Option<String>,
}

#[derive(Deserialize)]
struct VersionOnly {
    version: Option<Spanned<toml::Value>>,
}

// ---------------------------------------------------------------------------
// The loaded policy.

/// A loaded policy: every table checked, every pattern compiled and every
/// expression parsed and checked. Evaluate it with [`Policy::evaluate`].
#[derive(Debug)]
pub struct Policy {
    pub(crate) sha256: String,
    pub(crate) owners: Signoff,
    pub(crate) groups: BTreeMap<String, Vec<String>>,
    pub(crate) attestors: BTreeMap<String, Attestor>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) conflicts: Conflicts,
    pub(crate) landing: LandingSettings,
    pub(crate) history: HistorySettings,
}

impl Policy {
    /// The `[landing]` table, or its defaults (R3.14, ADR 0067).
    pub fn landing(&self) -> &LandingSettings {
        &self.landing
    }

    /// The `[history]` table, or its defaults (R3.16, ADR 0075).
    pub fn history(&self) -> &HistorySettings {
        &self.history
    }
}

/// `[history] message`: how a landed change's description is written on
/// the destination (R3.16).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Message {
    /// As the change has it: the commit lands as written. The default when
    /// the policy has no `[history]` table.
    #[default]
    Keep,
    /// The change's description normalized, the trailers the projection
    /// owns removed and its own appended. The default in a `[history]`
    /// table.
    Summary,
}

impl Message {
    /// Its text in the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Message::Keep => "keep",
            Message::Summary => "summary",
        }
    }

    /// The value `text` names.
    pub fn parse(text: &str) -> Option<Message> {
        match text {
            "keep" => Some(Message::Keep),
            "summary" => Some(Message::Summary),
            _ => None,
        }
    }
}

/// A set of trailers `[history] trailers` names (R3.16), in the order the
/// projection writes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TrailerSet {
    /// `Reef-Change: <change id>`.
    Change,
    /// `Reef-Landing: lnd_…`, and `Reef-Batch: bat_…` for a batch that
    /// landed more than one change.
    Landing,
    /// `Reef-Agent: <name>` for each agent of the change's provenance.
    Agents,
    /// `Reviewed-by: <name> <<email>>` for each person whose sign-off
    /// counted.
    Signoffs,
}

impl TrailerSet {
    /// Every set, in the order they are written.
    pub const ALL: [TrailerSet; 4] = [
        TrailerSet::Change,
        TrailerSet::Landing,
        TrailerSet::Agents,
        TrailerSet::Signoffs,
    ];

    /// Its text in the file.
    pub fn as_str(self) -> &'static str {
        match self {
            TrailerSet::Change => "change",
            TrailerSet::Landing => "landing",
            TrailerSet::Agents => "agents",
            TrailerSet::Signoffs => "signoffs",
        }
    }

    /// The value `text` names.
    pub fn parse(text: &str) -> Option<TrailerSet> {
        TrailerSet::ALL.into_iter().find(|set| set.as_str() == text)
    }
}

/// `[history]`: how landed changes are written on the destination whose
/// tree has this policy (R3.16, ADR 0075).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistorySettings {
    /// How each landed change's description is written.
    pub message: Message,
    /// The trailer sets a summary appends, in the order written; empty
    /// under [`Message::Keep`].
    pub trailers: Vec<TrailerSet>,
}

impl HistorySettings {
    /// The trailers a `[history]` table writes when it names none.
    pub const DEFAULT_TRAILERS: [TrailerSet; 2] = [TrailerSet::Change, TrailerSet::Landing];

    /// Whether landed changes are summarized.
    pub fn summarized(&self) -> bool {
        self.message == Message::Summary
    }
}

/// Where a landing judges a change's `[[require]]` evidence (R3.14).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "policy.Checks")]
pub enum Checks {
    /// On the submitted tree at the gate, and again on the batch's tested
    /// tree at landing.
    #[default]
    Batch,
    /// On the submitted tree only (R3.13's landing).
    Proposal,
}

impl Checks {
    /// Its text in the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Checks::Batch => "batch",
            Checks::Proposal => "proposal",
        }
    }

    /// The value `text` names.
    pub fn parse(text: &str) -> Option<Checks> {
        match text {
            "batch" => Some(Checks::Batch),
            "proposal" => Some(Checks::Proposal),
            _ => None,
        }
    }
}

/// `[landing]`: how the landing service batches and checks landings onto
/// the destination whose tree has this policy (R3.14, ADR 0067).
#[derive(Debug)]
pub struct LandingSettings {
    /// Where `[[require]]` evidence must hold.
    pub checks: Checks,
    /// The most changes in a batch.
    pub batch_max: u32,
    /// How long a batch waits for more changes after its oldest was queued.
    pub batch_wait: std::time::Duration,
    /// Paths whose changes land in a batch of their own.
    pub alone: Patterns,
}

impl Default for LandingSettings {
    fn default() -> Self {
        LandingSettings {
            checks: Checks::Batch,
            batch_max: DEFAULT_BATCH_MAX,
            batch_wait: std::time::Duration::ZERO,
            alone: Patterns::empty(),
        }
    }
}

impl LandingSettings {
    /// Whether a change touching `paths` lands in a batch of its own: it
    /// touches a policy file ([`POLICY_FILES`]) or an `alone` path.
    pub fn alone<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> bool {
        paths.into_iter().any(|path| {
            POLICY_FILES
                .iter()
                .any(|file| path.eq_ignore_ascii_case(file))
                || (!self.alone.is_empty() && self.alone.matches(path))
        })
    }
}

/// `text` as a duration: digits then `ms`, `s` or `m`.
fn parse_duration(text: &str) -> Option<std::time::Duration> {
    let text = text.trim();
    let digits = text.find(|c: char| !c.is_ascii_digit())?;
    if digits == 0 {
        return None;
    }
    let value: u64 = text[..digits].parse().ok()?;
    match &text[digits..] {
        "ms" => Some(std::time::Duration::from_millis(value)),
        "s" => Some(std::time::Duration::from_secs(value)),
        "m" => value.checked_mul(60).map(std::time::Duration::from_secs),
        _ => None,
    }
}

/// One rule of the file, in file order.
#[derive(Debug)]
pub(crate) enum Entry {
    Require(Require),
    Limits(Limits),
    Forbid(Forbid),
    Signoff(Signoff),
    Rule(Rule),
}

impl Entry {
    pub(crate) fn name(&self) -> &str {
        match self {
            Entry::Require(require) => &require.name,
            Entry::Limits(_) => "limits",
            Entry::Forbid(forbid) => &forbid.name,
            Entry::Signoff(signoff) => &signoff.name,
            Entry::Rule(rule) => &rule.name,
        }
    }
}

/// An expression with what it sees.
#[derive(Debug)]
pub(crate) struct Expression {
    pub(crate) source: String,
    pub(crate) parsed: Parsed,
    pub(crate) scope: Scope,
}

/// The least presence a key may have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Presence {
    None,
    UserVerified,
}

impl Presence {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Presence::None => "none",
            Presence::UserVerified => "user_verified",
        }
    }

    /// A key's presence; an unknown one is below every requirement.
    pub(crate) fn of_key(text: &str) -> Option<Presence> {
        match text {
            "none" | "" => Some(Presence::None),
            "user_verified" => Some(Presence::UserVerified),
            _ => None,
        }
    }
}

/// An `[attestors.<name>]` set.
#[derive(Debug)]
pub(crate) struct Attestor {
    pub(crate) members: Members,
    pub(crate) identity_kinds: Vec<String>,
    pub(crate) actors: Vec<String>,
    pub(crate) authorities: Vec<String>,
    pub(crate) issuer_kinds: Vec<String>,
    pub(crate) presence: Presence,
    pub(crate) allow_self: bool,
    pub(crate) when: Option<Expression>,
}

/// Identities, directly and through groups, resolved at load.
#[derive(Debug, Default)]
pub(crate) struct Members {
    pub(crate) groups: Vec<String>,
    pub(crate) identities: Vec<String>,
    pub(crate) ids: BTreeSet<String>,
    pub(crate) names: BTreeSet<String>,
}

impl Members {
    /// Whether a key's identity, by id or by name, is a member.
    pub(crate) fn contains(&self, id: &str, name: &str) -> bool {
        (!id.is_empty() && self.ids.contains(id)) || (!name.is_empty() && self.names.contains(name))
    }
}

/// A `[[require]]` entry.
#[derive(Debug)]
pub(crate) struct Require {
    pub(crate) name: String,
    pub(crate) filter: PathFilter,
    pub(crate) kinds: Vec<String>,
    pub(crate) targets: Vec<String>,
    pub(crate) attestors: Vec<String>,
    pub(crate) when: Option<Expression>,
    pub(crate) message: String,
}

/// `[limits]`.
#[derive(Debug)]
pub(crate) struct Limits {
    pub(crate) max_files: Option<i64>,
    pub(crate) max_lines: Option<i64>,
    pub(crate) max_file_size: Option<i64>,
    pub(crate) except: Patterns,
}

/// A `[[forbid]]` entry.
#[derive(Debug)]
pub(crate) struct Forbid {
    pub(crate) name: String,
    pub(crate) filter: PathFilter,
    pub(crate) when: Option<Expression>,
    pub(crate) message: String,
}

/// A `[[signoff]]` entry, or `[owners]` as `builtin:owners`.
#[derive(Debug)]
pub(crate) struct Signoff {
    pub(crate) name: String,
    pub(crate) filter: PathFilter,
    pub(crate) members: Members,
    pub(crate) count: i64,
    pub(crate) presence: Presence,
    pub(crate) kinds: Vec<String>,
    pub(crate) when: Option<Expression>,
    pub(crate) message: String,
}

/// A `[[rule]]` entry.
#[derive(Debug)]
pub(crate) struct Rule {
    pub(crate) name: String,
    pub(crate) when: Option<Expression>,
    pub(crate) expr: Expression,
    pub(crate) message: String,
}

/// A conflict resolution tier (R3.28 applies it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "policy.Tier")]
pub enum Tier {
    /// Resolve and land.
    Land,
    /// Resolve and require approval.
    Approve,
    /// Ask before attempting.
    Ask,
}

impl Tier {
    fn parse(text: &str) -> Option<Tier> {
        match text {
            "land" => Some(Tier::Land),
            "approve" => Some(Tier::Approve),
            "ask" => Some(Tier::Ask),
            _ => None,
        }
    }
}

/// `[conflicts]`.
#[derive(Debug)]
pub(crate) struct Conflicts {
    pub(crate) tier: Tier,
    pub(crate) attempts: i64,
    pub(crate) rules: Vec<ConflictRule>,
}

/// A `[[conflicts.rule]]` entry.
#[derive(Debug)]
pub(crate) struct ConflictRule {
    pub(crate) filter: PathFilter,
    pub(crate) tier: Option<Tier>,
    pub(crate) attempts: Option<i64>,
    pub(crate) when: Option<Expression>,
}

// ---------------------------------------------------------------------------
// Loading.

/// Positions in the file being loaded.
struct Source<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str) -> Source<'a> {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        Source { text, line_starts }
    }

    /// The 1-based line and character column of byte `offset`.
    fn position(&self, offset: usize) -> (u32, u32) {
        let offset = offset.min(self.text.len());
        let line = self.line_starts.partition_point(|start| *start <= offset);
        let start = self.line_starts[line - 1];
        let column = self
            .text
            .get(start..offset)
            .map_or(0, |prefix| prefix.chars().count());
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(column + 1).unwrap_or(u32::MAX),
        )
    }

    /// Where byte `offset` of a string value's contents is in the file,
    /// given the value's span (its quotes included).
    fn in_string(&self, span: &Range<usize>, offset: usize) -> usize {
        let Some(raw) = self.text.get(span.clone()) else {
            return span.start;
        };
        let (open, literal) = if raw.starts_with("'''") {
            (3, true)
        } else if raw.starts_with("\"\"\"") {
            (3, false)
        } else if raw.starts_with('\'') {
            (1, true)
        } else if raw.starts_with('"') {
            (1, false)
        } else {
            return span.start;
        };
        let mut start = open;
        // A newline right after a multi-line string's opening is trimmed.
        if open == 3 {
            if raw[start..].starts_with("\r\n") {
                start += 2;
            } else if raw[start..].starts_with('\n') {
                start += 1;
            }
        }
        if literal {
            return span.start + start + offset;
        }
        // A basic string: map the decoded offset through its escapes.
        let body = &raw[start..];
        let mut decoded = 0usize;
        let mut chars = body.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            if decoded >= offset {
                return span.start + start + at;
            }
            if c != '\\' {
                decoded += c.len_utf8();
                continue;
            }
            match chars.next() {
                Some((_, 'u')) => {
                    let hex: String = (0..4)
                        .filter_map(|_| chars.next().map(|(_, c)| c))
                        .collect();
                    decoded += u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map_or(1, char::len_utf8);
                }
                Some((_, 'U')) => {
                    let hex: String = (0..8)
                        .filter_map(|_| chars.next().map(|(_, c)| c))
                        .collect();
                    decoded += u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map_or(1, char::len_utf8);
                }
                Some((_, c)) if c.is_whitespace() => {
                    // A line-ending backslash: the whitespace that follows is
                    // dropped.
                    while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
                        chars.next();
                    }
                }
                Some(_) => decoded += 1,
                None => {}
            }
        }
        span.start + start + body.len().min(offset)
    }
}

struct Loader<'a> {
    source: Source<'a>,
    diagnostics: Vec<Diagnostic>,
    entries: usize,
}

impl Loader<'_> {
    fn at(&mut self, rule: &str, field: &str, offset: usize, message: impl Into<String>) {
        let (line, column) = self.source.position(offset);
        self.diagnostics.push(Diagnostic {
            rule: rule.to_owned(),
            field: field.to_owned(),
            line,
            column,
            message: message.into(),
        });
    }

    fn list<'b>(&mut self, rule: &str, field: &str, list: &'b [Text]) -> Vec<&'b str> {
        if list.len() > MAX_LIST {
            let at = list.get(MAX_LIST).map_or(0, |item| item.span().start);
            self.at(
                rule,
                field,
                at,
                format!("{} entries; a list has at most {MAX_LIST}", list.len()),
            );
        }
        list.iter().map(|item| item.get_ref().as_str()).collect()
    }

    fn patterns(&mut self, rule: &str, field: &str, list: &[Text]) -> Patterns {
        self.list(rule, field, list);
        let mut ok = Vec::new();
        for item in list {
            match crate::glob::compile(item.get_ref()) {
                Ok(_) => ok.push(item.get_ref().as_str()),
                Err(error) => {
                    self.at(
                        rule,
                        field,
                        item.span().start,
                        format!("pattern {:?}: {error}", item.get_ref()),
                    );
                }
            }
        }
        Patterns::new(ok).unwrap_or_else(|_| Patterns::empty())
    }

    fn filter(&mut self, rule: &str, paths: &[Text], except: &[Text]) -> PathFilter {
        PathFilter {
            paths: self.patterns(rule, "paths", paths),
            except: self.patterns(rule, "except", except),
        }
    }

    fn name(&mut self, name: &Text, first: &BTreeMap<String, usize>) -> String {
        let text = name.get_ref();
        if !is_rule_name(text) {
            self.at(
                text,
                "name",
                name.span().start,
                format!("{text:?} is not a rule name: [a-z0-9][a-z0-9_-]{{0,63}}"),
            );
        } else if text == "limits" {
            self.at(
                text,
                "name",
                name.span().start,
                "`limits` is the name of the [limits] table's rule",
            );
        } else if first.get(text).is_some_and(|at| *at != name.span().start) {
            self.at(
                text,
                "name",
                name.span().start,
                format!("a second rule named {text:?}; names are unique in the file"),
            );
        }
        text.clone()
    }

    fn entry(&mut self, rule: &str, at: usize) {
        self.entries += 1;
        if self.entries == MAX_ENTRIES + 1 {
            self.at(
                rule,
                "",
                at,
                format!("more than {MAX_ENTRIES} entries across [[require]], [[forbid]], [[signoff]], [[conflicts.rule]] and [[rule]]"),
            );
        }
    }

    fn expression(
        &mut self,
        rule: &str,
        field: &str,
        text: &Text,
        scope: Scope,
    ) -> Option<Expression> {
        let source = text.get_ref();
        let span = text.span();
        if source.len() > MAX_EXPRESSION_BYTES {
            self.at(
                rule,
                field,
                span.start,
                format!(
                    "{} bytes; an expression is at most {MAX_EXPRESSION_BYTES}",
                    source.len()
                ),
            );
            return None;
        }
        let parsed = match env::parse(source) {
            Ok(parsed) => parsed,
            Err(errors) => {
                for (offset, message) in errors {
                    let at = self.source.in_string(&span, offset);
                    self.at(rule, field, at, message);
                }
                return None;
            }
        };
        let mut parsed = parsed;
        let checked = check::check(&parsed.ast, scope);
        if checked.problems.is_empty() {
            env::rewrite_matches(&mut parsed.ast);
            Some(Expression {
                source: source.clone(),
                parsed,
                scope,
            })
        } else {
            for problem in checked.problems {
                let at = self.source.in_string(&span, parsed.offset(problem.node));
                self.at(rule, field, at, problem.message);
            }
            None
        }
    }

    /// An optional `when`; an empty one is none.
    fn when(
        &mut self,
        rule: &str,
        when: Option<&Text>,
        scope: Scope,
    ) -> (Option<Expression>, bool) {
        match when {
            Some(text) if !text.get_ref().trim().is_empty() => {
                let expression = self.expression(rule, "when", text, scope);
                let failed = expression.is_none();
                (expression, failed)
            }
            _ => (None, false),
        }
    }

    fn presence(&mut self, rule: &str, presence: Option<&Text>) -> Presence {
        match presence.map(|p| (p.get_ref().as_str(), p.span().start)) {
            None | Some(("none", _)) => Presence::None,
            Some(("user_verified", _)) => Presence::UserVerified,
            Some((other, at)) => {
                self.at(
                    rule,
                    "presence",
                    at,
                    format!("{other:?} is not a presence: none or user_verified"),
                );
                Presence::None
            }
        }
    }

    fn count(&mut self, rule: &str, count: Option<&Spanned<i64>>) -> i64 {
        match count {
            None => 1,
            Some(count) if (1..=MAX_COUNT).contains(count.get_ref()) => *count.get_ref(),
            Some(count) => {
                self.at(
                    rule,
                    "count",
                    count.span().start,
                    format!("{} is not a count from 1 to {MAX_COUNT}", count.get_ref()),
                );
                1
            }
        }
    }

    fn members(
        &mut self,
        rule: &str,
        groups: &[Text],
        identities: &[Text],
        defined: &BTreeMap<String, Vec<String>>,
        at: usize,
    ) -> Members {
        self.list(rule, "groups", groups);
        self.list(rule, "identities", identities);
        let mut members = Members::default();
        if groups.is_empty() && identities.is_empty() {
            self.at(
                rule,
                "",
                at,
                "names no identities and no groups; at least one is needed",
            );
        }
        for group in groups {
            match defined.get(group.get_ref()) {
                Some(entries) => {
                    members.groups.push(group.get_ref().clone());
                    for entry in entries {
                        add_member(&mut members, entry);
                    }
                }
                None => self.at(
                    rule,
                    "groups",
                    group.span().start,
                    format!("no group named {:?} in [groups]", group.get_ref()),
                ),
            }
        }
        for identity in identities {
            if is_identity_id(identity.get_ref()) || is_identity_name(identity.get_ref()) {
                members.identities.push(identity.get_ref().clone());
                add_member(&mut members, identity.get_ref());
            } else {
                self.at(
                    rule,
                    "identities",
                    identity.span().start,
                    format!("{:?} is neither an identity name nor an identity id (idn_ and 16 hex digits)", identity.get_ref()),
                );
            }
        }
        members
    }

    fn vocabulary(
        &mut self,
        rule: &str,
        field: &str,
        list: &[Text],
        allowed: &[&str],
    ) -> Vec<String> {
        let mut out = Vec::new();
        for item in list {
            if allowed.contains(&item.get_ref().as_str()) {
                out.push(item.get_ref().clone());
            } else {
                self.at(
                    rule,
                    field,
                    item.span().start,
                    format!("{:?} is not one of {}", item.get_ref(), allowed.join(", ")),
                );
            }
        }
        out
    }

    fn kinds(&mut self, rule: &str, field: &str, list: &[Text], at: usize) -> Vec<String> {
        self.list(rule, field, list);
        if list.is_empty() {
            self.at(rule, field, at, "is empty; name at least one evidence kind");
        }
        let mut out = Vec::new();
        for kind in list {
            if is_kind(kind.get_ref()) {
                out.push(kind.get_ref().clone());
            } else {
                self.at(
                    rule,
                    field,
                    kind.span().start,
                    format!(
                        "{:?} is not an evidence kind: [a-z][a-z0-9_.-]{{0,63}}",
                        kind.get_ref()
                    ),
                );
            }
        }
        out
    }
}

fn add_member(members: &mut Members, entry: &str) {
    if is_identity_id(entry) {
        members.ids.insert(entry.to_owned());
    } else {
        members.names.insert(entry.to_owned());
    }
}

/// `[a-z0-9][a-z0-9_-]{0,63}`: a rule, group or attestor set name.
pub(crate) fn is_rule_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// An identity name as R3.03 defines one: `[a-z0-9][a-z0-9._-]{0,62}`, not
/// starting with `idn_` or `ses_`.
fn is_identity_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= 63
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_".contains(&b))
        && !name.starts_with("idn_")
        && !name.starts_with("ses_")
}

/// `idn_` and 16 lowercase hex digits.
fn is_identity_id(text: &str) -> bool {
    text.strip_prefix("idn_").is_some_and(|rest| {
        rest.len() == 16
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// An evidence kind, as R3.01's vocabulary allows: `[a-z][a-z0-9_.-]{0,63}`.
fn is_kind(kind: &str) -> bool {
    let mut bytes = kind.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && kind.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

/// `"5MiB"` and the like, or a bare integer of bytes.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let shift = match unit.trim() {
        "" | "B" => 0,
        "KiB" => 10,
        "MiB" => 20,
        "GiB" => 30,
        "TiB" => 40,
        _ => return None,
    };
    number.checked_mul(1u64 << shift)
}

impl Policy {
    /// Loads a policy file's bytes, or every problem it has. It runs on a
    /// thread of its own ([`crate::env::STACK_BYTES`]).
    pub fn load(bytes: &[u8]) -> Result<Policy, Vec<Diagnostic>> {
        env::on_policy_stack(|| Policy::load_here(bytes)).unwrap_or_else(|| {
            Err(vec![Diagnostic {
                message: "could not start a thread to load the policy".to_owned(),
                ..Diagnostic::default()
            }])
        })
    }

    fn load_here(bytes: &[u8]) -> Result<Policy, Vec<Diagnostic>> {
        let file = |message: String| {
            vec![Diagnostic {
                message,
                ..Diagnostic::default()
            }]
        };
        if bytes.len() > MAX_FILE_BYTES {
            return Err(file(format!(
                "the file is {} bytes; a policy is at most {MAX_FILE_BYTES}",
                bytes.len()
            )));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|error| file(format!("the file is not UTF-8: {error}")))?;
        let source = Source::new(text);
        let toml_error = |error: toml::de::Error| {
            let (line, column) = error
                .span()
                .map_or((0, 0), |span| source.position(span.start));
            vec![Diagnostic {
                line,
                column,
                message: error.message().trim().to_owned(),
                ..Diagnostic::default()
            }]
        };
        // The version first, so a file for a later format is reported as
        // that, not as its unknown tables.
        let version: VersionOnly = toml::from_str(text).map_err(toml_error)?;
        match version.version {
            None => {
                return Err(file(format!(
                    "no `version`; a policy starts with `version = {VERSION}`"
                )));
            }
            Some(found) if found.get_ref().as_integer() != Some(i64::from(VERSION)) => {
                let (line, column) = source.position(found.span().start);
                return Err(vec![Diagnostic {
                    field: "version".to_owned(),
                    line,
                    column,
                    message: format!(
                        "this Reef reads policy version {VERSION}; the file is version {}",
                        found.get_ref()
                    ),
                    ..Diagnostic::default()
                }]);
            }
            Some(_) => {}
        }
        let raw: RawFile = toml::from_str(text).map_err(toml_error)?;
        let mut loader = Loader {
            source,
            diagnostics: Vec::new(),
            entries: 0,
        };
        let policy = loader.load(&raw, text);
        if loader.diagnostics.is_empty() {
            Ok(policy)
        } else {
            loader.diagnostics.sort_by_key(|a| (a.line, a.column));
            Err(loader.diagnostics)
        }
    }
}

impl Loader<'_> {
    #[allow(clippy::too_many_lines)]
    fn load(&mut self, raw: &RawFile, text: &str) -> Policy {
        // Groups.
        if raw.groups.len() > MAX_GROUPS {
            self.at(
                "groups",
                "",
                0,
                format!("{} groups; at most {MAX_GROUPS}", raw.groups.len()),
            );
        }
        let mut groups = BTreeMap::new();
        for (name, members) in &raw.groups {
            let rule = format!("groups.{name}");
            let at = members.span().start;
            if !is_rule_name(name) {
                self.at(
                    &rule,
                    "",
                    at,
                    format!("{name:?} is not a group name: [a-z0-9][a-z0-9_-]{{0,63}}"),
                );
            }
            if members.get_ref().len() > MAX_GROUP_MEMBERS {
                self.at(
                    &rule,
                    "",
                    at,
                    format!(
                        "{} members; a group has at most {MAX_GROUP_MEMBERS}",
                        members.get_ref().len()
                    ),
                );
            }
            let mut list = Vec::new();
            for member in members.get_ref() {
                if is_identity_id(member.get_ref()) || is_identity_name(member.get_ref()) {
                    list.push(member.get_ref().clone());
                } else {
                    self.at(
                        &rule,
                        "",
                        member.span().start,
                        format!("{:?} is neither an identity name nor an identity id (idn_ and 16 hex digits)", member.get_ref()),
                    );
                }
            }
            groups.insert(name.clone(), list);
        }

        // Owners.
        let owners = if let Some(owners) = &raw.owners {
            let at = owners.span().start;
            let owners = owners.get_ref();
            Signoff {
                name: "builtin:owners".to_owned(),
                filter: PathFilter {
                    paths: Patterns::new(POLICY_FILES).unwrap_or_else(|_| Patterns::empty()),
                    except: Patterns::empty(),
                },
                members: self.members("owners", &owners.groups, &owners.identities, &groups, at),
                count: self.count("owners", owners.count.as_ref()),
                presence: self.presence("owners", owners.presence.as_ref()),
                kinds: vec!["signoff".to_owned()],
                when: None,
                message: String::new(),
            }
        } else {
            self.at("owners", "", 0, "no [owners]; a policy names who signs off a change to reef-policy.toml and reef-checks.toml");
            Signoff {
                name: "builtin:owners".to_owned(),
                filter: PathFilter {
                    paths: Patterns::empty(),
                    except: Patterns::empty(),
                },
                members: Members::default(),
                count: 1,
                presence: Presence::None,
                kinds: vec!["signoff".to_owned()],
                when: None,
                message: String::new(),
            }
        };

        // Attestor sets.
        if raw.attestors.len() > MAX_ATTESTOR_SETS {
            self.at(
                "attestors",
                "",
                0,
                format!(
                    "{} attestor sets; at most {MAX_ATTESTOR_SETS}",
                    raw.attestors.len()
                ),
            );
        }
        let mut attestors = BTreeMap::new();
        for (name, set) in &raw.attestors {
            let rule = format!("attestors.{name}");
            let at = set.span().start;
            let set = set.get_ref();
            if !is_rule_name(name) {
                self.at(
                    &rule,
                    "",
                    at,
                    format!("{name:?} is not an attestor set name: [a-z0-9][a-z0-9_-]{{0,63}}"),
                );
            }
            let members = self.members(&rule, &set.groups, &set.identities, &groups, at);
            self.list(&rule, "identity_kinds", &set.identity_kinds);
            let identity_kinds = self.vocabulary(
                &rule,
                "identity_kinds",
                &set.identity_kinds,
                &["person", "ci"],
            );
            let actors = match &set.actors {
                Some(actors) => {
                    if actors.get_ref().is_empty() {
                        self.at(
                            &rule,
                            "actors",
                            actors.span().start,
                            "is empty; leave it out for [\"executor\"]",
                        );
                    }
                    self.list(&rule, "actors", actors.get_ref());
                    self.vocabulary(
                        &rule,
                        "actors",
                        actors.get_ref(),
                        &["human", "agent", "executor"],
                    )
                }
                None => vec!["executor".to_owned()],
            };
            self.list(&rule, "authorities", &set.authorities);
            let mut authorities = Vec::new();
            for authority in &set.authorities {
                let text = authority.get_ref();
                if text.len() == 64
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    authorities.push(text.clone());
                } else {
                    self.at(
                        &rule,
                        "authorities",
                        authority.span().start,
                        format!("{text:?} is not an authority key id (64 lowercase hex digits)"),
                    );
                }
            }
            self.list(&rule, "issuer_kinds", &set.issuer_kinds);
            let issuer_kinds = self.vocabulary(
                &rule,
                "issuer_kinds",
                &set.issuer_kinds,
                &["reef-daemon", "sigstore"],
            );
            let presence = self.presence(&rule, set.presence.as_ref());
            let allow_self = match set
                .self_
                .as_ref()
                .map(|s| (s.get_ref().as_str(), s.span().start))
            {
                None | Some(("exclude", _)) => false,
                Some(("allow", _)) => true,
                Some((other, at)) => {
                    self.at(
                        &rule,
                        "self",
                        at,
                        format!("{other:?} is not exclude or allow"),
                    );
                    false
                }
            };
            let (when, _) = self.when(&rule, set.when.as_ref(), Scope::Evidence);
            attestors.insert(
                name.clone(),
                Attestor {
                    members,
                    identity_kinds,
                    actors,
                    authorities,
                    issuer_kinds,
                    presence,
                    allow_self,
                    when,
                },
            );
        }

        // Rules, in file order; a name's first use, in file order, is the
        // one a duplicate is reported against.
        let mut names: Vec<(usize, &String)> = raw
            .require
            .iter()
            .map(|r| &r.get_ref().name)
            .chain(raw.forbid.iter().map(|f| &f.get_ref().name))
            .chain(raw.signoff.iter().map(|s| &s.get_ref().name))
            .chain(raw.rule.iter().map(|r| &r.get_ref().name))
            .map(|name| (name.span().start, name.get_ref()))
            .collect();
        names.sort();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for (at, name) in names {
            seen.entry(name.clone()).or_insert(at);
        }
        let mut entries: Vec<(usize, Entry)> = Vec::new();
        for require in &raw.require {
            let at = require.span().start;
            let require = require.get_ref();
            let name = self.name(&require.name, &seen);
            self.entry(&name, at);
            let filter = self.filter(&name, &require.paths, &require.except);
            let kinds = self.kinds(
                &name,
                "kinds",
                require.kinds.get_ref(),
                require.kinds.span().start,
            );
            self.list(&name, "targets", &require.targets);
            let targets: Vec<String> = require
                .targets
                .iter()
                .map(|t| t.get_ref().clone())
                .collect();
            for target in &require.targets {
                if target.get_ref().is_empty() {
                    self.at(
                        &name,
                        "targets",
                        target.span().start,
                        "an empty target; leave `targets` out for the whole check",
                    );
                }
            }
            self.list(&name, "attestors", require.attestors.get_ref());
            if require.attestors.get_ref().is_empty() {
                self.at(
                    &name,
                    "attestors",
                    require.attestors.span().start,
                    "is empty; name at least one [attestors.<name>] set",
                );
            }
            let mut sets = Vec::new();
            for set in require.attestors.get_ref() {
                if attestors.contains_key(set.get_ref()) {
                    sets.push(set.get_ref().clone());
                } else {
                    self.at(
                        &name,
                        "attestors",
                        set.span().start,
                        format!("no attestor set named {:?}", set.get_ref()),
                    );
                }
            }
            let (when, _) = self.when(&name, require.when.as_ref(), Scope::Change);
            entries.push((
                at,
                Entry::Require(Require {
                    name,
                    filter,
                    kinds,
                    targets,
                    attestors: sets,
                    when,
                    message: require.message.clone().unwrap_or_default(),
                }),
            ));
        }
        if let Some(limits) = &raw.limits {
            let at = limits.span().start;
            let limits = limits.get_ref();
            let positive = |loader: &mut Self, field: &str, value: Option<&Spanned<i64>>| {
                value.and_then(|value| {
                    if *value.get_ref() >= 0 {
                        Some(*value.get_ref())
                    } else {
                        loader.at("limits", field, value.span().start, "is negative");
                        None
                    }
                })
            };
            let max_files = positive(self, "max_files", limits.max_files.as_ref());
            let max_lines = positive(self, "max_lines", limits.max_lines.as_ref());
            let max_file_size = limits.max_file_size.as_ref().and_then(|size| {
                let parsed = match size.get_ref() {
                    toml::Value::Integer(bytes) => u64::try_from(*bytes).ok(),
                    toml::Value::String(text) => parse_size(text),
                    _ => None,
                };
                if let Some(bytes) = parsed.and_then(|bytes| i64::try_from(bytes).ok()) {
                    Some(bytes)
                } else {
                    self.at(
                        "limits",
                        "max_file_size",
                        size.span().start,
                        "is not a size such as \"5MiB\", \"512KiB\" or a number of bytes",
                    );
                    None
                }
            });
            let except = self.patterns("limits", "except", &limits.except);
            entries.push((
                at,
                Entry::Limits(Limits {
                    max_files,
                    max_lines,
                    max_file_size,
                    except,
                }),
            ));
        }
        for forbid in &raw.forbid {
            let at = forbid.span().start;
            let forbid = forbid.get_ref();
            let name = self.name(&forbid.name, &seen);
            self.entry(&name, at);
            if forbid.paths.get_ref().is_empty() {
                self.at(
                    &name,
                    "paths",
                    forbid.paths.span().start,
                    "is empty; name the paths no change may touch",
                );
            }
            let filter = self.filter(&name, forbid.paths.get_ref(), &forbid.except);
            let (when, _) = self.when(&name, forbid.when.as_ref(), Scope::Change);
            entries.push((
                at,
                Entry::Forbid(Forbid {
                    name,
                    filter,
                    when,
                    message: forbid.message.clone().unwrap_or_default(),
                }),
            ));
        }
        for signoff in &raw.signoff {
            let at = signoff.span().start;
            let signoff = signoff.get_ref();
            let name = self.name(&signoff.name, &seen);
            self.entry(&name, at);
            let filter = self.filter(&name, &signoff.paths, &signoff.except);
            let members = self.members(&name, &signoff.groups, &signoff.identities, &groups, at);
            let count = self.count(&name, signoff.count.as_ref());
            let presence = self.presence(&name, signoff.presence.as_ref());
            let kinds = match &signoff.kinds {
                Some(kinds) => self.kinds(&name, "kinds", kinds.get_ref(), kinds.span().start),
                None => vec!["signoff".to_owned()],
            };
            let (when, _) = self.when(&name, signoff.when.as_ref(), Scope::Change);
            entries.push((
                at,
                Entry::Signoff(Signoff {
                    name,
                    filter,
                    members,
                    count,
                    presence,
                    kinds,
                    when,
                    message: signoff.message.clone().unwrap_or_default(),
                }),
            ));
        }
        for rule in &raw.rule {
            let at = rule.span().start;
            let rule = rule.get_ref();
            let name = self.name(&rule.name, &seen);
            self.entry(&name, at);
            let (when, _) = self.when(&name, rule.when.as_ref(), Scope::Change);
            let expr = if rule.expr.get_ref().trim().is_empty() {
                self.at(&name, "expr", rule.expr.span().start, "is empty");
                None
            } else {
                self.expression(&name, "expr", &rule.expr, Scope::Change)
            };
            if let Some(expr) = expr {
                entries.push((
                    at,
                    Entry::Rule(Rule {
                        name,
                        when,
                        expr,
                        message: rule.message.clone().unwrap_or_default(),
                    }),
                ));
            }
        }
        entries.sort_by_key(|(at, _)| *at);

        // Conflicts.
        let conflicts = match &raw.conflicts {
            None => Conflicts {
                tier: Tier::Approve,
                attempts: 3,
                rules: Vec::new(),
            },
            Some(conflicts) => {
                let conflicts = conflicts.get_ref();
                let tier = self.tier(conflicts.tier.as_ref()).unwrap_or(Tier::Approve);
                let attempts = self.attempts(conflicts.attempts.as_ref()).unwrap_or(3);
                let mut rules = Vec::new();
                for (index, rule) in conflicts.rule.iter().enumerate() {
                    let at = rule.span().start;
                    let rule = rule.get_ref();
                    let label = format!("conflicts.rule[{index}]");
                    self.entry(&label, at);
                    let filter = self.filter(&label, &rule.paths, &rule.except);
                    let tier = self.tier(rule.tier.as_ref());
                    let attempts = self.attempts(rule.attempts.as_ref());
                    let (when, _) = self.when(&label, rule.when.as_ref(), Scope::Conflict);
                    rules.push(ConflictRule {
                        filter,
                        tier,
                        attempts,
                        when,
                    });
                }
                Conflicts {
                    tier,
                    attempts,
                    rules,
                }
            }
        };

        let landing = self.landing(raw.landing.as_ref());
        let history = self.history(raw.history.as_ref());

        Policy {
            sha256: crate::sha256_hex(text.as_bytes()),
            owners,
            groups,
            attestors,
            entries: entries.into_iter().map(|(_, entry)| entry).collect(),
            conflicts,
            landing,
            history,
        }
    }

    fn history(&mut self, raw: Option<&Spanned<RawHistory>>) -> HistorySettings {
        let Some(raw) = raw else {
            return HistorySettings::default();
        };
        let raw = raw.get_ref();
        let mut settings = HistorySettings {
            message: Message::Summary,
            trailers: Vec::new(),
        };
        if let Some(message) = &raw.message {
            match Message::parse(message.get_ref()) {
                Some(parsed) => settings.message = parsed,
                None => self.at(
                    "history",
                    "message",
                    message.span().start,
                    format!("{:?} is not summary or keep", message.get_ref()),
                ),
            }
        }
        match &raw.trailers {
            None if settings.summarized() => {
                settings.trailers = HistorySettings::DEFAULT_TRAILERS.to_vec();
            }
            None => {}
            Some(trailers) if !settings.summarized() => self.at(
                "history",
                "trailers",
                trailers.span().start,
                "trailers are written only with message = \"summary\"".to_owned(),
            ),
            Some(trailers) => {
                for name in trailers.get_ref() {
                    match TrailerSet::parse(name.get_ref()) {
                        Some(set) if settings.trailers.contains(&set) => self.at(
                            "history",
                            "trailers",
                            name.span().start,
                            format!("{:?} is named twice", name.get_ref()),
                        ),
                        Some(set) => settings.trailers.push(set),
                        None => self.at(
                            "history",
                            "trailers",
                            name.span().start,
                            format!(
                                "{:?} is not a trailer set: change, landing, agents or signoffs",
                                name.get_ref()
                            ),
                        ),
                    }
                }
                settings.trailers.sort();
            }
        }
        settings
    }

    fn landing(&mut self, raw: Option<&Spanned<RawLanding>>) -> LandingSettings {
        let mut settings = LandingSettings::default();
        let Some(raw) = raw else {
            return settings;
        };
        let raw = raw.get_ref();
        if let Some(checks) = &raw.checks {
            match Checks::parse(checks.get_ref()) {
                Some(parsed) => settings.checks = parsed,
                None => self.at(
                    "landing",
                    "checks",
                    checks.span().start,
                    format!("{:?} is not batch or proposal", checks.get_ref()),
                ),
            }
        }
        if let Some(max) = &raw.batch_max {
            match u32::try_from(*max.get_ref()) {
                Ok(value) if BATCH_MAX.contains(max.get_ref()) => settings.batch_max = value,
                _ => self.at(
                    "landing",
                    "batch_max",
                    max.span().start,
                    format!(
                        "{} is not from {} to {}",
                        max.get_ref(),
                        BATCH_MAX.start(),
                        BATCH_MAX.end()
                    ),
                ),
            }
        }
        if let Some(wait) = &raw.batch_wait {
            match parse_duration(wait.get_ref()) {
                Some(value) if value <= MAX_BATCH_WAIT => settings.batch_wait = value,
                _ => self.at(
                    "landing",
                    "batch_wait",
                    wait.span().start,
                    format!(
                        "{:?} is not a duration from \"0s\" to \"10m\" (such as \"30s\")",
                        wait.get_ref()
                    ),
                ),
            }
        }
        settings.alone = self.patterns("landing", "alone", &raw.alone);
        settings
    }

    fn tier(&mut self, tier: Option<&Text>) -> Option<Tier> {
        let tier = tier?;
        let parsed = Tier::parse(tier.get_ref());
        if parsed.is_none() {
            self.at(
                "conflicts",
                "tier",
                tier.span().start,
                format!("{:?} is not a tier: land, approve or ask", tier.get_ref()),
            );
        }
        parsed
    }

    fn attempts(&mut self, attempts: Option<&Spanned<i64>>) -> Option<i64> {
        let attempts = attempts?;
        if ATTEMPTS.contains(attempts.get_ref()) {
            Some(*attempts.get_ref())
        } else {
            self.at(
                "conflicts",
                "attempts",
                attempts.span().start,
                format!(
                    "{} is not from {} to {}",
                    attempts.get_ref(),
                    ATTEMPTS.start(),
                    ATTEMPTS.end()
                ),
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
version = 1

[owners]
groups = ["maintainers"]
identities = []
count = 1
presence = "none"

[groups]
maintainers = ["alice", "idn_8f3a0c2d91e4b675"]

[attestors.ci]
identities = ["ci-main"]
groups = []
identity_kinds = ["ci"]
actors = ["executor"]
authorities = []
presence = "none"
self = "exclude"
when = 'e.fresh && e.artifacts.exists(a, a.state == "stored")'

[[require]]
name = "tests"
paths = ["crates/**"]
except = ["crates/**/*.md"]
kinds = ["test", "lint"]
targets = []
attestors = ["ci"]
when = 'change.lines_added > 0'
message = "crate changes need CI's tests and lint"

[limits]
max_files = 400
max_lines = 4000
max_file_size = "5MiB"
except = ["Cargo.lock", "web/package-lock.json"]

[[forbid]]
name = "secrets"
paths = ["**/*.pem", "secrets/**"]
except = []
when = ''
message = "no keys in the tree"

[[signoff]]
name = "ci-config"
paths = [".github/**", "scripts/release*"]
groups = ["maintainers"]
identities = []
count = 1
presence = "none"
kinds = ["signoff"]
when = ''

[conflicts]
tier = "approve"
attempts = 3

[[conflicts.rule]]
paths = ["docs/**"]
tier = "land"
attempts = 5
when = ''

[[rule]]
name = "provenance"
when = 'change.files.size() > 0'
expr = 'provenance.all(r, r.signature == "valid" && r.key.status == "bound")'
message = "every commit of the change has a signed, bound provenance record"
"#;

    fn errors(text: &str) -> Vec<Diagnostic> {
        Policy::load(text.as_bytes()).unwrap_err()
    }

    fn with(extra: &str) -> String {
        format!("version = 1\n[owners]\nidentities = [\"alice\"]\n{extra}")
    }

    fn one(text: &str, wanted: &str) -> Diagnostic {
        let found = errors(text);
        found
            .iter()
            .find(|d| d.message.contains(wanted))
            .cloned()
            .unwrap_or_else(|| panic!("no {wanted:?} in {found:#?}"))
    }

    #[test]
    fn a_file_with_every_key_loads() {
        let policy = Policy::load(FULL.as_bytes()).unwrap();
        let names: Vec<&str> = policy.entries.iter().map(Entry::name).collect();
        assert_eq!(
            names,
            ["tests", "limits", "secrets", "ci-config", "provenance"]
        );
        assert_eq!(policy.attestors.len(), 1);
        assert_eq!(policy.conflicts.rules.len(), 1);
        assert_eq!(policy.sha256.len(), 64);
        let Entry::Limits(limits) = &policy.entries[1] else {
            panic!()
        };
        assert_eq!(limits.max_file_size, Some(5 << 20));
        assert!(policy.owners.members.contains("idn_8f3a0c2d91e4b675", ""));
        assert!(policy.owners.members.contains("", "alice"));
    }

    #[test]
    fn file_level_problems_are_refused() {
        let unknown = one(&with("[nope]\n"), "unknown field");
        assert_eq!(unknown.line, 4);
        let version = one("version = 2\n[nope]\n", "reads policy version 1");
        assert_eq!((version.line, version.column), (1, 11));
        one("[owners]\nidentities = [\"a\"]\n", "no `version`");
        one("version = 1\n", "no [owners]");
        let big = format!("version = 1\n# {}\n", "x".repeat(MAX_FILE_BYTES));
        one(&big, "at most 262144");
        let key = one(
            &with("[[rule]]\nname = \"a\"\nexpr = \"true\"\nbogus = 1\n"),
            "unknown field",
        );
        assert_eq!(key.line, 7);
    }

    #[test]
    fn references_names_and_patterns_are_checked() {
        let dup = one(
            &with(
                "[[rule]]\nname = \"a\"\nexpr = \"true\"\n[[forbid]]\nname = \"a\"\npaths = [\"x\"]\n",
            ),
            "a second rule named",
        );
        assert_eq!(dup.line, 8);
        one(
            &with("[attestors.ci]\ngroups = [\"nobody\"]\n"),
            "no group named \"nobody\"",
        );
        one(
            &with("[[require]]\nname = \"t\"\nkinds = [\"test\"]\nattestors = [\"ci\"]\n"),
            "no attestor set named \"ci\"",
        );
        one(
            &with("[attestors.ci]\nidentities = [\"Bad Name\"]\n"),
            "neither an identity name",
        );
        let glob = one(
            &with("[[forbid]]\nname = \"f\"\npaths = [\"a/[\"]\n"),
            "pattern \"a/[\"",
        );
        assert_eq!((glob.line, glob.column), (6, 10));
        one(
            &with("[[forbid]]\nname = \"Bad\"\npaths = [\"a\"]\n"),
            "not a rule name",
        );
        one(&with("[conflicts]\nattempts = 21\n"), "is not from 1 to 20");
        one(
            &with("[conflicts]\n[[conflicts.rule]]\nattempts = 0\n"),
            "is not from 1 to 20",
        );
        one(
            &with("[attestors.ci]\nidentities = [\"a\"]\nactors = [\"robot\"]\n"),
            "not one of human, agent, executor",
        );
        one(
            &with("[attestors.ci]\nidentities = [\"a\"]\nauthorities = [\"abc\"]\n"),
            "not an authority key id",
        );
    }

    #[test]
    fn entry_limits_are_enforced() {
        use std::fmt::Write;
        let mut rules = String::new();
        for i in 0..257 {
            writeln!(rules, "[[rule]]\nname = \"r{i}\"\nexpr = \"true\"").unwrap();
        }
        one(&with(&rules), "more than 256 entries");
    }

    #[test]
    fn expressions_are_parsed_and_checked_with_positions() {
        let parse = one(
            &with("[[rule]]\nname = \"a\"\nexpr = 'change.empty &&'\n"),
            "",
        );
        assert_eq!(parse.rule, "a");
        assert_eq!(parse.field, "expr");
        assert_eq!(parse.line, 6);
        let unknown = one(
            &with("[[rule]]\nname = \"a\"\nexpr = 'chnage.paths.size() > 0'\n"),
            "unknown variable `chnage`",
        );
        assert_eq!((unknown.line, unknown.column), (6, 9));
        let field = one(
            &with("[[rule]]\nname = \"a\"\nexpr = \"change.empty || change.pahts == []\"\n"),
            "no field `pahts`",
        );
        // The select node's offset, through a basic string's quotes.
        assert_eq!(field.line, 6);
        assert!(field.column > 9, "{field:?}");
        let escaped = one(
            &with("[[rule]]\nname = \"a\"\nexpr = \"'\\u00e9' == 'x' && nope\"\n"),
            "unknown variable `nope`",
        );
        assert_eq!(escaped.column, 28, "{escaped:?}");
        one(
            &with("[[rule]]\nname = \"a\"\nexpr = 'e.fresh'\n"),
            "unknown variable `e`",
        );
        one(
            &with("[attestors.ci]\nidentities = [\"a\"]\nwhen = 'groups.all(g, true)'\n"),
            "over a map",
        );
        let deep = format!(
            "[[rule]]\nname = \"a\"\nexpr = '{}true{}'\n",
            "(".repeat(49),
            ")".repeat(49)
        );
        one(&with(&deep), "");
        let long = format!(
            "[[rule]]\nname = \"a\"\nexpr = '{}'\n",
            vec!["change.empty"; 700].join(" || ")
        );
        one(&with(&long), "bytes; an expression is at most");
        let nodes = format!(
            "[[rule]]\nname = \"a\"\nexpr = '{}'\n",
            vec!["true"; 600].join(" || ")
        );
        one(&with(&nodes), "nodes");
    }

    #[test]
    fn every_problem_is_reported() {
        let text = with(
            "[[rule]]\nname = \"a\"\nexpr = 'chnage.x'\n[[rule]]\nname = \"b\"\nexpr = 'nope()'\n[attestors.x]\ngroups = [\"g\"]\n",
        );
        let found = errors(&text);
        assert!(found.len() >= 3, "{found:#?}");
    }

    #[test]
    fn the_history_table() {
        let defaults = Policy::load(with("").as_bytes()).unwrap();
        assert_eq!(*defaults.history(), HistorySettings::default());
        assert!(!defaults.history().summarized());
        let bare = Policy::load(with("[history]\n").as_bytes()).unwrap();
        assert!(bare.history().summarized());
        assert_eq!(bare.history().trailers, HistorySettings::DEFAULT_TRAILERS);
        let set = Policy::load(
            with("[history]\nmessage = \"summary\"\ntrailers = [\"signoffs\", \"change\", \"agents\"]\n")
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            set.history().trailers,
            [TrailerSet::Change, TrailerSet::Agents, TrailerSet::Signoffs]
        );
        let none = Policy::load(with("[history]\ntrailers = []\n").as_bytes()).unwrap();
        assert!(none.history().summarized());
        assert_eq!(none.history().trailers, [] as [TrailerSet; 0]);
        let keep = Policy::load(with("[history]\nmessage = \"keep\"\n").as_bytes()).unwrap();
        assert_eq!(keep.history().message, Message::Keep);
        assert_eq!(keep.history().trailers, [] as [TrailerSet; 0]);
        one(
            &with("[history]\nmessage = \"squash\"\n"),
            "is not summary or keep",
        );
        one(
            &with("[history]\nmessage = \"keep\"\ntrailers = [\"change\"]\n"),
            "only with message",
        );
        one(
            &with("[history]\ntrailers = [\"change\", \"change\"]\n"),
            "named twice",
        );
        one(
            &with("[history]\ntrailers = [\"authors\"]\n"),
            "is not a trailer set",
        );
        one(&with("[history]\nsquash = \"stack\"\n"), "unknown field");
    }

    #[test]
    fn the_landing_table() {
        let defaults = Policy::load(with("").as_bytes()).unwrap();
        let landing = defaults.landing();
        assert_eq!(landing.checks, Checks::Batch);
        assert_eq!(landing.batch_max, DEFAULT_BATCH_MAX);
        assert_eq!(landing.batch_wait, std::time::Duration::ZERO);
        assert!(landing.alone(["reef-policy.toml"]));
        assert!(landing.alone(["Reef-Checks.toml"]));
        assert!(!landing.alone(["src/lib.rs"]));
        let set = Policy::load(
            with(
                "[landing]\nchecks = \"proposal\"\nbatch_max = 1\nbatch_wait = \"90s\"\nalone = [\"migrations/**\"]\n",
            )
            .as_bytes(),
        )
        .unwrap();
        let landing = set.landing();
        assert_eq!(landing.checks, Checks::Proposal);
        assert_eq!(landing.batch_max, 1);
        assert_eq!(landing.batch_wait, std::time::Duration::from_secs(90));
        assert!(landing.alone(["src/a.rs", "migrations/0001.sql"]));
        one(
            &with("[landing]\nchecks = \"never\"\n"),
            "is not batch or proposal",
        );
        one(&with("[landing]\nbatch_max = 33\n"), "is not from 1 to 32");
        one(&with("[landing]\nbatch_max = 0\n"), "is not from 1 to 32");
        one(
            &with("[landing]\nbatch_wait = \"11m\"\n"),
            "is not a duration",
        );
        one(
            &with("[landing]\nbatch_wait = \"soon\"\n"),
            "is not a duration",
        );
        one(&with("[landing]\nautomatic = true\n"), "unknown field");
        one(&with("[landing]\nalone = [\"a/[\"]\n"), "pattern \"a/[\"");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0s"), Some(std::time::Duration::ZERO));
        assert_eq!(
            parse_duration("250ms"),
            Some(std::time::Duration::from_millis(250))
        );
        assert_eq!(
            parse_duration("10m"),
            Some(std::time::Duration::from_mins(10))
        );
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration("5h"), None);
        assert_eq!(parse_duration("-1s"), None);
    }
}
