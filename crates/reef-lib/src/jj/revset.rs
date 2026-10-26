//! Revisions and revsets (R2.02, ADR 0030): jj's revset language, parsed
//! and resolved by jj-lib in jj's symbol order, with Reef's four aliases and
//! one addition, change id prefixes in hex.
//!
//! jj resolves a symbol as a tag, then a local bookmark, then a commit id
//! prefix in hex, then a change id prefix in reverse hex. Reef's responses
//! spell change ids in hex, so [`HexChangeIds`] resolves a hex symbol as a
//! change id prefix after all of those: hex text is a commit id prefix
//! first. `trunk()`, `immutable_heads()`, `immutable()` and `mutable()` are
//! defined per read from the view read; the user's jj configuration is not
//! read, so every daemon answers alike.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, CommitId};
use jj_lib::fileset::FilesetAliasesMap;
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::ref_name::{RefName, RemoteName, WorkspaceName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::{
    self as jj_revset, PartialSymbolResolver, Revset, RevsetAliasesMap, RevsetDiagnostics,
    RevsetEvaluationError, RevsetExtensions, RevsetParseContext, RevsetParseError,
    RevsetResolutionError, RevsetWorkspaceContext, SymbolResolver, SymbolResolverExtension,
    UserRevsetExpression,
};
use jj_lib::ui_path::RepoPathUiConverter;

use super::repo::{ChangeIdHex, CommitIdHex};
use super::workspace::Resolved;
use crate::error::{Classify, ErrorKind, classify_by_source};

/// Reef's default log set: every working copy, every visible mutable commit
/// (each mutable head and its ancestors back to `trunk()`), and `trunk()`.
pub const DEFAULT_LOG: &str = "trunk() | working_copies() | mutable()";

/// The remote whose default branch and remote-tracking bookmarks
/// [`trunk`] tries.
pub const DEFAULT_REMOTE: &str = "origin";

/// How many commit ids [`RevisionError::Ambiguous`] lists at most.
pub const MAX_LISTED: usize = 10;

/// The workspace names a revset is parsed with to learn whether it uses
/// `@`: a revset that names neither explicitly parses the same with both
/// unless it says `@`.
const PROBE_A: &str = "\u{0}reef-probe-a";
const PROBE_B: &str = "\u{0}reef-probe-b";

/// The repository's trunk: the first default-start bookmark (R1.31) that
/// exists in a view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trunk {
    /// The commit it points at.
    pub commit_id: CommitIdHex,
    /// The bookmark: `main`, or `main@origin` for a remote-tracking one.
    pub bookmark: String,
}

/// The bookmarks [`trunk`] tries, in order, as (name, remote): `main`; the
/// branch `origin/HEAD` names, local then remote-tracking, so a stale
/// local `master` beside a remote whose default is now `main` does not
/// win; `master` and `trunk`; then `main`, `master` and `trunk` on
/// `origin`.
pub fn default_candidates(origin_head: Option<&str>) -> Vec<(String, Option<&'static str>)> {
    let mut candidates: Vec<(String, Option<&'static str>)> = Vec::new();
    let mut push = |name: &str, remote| {
        // origin's HEAD is often `main` or `master`: each is tried once.
        if !candidates.iter().any(|(n, r)| n == name && *r == remote) {
            candidates.push((name.to_owned(), remote));
        }
    };
    push("main", None);
    if let Some(head) = origin_head {
        push(head, None);
        push(head, Some(DEFAULT_REMOTE));
    }
    for name in ["master", "trunk"] {
        push(name, None);
    }
    for name in ["main", "master", "trunk"] {
        push(name, Some(DEFAULT_REMOTE));
    }
    candidates
}

/// The first of [`default_candidates`] that exists in `repo`'s view, a
/// conflicted one skipped with a warning; `None` when there is none.
/// origin's HEAD is read from git's refs as they are now, whatever
/// operation `repo` is loaded at.
pub fn trunk(repo: &ReadonlyRepo) -> Option<Trunk> {
    let origin_head = super::git::remote_head(repo.store(), DEFAULT_REMOTE);
    let view = repo.view();
    for (name, remote) in default_candidates(origin_head.as_deref()) {
        let target = match remote {
            None => view.get_local_bookmark(RefName::new(&name)),
            Some(remote) => {
                &view
                    .get_remote_bookmark(
                        RefName::new(&name).to_remote_symbol(RemoteName::new(remote)),
                    )
                    .target
            }
        };
        if target.is_absent() {
            continue;
        }
        let bookmark = match remote {
            None => name,
            Some(remote) => format!("{name}@{remote}"),
        };
        if let Some(id) = target.as_normal() {
            return Some(Trunk {
                commit_id: CommitIdHex::of(id),
                bookmark,
            });
        }
        tracing::warn!("bookmark {bookmark} is conflicted; not using it as trunk()");
    }
    None
}

/// What a revset is parsed against besides the view.
#[derive(Clone, Copy, Debug)]
pub struct RevsetScope<'a> {
    /// The jj workspace `@` names (`reef-<id>`); `None` when the request
    /// names no workspace, which makes `@` [`RevisionError::WorkspaceRequired`].
    pub workspace: Option<&'a str>,
    /// The email `mine()` matches.
    pub user_email: &'a str,
    /// The registered root: `files()` patterns are relative to it.
    pub root: &'a Path,
}

/// A parsed revision or revset.
#[derive(Clone, Debug)]
pub struct ParsedRevset {
    text: String,
    expression: Arc<UserRevsetExpression>,
    uses_working_copy: bool,
}

impl ParsedRevset {
    /// The text as given.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the expression names the request workspace's working-copy
    /// commit, `@`: a read of it snapshots first.
    pub fn uses_working_copy(&self) -> bool {
        self.uses_working_copy
    }
}

/// Failure to parse, resolve, or evaluate a revision or revset.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RevisionError {
    /// A required revision is empty.
    #[error("a revision is required")]
    Required,
    /// The text uses `@` and the request names no workspace.
    #[error("{revision:?} uses `@`, which needs a workspace")]
    WorkspaceRequired {
        /// The text.
        revision: String,
    },
    /// The text does not parse. The message is jj's, with its position.
    #[error("{revision:?} is not a valid revset: {message}")]
    Parse {
        /// The text.
        revision: String,
        /// jj's message.
        message: String,
    },
    /// A symbol names nothing.
    #[error("{revision:?}: revision {name:?} does not exist")]
    NotFound {
        /// The text.
        revision: String,
        /// The symbol.
        name: String,
        /// Similar names jj suggests.
        candidates: Vec<String>,
    },
    /// The revision names no commit (an empty revset).
    #[error("{revision:?} names no commit")]
    Empty {
        /// The text.
        revision: String,
    },
    /// An id prefix shared by several commits or changes.
    #[error("{revision:?}: id prefix {prefix:?} is ambiguous")]
    AmbiguousPrefix {
        /// The text.
        revision: String,
        /// The prefix.
        prefix: String,
    },
    /// Several commits where one is required: a divergent change, a
    /// conflicted bookmark, or a revset of several.
    #[error("{revision:?} names {} commits where one is required", if *.more { format!("more than {}", commit_ids.len()) } else { commit_ids.len().to_string() })]
    Ambiguous {
        /// The text.
        revision: String,
        /// The commits, at most [`MAX_LISTED`].
        commit_ids: Vec<CommitIdHex>,
        /// More commits than `commit_ids` lists.
        more: bool,
    },
    /// The workspace `@` (or `<name>@`) names has no working-copy commit in
    /// the view read.
    #[error("workspace {workspace} has no working-copy commit at operation {at_operation}")]
    WorkingCopyNotFound {
        /// The jj workspace name.
        workspace: String,
        /// The operation read.
        at_operation: String,
    },
    /// A commit could not be read.
    #[error("failed to read a commit")]
    Backend(#[from] BackendError),
    /// Evaluating the revset failed.
    #[error("failed to evaluate the revset")]
    Evaluation(#[source] RevsetEvaluationError),
    /// Resolving a symbol failed for another reason (the index).
    #[error("failed to resolve the revset")]
    Other(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl Classify for RevisionError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Required
            | Self::WorkspaceRequired { .. }
            | Self::Parse { .. }
            | Self::AmbiguousPrefix { .. }
            | Self::Ambiguous { .. } => ErrorKind::InvalidArgument,
            Self::NotFound { .. } | Self::Empty { .. } | Self::WorkingCopyNotFound { .. } => {
                ErrorKind::NotFound
            }
            Self::Backend(error) => classify_by_source(error),
            Self::Evaluation(error) => classify_by_source(error),
            Self::Other(error) => classify_by_source(&**error),
        }
    }
}

/// Resolves change id prefixes written in hex, as Reef's responses spell
/// them, after jj's own resolvers. `<hex>/<n>` names the n-th commit of a
/// divergent change, as jj's reverse-hex form does.
#[derive(Debug)]
pub struct HexChangeIds;

impl SymbolResolverExtension for HexChangeIds {
    fn new_resolvers<'a>(
        &self,
        _context_repo: &'a dyn jj_lib::repo::Repo,
    ) -> Vec<Box<dyn PartialSymbolResolver + 'a>> {
        vec![Box::new(HexChangeResolver)]
    }
}

struct HexChangeResolver;

impl PartialSymbolResolver for HexChangeResolver {
    fn resolve_symbol(
        &self,
        repo: &dyn jj_lib::repo::Repo,
        symbol: &str,
    ) -> Result<Option<CommitId>, RevsetResolutionError> {
        let (text, offset) = match symbol.split_once('/') {
            Some((prefix, suffix)) => {
                let Ok(offset) = suffix.parse::<usize>() else {
                    return Ok(None);
                };
                (prefix, Some(offset))
            }
            None => (symbol, None),
        };
        let Some(prefix) = HexPrefix::try_from_hex(text) else {
            return Ok(None);
        };
        // jj's own resolvers block on the index the same way; this never
        // runs on an async executor's worker (R2.02).
        let targets = match pollster::block_on(repo.resolve_change_id_prefix(&prefix))
            .map_err(|error| RevsetResolutionError::Other(error.into()))?
        {
            PrefixResolution::NoMatch => return Ok(None),
            PrefixResolution::AmbiguousMatch => {
                return Err(RevsetResolutionError::AmbiguousChangeIdPrefix(
                    text.to_owned(),
                ));
            }
            PrefixResolution::SingleMatch(targets) => targets,
        };
        if let Some(offset) = offset {
            return Ok(targets.at_offset(offset).cloned());
        }
        let visible: Vec<(usize, CommitId)> = targets
            .visible_with_offsets()
            .map(|(i, id)| (i, id.clone()))
            .collect();
        match visible.as_slice() {
            [] => Ok(None),
            [(_, id)] => Ok(Some(id.clone())),
            _ => Err(RevsetResolutionError::DivergentChangeId {
                symbol: text.to_owned(),
                visible_targets: visible,
            }),
        }
    }
}

static EXTENSIONS: LazyLock<RevsetExtensions> = LazyLock::new(|| {
    let mut extensions = RevsetExtensions::new();
    extensions.add_symbol_resolver(Box::new(HexChangeIds));
    extensions
});

/// Reef's aliases over `repo`'s view.
fn aliases(repo: &ReadonlyRepo) -> RevsetAliasesMap {
    let trunk = trunk(repo).map_or_else(
        || "root()".to_owned(),
        |trunk| format!("commit_id(\"{}\")", trunk.commit_id),
    );
    let mut map = RevsetAliasesMap::new();
    for (decl, defn) in [
        ("trunk()", trunk.as_str()),
        ("immutable_heads()", "trunk() | tags()"),
        ("immutable()", "::(immutable_heads() | root())"),
        ("mutable()", "~immutable()"),
    ] {
        if let Err(error) = map.insert(decl, defn, None) {
            tracing::error!(error = %error, "Reef's alias {decl} does not parse");
        }
    }
    map
}

fn parse_as(
    text: &str,
    aliases: &RevsetAliasesMap,
    scope: &RevsetScope<'_>,
    workspace: &str,
) -> Result<Arc<UserRevsetExpression>, RevsetParseError> {
    let converter = RepoPathUiConverter::Fs {
        cwd: scope.root.to_owned(),
        base: scope.root.to_owned(),
    };
    let fileset_aliases = FilesetAliasesMap::new();
    let context = RevsetParseContext {
        aliases_map: aliases,
        local_variables: HashMap::new(),
        user_email: scope.user_email,
        date_pattern_context: chrono::Local::now().into(),
        default_ignored_remote: Some(RemoteName::new("git")),
        fileset_aliases_map: &fileset_aliases,
        extensions: &EXTENSIONS,
        workspace: Some(RevsetWorkspaceContext {
            path_converter: &converter,
            workspace_name: WorkspaceName::new(workspace),
        }),
    };
    let mut diagnostics = RevsetDiagnostics::new();
    let expression = jj_revset::parse(&mut diagnostics, text, &context)?;
    for warning in &diagnostics {
        tracing::debug!(error = %warning, "revset parse warning in {text:?}");
    }
    Ok(expression)
}

/// Parses `text` against `repo`'s view. `files()` works without a request
/// workspace; `@` does not.
///
/// Whether the expression uses `@` is learned by parsing it as two
/// workspaces no one can name: only `@` itself differs between the two
/// parses (jj-lib keeps its expression walkers private, so the parsed
/// expressions are compared by their `Debug` forms).
pub fn parse(
    repo: &ReadonlyRepo,
    text: &str,
    scope: &RevsetScope<'_>,
) -> Result<ParsedRevset, RevisionError> {
    let aliases = aliases(repo);
    let invalid = |error: RevsetParseError| RevisionError::Parse {
        revision: text.to_owned(),
        message: error.to_string(),
    };
    let probe = parse_as(text, &aliases, scope, PROBE_A).map_err(invalid)?;
    let other = parse_as(text, &aliases, scope, PROBE_B).map_err(invalid)?;
    let uses_working_copy = format!("{probe:?}") != format!("{other:?}");
    let expression = if uses_working_copy {
        let Some(workspace) = scope.workspace else {
            return Err(RevisionError::WorkspaceRequired {
                revision: text.to_owned(),
            });
        };
        parse_as(text, &aliases, scope, workspace).map_err(invalid)?
    } else {
        other
    };
    Ok(ParsedRevset {
        text: text.to_owned(),
        expression,
        uses_working_copy,
    })
}

fn resolution(revision: &str, error: RevsetResolutionError) -> RevisionError {
    let revision = revision.to_owned();
    match error {
        RevsetResolutionError::NoSuchRevision { name, candidates } => RevisionError::NotFound {
            revision,
            name,
            candidates,
        },
        RevsetResolutionError::WorkspaceMissingWorkingCopy { name } => {
            RevisionError::WorkingCopyNotFound {
                workspace: name.as_str().to_owned(),
                at_operation: String::new(),
            }
        }
        RevsetResolutionError::EmptyString => RevisionError::Parse {
            revision,
            message: "an empty string is not a valid revision".to_owned(),
        },
        RevsetResolutionError::AmbiguousCommitIdPrefix(prefix)
        | RevsetResolutionError::AmbiguousChangeIdPrefix(prefix) => {
            RevisionError::AmbiguousPrefix { revision, prefix }
        }
        RevsetResolutionError::DivergentChangeId {
            visible_targets, ..
        } => {
            let more = visible_targets.len() > MAX_LISTED;
            RevisionError::Ambiguous {
                revision,
                commit_ids: visible_targets
                    .iter()
                    .take(MAX_LISTED)
                    .map(|(_, id)| CommitIdHex::of(id))
                    .collect(),
                more,
            }
        }
        RevsetResolutionError::ConflictedRef { targets, .. } => RevisionError::Ambiguous {
            revision,
            more: targets.len() > MAX_LISTED,
            commit_ids: targets
                .iter()
                .take(MAX_LISTED)
                .map(CommitIdHex::of)
                .collect(),
        },
        RevsetResolutionError::Backend(error) => RevisionError::Backend(error),
        RevsetResolutionError::Other(error) => RevisionError::Other(error),
    }
}

/// Resolves and evaluates `parsed` against `repo`'s view.
pub(crate) fn evaluate<'a>(
    repo: &'a ReadonlyRepo,
    parsed: &ParsedRevset,
) -> Result<Box<dyn Revset + 'a>, RevisionError> {
    let symbols = SymbolResolver::new(repo, EXTENSIONS.symbol_resolvers());
    let resolved = parsed
        .expression
        .resolve_user_expression(repo, &symbols)
        .map_err(|error| match resolution(&parsed.text, error) {
            RevisionError::WorkingCopyNotFound { workspace, .. } => {
                RevisionError::WorkingCopyNotFound {
                    workspace,
                    at_operation: repo.op_id().hex(),
                }
            }
            error => error,
        })?;
    resolved.evaluate().map_err(RevisionError::Evaluation)
}

/// The one commit `parsed` names: none is [`RevisionError::Empty`], several
/// are [`RevisionError::Ambiguous`]. A commit named by id that is hidden at
/// the operation read resolves, as in jj.
pub async fn resolve_one(
    repo: &ReadonlyRepo,
    parsed: &ParsedRevset,
) -> Result<Resolved, RevisionError> {
    let revset = evaluate(repo, parsed)?;
    let ids: Vec<CommitId> = revset
        .stream()
        .take(MAX_LISTED + 1)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .map_err(RevisionError::Evaluation)?;
    match ids.as_slice() {
        [] => Err(RevisionError::Empty {
            revision: parsed.text.clone(),
        }),
        [id] => {
            let commit = repo.store().get_commit_async(id).await?;
            Ok(Resolved {
                commit_id: CommitIdHex::of(id),
                change_id: ChangeIdHex::of(commit.change_id()),
            })
        }
        several => Err(RevisionError::Ambiguous {
            revision: parsed.text.clone(),
            more: several.len() > MAX_LISTED,
            commit_ids: several
                .iter()
                .take(MAX_LISTED)
                .map(CommitIdHex::of)
                .collect(),
        }),
    }
}

/// How many commits `parsed` names. Walks the whole set.
pub fn count(repo: &ReadonlyRepo, parsed: &ParsedRevset) -> Result<usize, RevisionError> {
    let revset = evaluate(repo, parsed)?;
    let mut stream = revset.stream();
    let mut count = 0;
    while let Some(id) = pollster::block_on(stream.next()) {
        id.map_err(RevisionError::Evaluation)?;
        count += 1;
    }
    Ok(count)
}

/// [`parse`] then [`resolve_one`], with an empty `text`
/// [`RevisionError::Required`].
pub async fn resolve_revision(
    repo: &ReadonlyRepo,
    text: &str,
    scope: &RevsetScope<'_>,
) -> Result<Resolved, RevisionError> {
    if text.trim().is_empty() {
        return Err(RevisionError::Required);
    }
    let parsed = parse(repo, text, scope)?;
    resolve_one(repo, &parsed).await
}

impl super::repo::Repo {
    /// Resolves `text`, a revset naming one commit, in this repository's
    /// loaded view, with `@` this workspace's working-copy commit (R2.02).
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn resolve_revision(&self, text: &str) -> Result<Resolved, RevisionError> {
        let scope = RevsetScope {
            workspace: Some(self.workspace_name()),
            user_email: self.workspace.settings().user_email(),
            root: self.workspace_root(),
        };
        resolve_revision(&self.repo, text, &scope)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }
}
